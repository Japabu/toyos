//! The kernel driver for the scheduler core — spec §6.2, §6.3, §7.5.
//!
//! This file is the whitelist of §3: percpu plumbing, the asm switch, the idle
//! loop, the trampoline. It decides nothing. Every scheduling decision, state
//! transition and ordering-sensitive step happens above it, in `toyos-sched`,
//! where the simulator drives the same code.
//!
//! The shape of a scheduler entry is fixed and total:
//!
//! ```text
//! preempt::disable()
//! drain device IRQ records into wakes
//! with_cpu(|cpu| SchedPass::begin(cpu, env, now).dispose_*().finish())
//! match action { Run(tok) => switch(tok), Resume => {}, Idle(tok) => halt }
//! preempt::enable_no_resched()
//! ```
//!
//! Everything after the switch belongs to whichever task resumes on this
//! stack, and there is nothing scheduler-related left to do there — no guard to
//! release, no outgoing task to park. That is what park-before-switch buys, and
//! it is sound only because a wake for the just-parked task is a *message to
//! this same CPU*, which cannot be consumed before the switch completes.
//!
//! **What is *not* done by the time the pass ends is the save.** `switch`'s
//! last instruction writes the outgoing context's `rsp`, and everything above
//! it — the `with_cpu` return, `charge_cpu_time`, the publish, CR3, `TSS.rsp0`,
//! the FS base — runs with that context still holding the stack pointer from
//! the *previous* time it was switched away, or, for a task that never has
//! been, `alloc_kernel_stack`'s entry frame. Any CPU that restores it inside
//! that window lands on a stack this one is standing on.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::{asm, naked_asm};
use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use toyos_sched::cpu::{Action, Balance, CpuHandle, CpuHandles, CpuSched, Env, SchedPass};
use toyos_sched::fair::Frontier;
use toyos_sched::hw::{CpuId, Hw, Kicker, Machine, Nanos};
use toyos_sched::mailbox::{mailbox, Kick, PreemptGuard, Urgency};
use toyos_sched::msg::Msg;
use toyos_sched::task::{RtState, TaskBuilder, TaskKey, WaitClass};
use toyos_sched::waitq::{Cancel, Cancelled, Commit, CurrentTask};

use crate::arch::percpu;
use crate::hw::HW;
use crate::process::{OwnedAlloc, PageTables, TaskId, KERNEL_STACK_SIZE};

use super::payload::{
    KMsg, KShare, KShared, KWaitQueue, KernelCtx, KernelPayload, RawTicket, TaskHandle, ThreadSched,
};
use super::MAX_CPUS;

/// Proof that preemption is disabled for as long as the borrow lasts (spec
/// §7.2's N3). Constructible only by the two functions below, both of which
/// bracket it with the preempt count.
pub struct PreemptOff(());

// SAFETY: every constructor raises the kernel's preempt count first and lowers
// it only after the borrow ends, so the executing context cannot be
// descheduled while a value of this type is alive.
unsafe impl PreemptGuard for PreemptOff {}

/// Run `f` in a preempt-disabled region. Wake paths post mailbox messages from
/// here; a request raised inside is honoured on the way out, which is how an
/// RT wake reaches its own preemption.
pub fn preempt_off<R>(f: impl FnOnce(&PreemptOff) -> R) -> R {
    crate::preempt::disable();
    let result = f(&PreemptOff(()));
    crate::preempt::enable();
    result
}

/// The same proof, bought with `cli` instead of the preempt count.
///
/// **`log::emit` may pay neither of [`preempt_off`]'s two locked
/// read-modify-writes.** `preempt::disable` is `lock add` and `enable` is a
/// `lock sub` plus a `need_resched` poll that can reach `do_preempt`, which is
/// a scheduling pass — and one locked RMW per log line cost 350 ms of boot
/// (`issues/hardware/one-rmw-per-log-line-cost-350ms.md`). `IrqGuard` is
/// `pushfq`/`pop`/`cli` with `push`/`popfq` on drop: no locked operation at
/// all, and on the dominant path `IF` is already clear.
pub struct IrqOff(());

// SAFETY: **preemption in this kernel is delivered at an interrupt.**
// `do_preempt` has exactly three callers — the LAPIC timer
// (`arch/idt/timer.rs:101`), the exit-to-user epilogue (`arch/idt/mod.rs:370`,
// which `sti`s before it calls) and `preempt::enable`'s poll
// (`preempt.rs:126`). With `IF` masked the first two are unreachable, and the
// region `irq_off` brackets calls `wake_direct` and nothing else, so it reaches
// neither `preempt::enable` nor a voluntary pass. A voluntary pass is the one
// way to be descheduled with `IF` clear, which is why this type has no
// constructor but the bracket below. The scheduler core grants the same impl to
// an IRQ context in its own loom model and the trait's SAFETY paragraph names
// one.
unsafe impl PreemptGuard for IrqOff {}

/// Run `f` with interrupts masked, holding [`IrqOff`] for exactly that region.
pub fn irq_off<R>(f: impl FnOnce(&IrqOff) -> R) -> R {
    let _guard = crate::hw::IrqGuard::close();
    f(&IrqOff(()))
}

static CPUS: AtomicPtr<CpuHandles<KMsg>> = AtomicPtr::new(ptr::null_mut());
static FRONTIER: Frontier = Frontier::new();
static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// Per-CPU CPU-time counters, for `total_cpu_ns`. Cache-line padded.
#[repr(align(64))]
struct CpuTime(AtomicU64);
static CPU_TIME_NS: [CpuTime; MAX_CPUS] = [const { CpuTime(AtomicU64::new(0)) }; MAX_CPUS];

/// Nanoseconds between two pass-cost reports from one CPU.
///
/// The counters are cumulative since boot, so the *last* line a capture holds
/// is the whole run and the ones before it cost only their own record — which
/// makes this number a resolution rather than a sample size: the last report is
/// at most this long before the end.
///
/// **A wall-clock cadence and not a per-`n`-passes one, because the second is a
/// feedback loop.** A report is a log record, a record wakes `klogd`, and a
/// wake is a pass — so "every N passes" makes the report rate drive the pass
/// rate it is reporting on. Measured on the dev host, 2026-08-17: at one report
/// per 64 passes cpu0 finished a boot plus `sched_stress` at 1,408 passes, and
/// at one per 256 it never reached 256 at all. This clock is the guest's own
/// and nothing the reports do moves it.
#[cfg(feature = "sched-check")]
const PASS_COST_REPORT_EVERY_NS: u64 = 200_000_000;

/// When each CPU last reported, in nanoseconds since boot. Read and written by
/// the owning CPU alone.
#[cfg(feature = "sched-check")]
static PASS_COST_REPORTED: [CpuTime; MAX_CPUS] = [const { CpuTime(AtomicU64::new(0)) }; MAX_CPUS];

/// Publish this CPU's pass-cost distribution, at most once every
/// [`PASS_COST_REPORT_EVERY_NS`].
///
/// **Outside the pass and inside the preempt-off region**, which is the only
/// window that works: the measurement lands after `finish_inner` has consumed
/// every borrow of the `CpuSched`, and `log::emit` may take no lock and does
/// not — it fills a stack record and publishes it under one trap-state bracket
/// (`log/mod.rs`), which is why it is already called from IRQ handlers and from
/// inside the scheduler.
///
/// `now` is the pass's own sample rather than a fresh clock read: this is a
/// cadence and not a measurement, and one read per pass is what the check build
/// already pays.
///
/// A check build only. On the pass path between two reports this costs one
/// relaxed load and one comparison, and everything about it — the histogram,
/// the clock read that feeds it, the record — exists only where `sched-check`
/// does.
#[cfg(feature = "sched-check")]
fn report_pass_costs(now: Nanos) {
    let cpu = current_cpu();
    let last = &PASS_COST_REPORTED[cpu.0 as usize].0;
    if now.0 < last.load(Ordering::Relaxed) + PASS_COST_REPORT_EVERY_NS {
        return;
    }
    last.store(now.0, Ordering::Relaxed);
    crate::log!("{}", cpus().get(cpu).pass_costs().report(cpu));
}

/// How often the heap sweep walks every live band, in guest nanoseconds.
///
/// **Guest time and not passes**, because a pass is not a unit of anything: a
/// loaded CPU takes thousands in the spawn burst this class dies in and an idle
/// one takes a handful. 25 ms puts roughly a dozen sweeps inside a boot that
/// reaches `compositor: ready` at ~600 ms, with at least one after the burst —
/// which is where a band that was written has to be found, since the writer is
/// long gone by then and the allocation it wrote past is never freed.
#[cfg(feature = "heap-sweep")]
const SWEEP_EVERY_NS: u64 = 25_000_000;

#[cfg(feature = "heap-sweep")]
static NEXT_SWEEP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Take the sweep if this CPU is the one that claims the slot.
///
/// **Outside `with_cpu`, deliberately and not incidentally.** The sweep takes
/// `dlmalloc`'s lock; taking any lock inside the driver's exclusive region is
/// what wedged four of twelve guests when the first heap tripwire logged from
/// in there, because the log's readiness path re-enters `driver::pass`. Here
/// there is no pass in progress and no `&mut CpuSched` alive, so a lock — and
/// the panic a dirty band raises — is an ordinary one.
///
/// The claim is a compare-exchange rather than a store, so twelve CPUs coming
/// through the same nanosecond run one sweep between them and not twelve.
#[cfg(feature = "heap-sweep")]
fn maybe_sweep(now: Nanos) {
    let due = NEXT_SWEEP.load(Ordering::Relaxed);
    if now.0 < due {
        return;
    }
    if NEXT_SWEEP
        .compare_exchange(due, now.0 + SWEEP_EVERY_NS, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    crate::mm::sweep_heap_bands("pass");
}

/// How long a [`maybe_hold`] visit spends on the pass path, and how often.
///
/// **The number is a floor on the sweep's own hold and not a match for it.**
/// `heap-sweep` walks every 2 MiB page the heap owns under `dlmalloc`'s lock on
/// the same 25 ms cadence; the storms that measured it lost about 2% of their
/// boots to it, which over a ~3 s boot and a handful of sweeps is milliseconds
/// per walk. 1 ms every 25 ms is 4% of the pass path spent the way the sweep
/// spends it, chosen so an arm that amplifies says so without a duty cycle that
/// stops being a boot storm. An arm that does *not* amplify at 1 ms has bounded
/// the effect rather than refuted it, and the next arm is a longer hold.
#[cfg(feature = "pass-spin")]
const HOLD_NS: u64 = 1_000_000;
#[cfg(feature = "pass-spin")]
const HOLD_EVERY_NS: u64 = 25_000_000;
#[cfg(feature = "pass-spin")]
static NEXT_HOLD: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Spend [`HOLD_NS`] on the pass path, and — under `heap-lockspin` — spend it
/// holding `dlmalloc`'s lock.
///
/// **The control the amplifier has never had.** Two instruments now multiply
/// this class and neither writes anything: `sched-tripwire`'s byte shadow (7.2x)
/// and `heap-sweep`'s walk (absent to baseline). They share a shape — time spent
/// on the path every pass takes — and `heap-sweep` adds a second thing, the
/// allocator's lock. Nothing has separated the two, and the separation is one
/// cargo feature: `pass-spin` spends the time and takes no lock, `heap-lockspin`
/// spends the same time under the same lock the sweep takes. Two arms at one
/// `HOLD_NS` say whether the window this class needs is the *lock* or merely the
/// *delay*, and the answer decides where a fix can be looked for at all.
///
/// It reads nothing and writes nothing but its own claim, so — like the sweep —
/// it compiles no decision and cannot itself be corrupting anything.
///
/// Outside `with_cpu` for [`maybe_sweep`]'s reason: this takes a lock, and the
/// driver's exclusive region is where a lock wedged four of twelve guests.
#[cfg(feature = "pass-spin")]
fn maybe_hold(now: Nanos) {
    let due = NEXT_HOLD.load(Ordering::Relaxed);
    if now.0 < due {
        return;
    }
    if NEXT_HOLD
        .compare_exchange(due, now.0 + HOLD_EVERY_NS, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    #[cfg(feature = "heap-lockspin")]
    crate::mm::hold_heap_lock(HOLD_NS);
    #[cfg(not(feature = "heap-lockspin"))]
    spin_for(HOLD_NS);
}

/// Burn `ns` of guest time and nothing else.
#[cfg(feature = "pass-spin")]
pub(crate) fn spin_for(ns: u64) {
    let until = crate::clock::nanos_since_boot() + ns;
    while crate::clock::nanos_since_boot() < until {
        core::hint::spin_loop();
    }
}

pub fn cpus() -> &'static CpuHandles<KMsg> {
    let ptr = CPUS.load(Ordering::Acquire);
    assert!(!ptr.is_null(), "scheduler used before sched::init");
    // SAFETY: set once by `init` from a leaked Box, never cleared.
    unsafe { &*ptr }
}

pub fn frontier() -> &'static Frontier {
    &FRONTIER
}

/// Monotonic and never reused, so a message about a dead task is provably
/// stale rather than ambiguously about its successor (spec §5.1). Deliberately
/// not `TaskId`: pids and tids are recycled.
fn next_key() -> TaskKey {
    TaskKey(NEXT_KEY.fetch_add(1, Ordering::Relaxed))
}

pub fn total_cpu_ns() -> u64 {
    (0..crate::arch::smp::cpu_count() as usize)
        .map(|i| CPU_TIME_NS[i].0.load(Ordering::Relaxed))
        .sum()
}

struct SchedSlot(UnsafeCell<Option<CpuSched<KernelPayload>>>);

// SAFETY: the cell is only ever reached through `with_cpu`, which indexes by
// the *calling* CPU's own id and refuses reentry. `CpuSched` itself is `!Sync`,
// so nothing it contains can escape into another CPU by any other route.
unsafe impl Sync for SchedSlot {}

static SCHEDS: [SchedSlot; MAX_CPUS] = [const { SchedSlot(UnsafeCell::new(None)) }; MAX_CPUS];
static IN_PASS: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Is this CPU inside a pass? A nested pass is a bug, not something to defer —
/// but the preempt poll can legitimately ask, and the panic path must know
/// before it tries to rejoin.
pub fn in_pass() -> bool {
    IN_PASS[percpu::cpu_id() as usize].load(Ordering::Relaxed)
}

/// The only accessor. Panics on reentry: the busy flag is the typed
/// replacement for `IN_SCHEDULE`, and a nested pass would alias `&mut`.
///
/// **It is also the whole of the window the tripwire watches.** Nothing else in
/// the kernel writes `SCHEDS`: `init` fills it before any AP is released and
/// [`try_with_cpu`] only reads, so between one exit from here and the next entry
/// the record is a thing no code may change. `sched-tripwire` holds it to that.
fn with_cpu<R>(f: impl FnOnce(&mut CpuSched<KernelPayload>) -> R) -> R {
    let cpu = percpu::cpu_id() as usize;
    assert!(
        !IN_PASS[cpu].swap(true, Ordering::Acquire),
        "nested scheduler pass on cpu {cpu}",
    );
    // SAFETY: exclusive by the flag above, and by CpuId — no other CPU indexes
    // this slot.
    let sched = unsafe { (*SCHEDS[cpu].0.get()).as_mut() }
        .unwrap_or_else(|| panic!("cpu {cpu} has no CpuSched"));
    #[cfg(feature = "sched-tripwire")]
    tripwire::verify(cpu, sched);
    let result = f(&mut *sched);
    #[cfg(feature = "sched-tripwire")]
    tripwire::record(cpu, sched);
    IN_PASS[cpu].store(false, Ordering::Release);
    result
}

/// The stray-write tripwire's storage and its two halves.
///
/// **What it is for.** Four kernel deaths are on record — 2026-08-19 and
/// 2026-08-20, all under a loaded boot — whose entire content is a per-CPU
/// scheduler record reading as a value no operation on it produces: a `BTreeMap`
/// walked with `root == None` and `length != 0`, a `BTreeMap` node whose `len`
/// overran its own key storage, and twice this file's own
/// `cpu {n} has no CpuSched` on a CPU that had already completed a pass. Each is
/// a *word that changed*, and the reports say only that one did. This says which
/// word, in which field, from what to what — and prints
/// [`crate::hw::report_contexts`] beside it, which is the other half of the one
/// mechanism anyone has written down for the class.
///
/// **The shadow is per CPU and touched by that CPU alone**, inside `with_cpu`'s
/// exclusive region, which is why an `UnsafeCell` is enough and no lock is
/// wanted: this runs on the path a pass takes.
#[cfg(feature = "sched-tripwire")]
mod tripwire {
    use super::{CpuSched, KernelPayload, MAX_CPUS};
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicBool, Ordering};

    /// Words of shadow per CPU, checked against the record it shadows below.
    const WORDS: usize = 96;
    const _: () = assert!(
        core::mem::size_of::<CpuSched<KernelPayload>>().div_ceil(8) <= WORDS,
        "the CpuSched outgrew its shadow: raise WORDS",
    );

    struct Shadow {
        words: UnsafeCell<[u64; WORDS]>,
        taken: AtomicBool,
    }

    // SAFETY: each element is read and written by the CPU whose index it is and
    // by no other, inside `with_cpu`'s exclusive region.
    unsafe impl Sync for Shadow {}

    static SHADOW: [Shadow; MAX_CPUS] = [const {
        Shadow {
            words: UnsafeCell::new([0; WORDS]),
            taken: AtomicBool::new(false),
        }
    }; MAX_CPUS];

    /// The record's bytes as little-endian words, with the words covering the
    /// one remotely-written field read back as zero.
    ///
    /// Byte reads rather than word reads, so a record whose size is not a
    /// multiple of eight is not read past its own end. Volatile, because what
    /// this wants is what is in the memory and not what the abstract machine
    /// says should be.
    fn snapshot(sched: &CpuSched<KernelPayload>, out: &mut [u64; WORDS]) {
        let base = (sched as *const CpuSched<KernelPayload>).cast::<u8>();
        let size = core::mem::size_of::<CpuSched<KernelPayload>>();
        let (lo, hi) = CpuSched::<KernelPayload>::tripwire_remote_range();
        for (i, word) in out.iter_mut().enumerate().take(size.div_ceil(8)) {
            let off = i * 8;
            if off < hi && off + 8 > lo {
                *word = 0;
                continue;
            }
            let mut bytes = [0u8; 8];
            for (k, byte) in bytes.iter_mut().enumerate() {
                if off + k >= size {
                    break;
                }
                // SAFETY: `off + k < size`, so this addresses a byte of the very
                // record `sched` borrows.
                *byte = unsafe { core::ptr::read_volatile(base.add(off + k)) };
            }
            *word = u64::from_le_bytes(bytes);
        }
    }

    /// Leaving the exclusive region: this is what the record is expected to
    /// still look like when it is next entered.
    pub fn record(cpu: usize, sched: &CpuSched<KernelPayload>) {
        // Walked at *both* ends, which is what makes a red say when. A container
        // that walks straight here and crookedly at the next entry was broken
        // while no pass held the record; one that walks straight at entry and
        // crookedly here was broken by the pass in between. Neither statement is
        // available from one end alone.
        walk(cpu, sched);
        // SAFETY: this CPU's own slot, inside the exclusive region.
        let slot = unsafe { &mut *SHADOW[cpu].words.get() };
        snapshot(sched, slot);
        SHADOW[cpu].taken.store(true, Ordering::Relaxed);
    }

    /// Entering it: anything that differs was written by something with no
    /// business writing it.
    pub fn verify(cpu: usize, sched: &CpuSched<KernelPayload>) {
        if !SHADOW[cpu].taken.load(Ordering::Relaxed) {
            return;
        }
        let mut now = [0u64; WORDS];
        snapshot(sched, &mut now);
        // SAFETY: as `record`.
        let was = unsafe { &*SHADOW[cpu].words.get() };
        let words = CpuSched::<KernelPayload>::tripwire_words();
        let mut hit = false;
        for i in 0..words {
            if was[i] == now[i] {
                continue;
            }
            if !hit {
                crate::log!("A STRAY WRITE REACHED cpu{cpu}'s CpuSched while no pass held it:");
                hit = true;
            }
            crate::log!(
                "  +{:#05x} {} was {:#018x}, is {:#018x}",
                i * 8,
                CpuSched::<KernelPayload>::tripwire_field(i * 8),
                was[i],
                now[i],
            );
        }
        if hit {
            let here = 0u64;
            crate::hw::report_contexts(core::ptr::addr_of!(here) as u64, None);
            panic!("cpu {cpu}: a stray write reached its CpuSched");
        }
        walk(cpu, sched);
    }

    /// The shadow covers this record's own bytes and says nothing about the heap
    /// its three containers hang off — and the deaths this exists for are a
    /// `BTreeMap` **node** as often as a `BTreeMap` header. So the containers are
    /// walked here too.
    ///
    /// **What a red here buys is the moment.** A walk that panics or disagrees at
    /// the *entry* to a pass proves the container was already broken before that
    /// pass ran a single statement, which is what separates "this pass did it"
    /// from "something wrote it while no pass held the record" — and combined
    /// with a clean byte diff one line above, it says the write landed in the
    /// heap rather than in the record.
    /// **Every element is touched, and `count()` alone would not have been a
    /// walk.** `RunQueue::tasks` is a `Chain` of two `ExactSizeIterator`s, so a
    /// bare `.count()` is free for the optimiser to fold into the two lengths —
    /// which is precisely the number the assertion would then be comparing it
    /// against. Reading each task's key forces the traversal and the deref of
    /// the record behind it, which is where a broken node is met.
    fn walk(cpu: usize, sched: &CpuSched<KernelPayload>) {
        let mut walked = 0usize;
        let mut fingerprint = 0u64;
        for task in sched.rq().tasks() {
            walked += 1;
            fingerprint ^= task.key().0;
        }
        assert_eq!(
            walked,
            sched.rq().len(),
            "cpu {cpu}: the ready band walks {walked} tasks and calls itself {} long",
            sched.rq().len(),
        );
        // Walked for the walk's sake: a corrupt node fails inside the iterator,
        // which is the report wanted, and neither of these publishes a second
        // length to disagree with.
        for parked in sched.parked() {
            fingerprint ^= parked.key().0;
        }
        for dying in sched.dying() {
            fingerprint ^= dying.key().0;
        }
        core::hint::black_box(fingerprint);
    }
}

/// A read-only peek for diagnostics that must not fail while a pass runs.
fn try_with_cpu<R>(f: impl FnOnce(&CpuSched<KernelPayload>) -> R) -> Option<R> {
    let cpu = percpu::cpu_id() as usize;
    if IN_PASS[cpu].load(Ordering::Relaxed) {
        return None;
    }
    // SAFETY: as `with_cpu`, and shared rather than exclusive.
    let sched = unsafe { (*SCHEDS[cpu].0.get()).as_ref() }?;
    Some(f(sched))
}

/// Build every CPU's mailbox and handle, and the BSP's `CpuSched`. Called once,
/// before any task exists.
pub fn init() {
    let count = crate::arch::smp::cpu_count() as usize;
    assert!(count <= MAX_CPUS, "cpu count {count} exceeds MAX_CPUS");
    let mut handles = Vec::with_capacity(count);
    // A CPU number rather than a walk of `SCHEDS`: it becomes the `CpuId` each
    // mailbox and handle is built for, and `SCHEDS` is `MAX_CPUS` long whatever
    // `count` is.
    #[allow(clippy::needless_range_loop)]
    for cpu in 0..count {
        let (tx, rx) = mailbox::<KMsg>();
        handles.push(CpuHandle::new(CpuId(cpu as u32), tx));
        // SAFETY: single-threaded boot; the APs have not joined yet.
        unsafe {
            *SCHEDS[cpu].0.get() = Some(CpuSched::new(CpuId(cpu as u32), rx, idle_ctx()));
        }
    }
    CPUS.store(
        Box::into_raw(Box::new(CpuHandles::new(handles))),
        Ordering::Release,
    );
}

/// The context a CPU runs on when it has nothing to do. Having one is what lets
/// a pass free the previous zombie — an idle CPU never stands on a dead task's
/// stack.
fn idle_ctx() -> KernelCtx {
    KernelCtx {
        rsp: 0,
        cr3: crate::mm::paging::kernel_cr3(),
        fs_base: 0,
        kernel_stack_top: 0,
        id: None,
        // Never read: a CPU can only switch *to* its idle context from a task,
        // and reaching a task means it switched away from idle first, which
        // wrote the real depth. The idle loop enters by jump, not by switch.
        preempt: 0,
    }
}

/// Least-loaded CPU by published ready count (spec §9.4), scanning from a
/// rotating start so that ties spread instead of piling on one CPU.
///
/// The rotation is load-bearing at boot and only there: `publish_load` runs at
/// the end of a pass, and the init programs are all spawned before any CPU has
/// run one, so every published load is still zero and a fixed scan order would
/// put the whole system on CPU 0. Balance would pull them apart eventually,
/// but "eventually" is measured in idle passes and boot has none to spare.
fn placement() -> CpuId {
    static ROTATE: AtomicU64 = AtomicU64::new(0);
    let count = crate::arch::smp::cpu_count();
    let start = (ROTATE.fetch_add(1, Ordering::Relaxed) % count as u64) as u32;
    let mut best = CpuId(start);
    let mut best_load = cpus().get(best).load();
    for offset in 1..count {
        let cpu = CpuId((start + offset) % count);
        let load = cpus().get(cpu).load();
        if load < best_load {
            best_load = load;
            best = cpu;
        }
    }
    best
}

/// Everything a new thread needs. `entry_rsp` points at the trampoline frame
/// `alloc_kernel_stack` built.
///
/// **`address_space` is not an `Option`, and the history is worth one
/// sentence.** "The scheduler cannot host a kernel task" was a single `.expect`
/// here until `klogd` arrived; that became a two-armed `match` naming the
/// kernel's `cr3` for a task that named no address space, and the second arm is
/// gone because every kernel thread now names `mm::paging::kernel` itself. One
/// declaration decides a task's `cr3`, which is the rule the root `CLAUDE.md`
/// states for control registers and is the same rule here.
pub struct NewTask {
    pub id: TaskId,
    pub kernel_stack: OwnedAlloc,
    pub entry_rsp: u64,
    pub address_space: PageTables,
    pub fs_base: u64,
    pub share: Arc<KShare>,
}

/// Place a new task by message — never by reaching into the destination's
/// queue (spec §9.4). Returns what the process table keeps.
pub fn spawn(new: NewTask) -> ThreadSched {
    // A kernel thread's is the kernel address space — the one every CPU is
    // already in between two user threads, which is why `idle_ctx` above names
    // the same `cr3`. Nothing is released when the task ends: that `Arc` is a
    // clone of a leaked one, and the kernel's page tables outlive every task by
    // construction.
    let cr3 = new.address_space.lock().cr3();
    let kernel_stack_top = new.kernel_stack.ptr() as u64 + KERNEL_STACK_SIZE as u64;
    let ctx = KernelCtx {
        rsp: new.entry_rsp,
        cr3,
        fs_base: new.fs_base,
        kernel_stack_top,
        id: Some(new.id),
        // The one level `trampoline_entry` discharges before the first `iretq`.
        preempt: 1,
    };
    let handle = Arc::new(TaskHandle::new());
    let task = TaskBuilder {
        key: next_key(),
        share: new.share,
        ctx,
        ext: KernelPayload {
            id: new.id,
            kernel_stack: new.kernel_stack,
            address_space: new.address_space,
            handle: handle.clone(),
        },
        rt: RtState::default(),
    }
    .build(placement(), HW.now());
    let sched = ThreadSched {
        handle,
        shared: task.shared().clone(),
    };
    let dst = match task.shared().state() {
        toyos_sched::task::TaskState::InTransit(cpu) => cpu,
        state => panic!("a freshly built task is not in transit: {state:?}"),
    };
    preempt_off(|p| {
        if cpus()
            .get(dst)
            .post_owned(Msg::Adopt { task }, Msg::adopt_node, Urgency::Normal, p)
            == Kick::Send
        {
            HW.kick(dst);
        }
    });
    sched
}

pub enum Dispose {
    /// An IRQ-exit poll: the pass decides for itself whether the running task
    /// keeps the CPU.
    None,
    Yield,
    Exit,
}

/// The environment every pass runs against.
///
/// `balance` is the one policy value in it, and it is [`Balance::Pull`]: an idle
/// pass probes the busiest CPU for work and a loaded pass answers probes from
/// surplus (spec §7.7, §9.4's pull half). Without it a task woken onto a busy
/// CPU waits there until the owner yields.
///
/// The other settings of that knob are the simulator's, and deliberately not
/// this line's: [`Balance::PullWithRearm`] and [`Balance::PushOnSurplus`] both
/// add wakes to the idle path, which `kernel/CLAUDE.md` makes an audio change,
/// and what they buy and cost is measured in `toyos-sched/sim/tests/policy.rs`
/// rather than decided here.
///
/// The guard comes in by reference because its lifetime is the pass's and it
/// belongs to the caller that raised the count.
fn env(preempt: &PreemptOff) -> Env<'_, crate::hw::KernelHw, PreemptOff> {
    Env {
        hw: &HW,
        cpus: cpus(),
        frontier: &FRONTIER,
        preempt,
        balance: Balance::Pull,
    }
}

/// Run one scheduler pass and execute its action.
///
/// The preempt count is raised here and lowered by whichever context comes back
/// on this stack: this one after the switch returns, or a fresh task's
/// trampoline. It balances per context, not per call — which is why the count
/// travels *with* the context across the switch (`Hw::switch`) instead of being
/// inherited by whoever lands on the CPU next.
pub fn pass(dispose: Dispose) {
    // The witness's own negative control (`df-witness-mutate`): set the flag one
    // instruction before the reader that must refuse it. Nothing runs in between,
    // so the machine never executes a string operation with it set — the panic is
    // the next statement.
    #[cfg(feature = "df-witness-mutate")]
    // SAFETY: a build that exists to stage the defect, and the reader below
    // panics before any `rep movs` can run.
    unsafe {
        core::arch::asm!("std", options(nomem, nostack))
    };
    #[cfg(feature = "df-witness")]
    crate::arch::cpu::df_witness("a scheduler pass");
    crate::preempt::disable();
    // A pass *is* the reschedule the request asks for, so it owns the clear —
    // and it must clear before it drains, so a request raised by this pass's
    // own wakes survives into the next poll. Without this the idle loop never
    // sleeps: a kick IPI to a halted CPU is taken in Ring 0, which sets
    // `need_resched` and nothing else, and the pre-halt recheck then finds the
    // request still standing on every iteration.
    crate::preempt::clear_need_resched();
    drain_irqs();
    // The object layer's second drain site. After `drain_irqs` and before the
    // pass picks, so a wake a zero-handle hook posts is in the run queue by the
    // time this pass chooses — the same placement, and the same reason, as the
    // irq drain above it.
    crate::object::drain_zero_handles();
    let now = HW.now();
    #[cfg(feature = "heap-sweep")]
    maybe_sweep(now);
    #[cfg(feature = "pass-spin")]
    maybe_hold(now);
    let action = with_cpu(|cpu| {
        let pass = SchedPass::begin(cpu, env(&PreemptOff(())), now);
        if let Some(current) = pass.cpu().running() {
            check_stack_canary(current.ext());
            current.ext().handle.publish(current.acct(), None);
        }
        let disposed = match dispose {
            Dispose::None => pass.dispose_none(),
            Dispose::Yield => pass.dispose_yield(),
            Dispose::Exit => pass.dispose_exit(),
        };
        disposed.finish()
    });
    charge_cpu_time(now);
    with_cpu(|cpu| {
        if let Some(current) = cpu.running() {
            current.ext().handle.publish(current.acct(), Some(now));
        }
    });
    // The one report site, and `pass_block` is deliberately not a second one:
    // every CPU with anything to run takes a timer tick through here, and a CPU
    // with nothing to run is idling through here too.
    #[cfg(feature = "sched-check")]
    report_pass_costs(now);
    execute(action);
    crate::preempt::enable_no_resched();
}

/// A wait registration, holding preemption off for the whole window between
/// phase 1 and phase 2 of the §8.1 handshake.
///
/// The window is not preemptible, and the guard is what makes that true rather
/// than hoped for. `prepare_wait` publishes `Committing(cpu, gen)` and the
/// machine has no edge out of it except the commit or the cancel: `preempt`
/// asserts on `Running`, and *inventing* a `Committing → Ready` edge would be
/// worse than the assert, because a waker that pops the registration and finds
/// the word `Ready` reports `Claim::Lost` and moves on to the next waiter —
/// the registered task is then off the queue, unwoken, and about to park. That
/// is a lost wake, which is the one thing this protocol exists to remove.
///
/// This is *not* §8.1's residual commit-to-park window, which has to be
/// tolerated because a remote CPU can act between two of our own instructions.
/// Nothing remote is involved here: the only route into a pass mid-window is
/// this CPU's own `preempt::enable` slow path, reached from the guard drop of
/// any lock the re-check takes. A window whose only intruder is ourselves can
/// be closed, so it is.
///
/// The guard is owned rather than remembered: the two ways to consume a ticket
/// both discharge it, so "registered with preemption on" has no expression.
#[must_use = "a wait ticket must be blocked on or cancelled"]
pub struct Ticket<'q>(RawTicket<'q>);

impl<'q> Ticket<'q> {
    /// Phase 1: register the running thread on `queue`.
    ///
    /// The count goes up before the current task is even read: without it, a
    /// preemption between reading the task and registering it would leave
    /// `CurrentTask` naming a CPU the thread no longer runs on, and
    /// `begin_commit` asserts on exactly that.
    pub fn register(queue: &'q KWaitQueue, cancel: Cancel, class: WaitClass) -> Self {
        crate::preempt::disable();
        let shared = current_shared().expect("prepare_wait: no running thread");
        let current = CurrentTask::new(&shared, current_cpu());
        // **The class is the wait's and not the queue's**, because the queue is
        // this thread's own parking place and has no subject —
        // `WaitQueue::prepare_wait_as` carries the argument, and the blocked-time
        // breakdown in `ProcessStats` is what it buys.
        Self(queue.prepare_wait_as(&current, cancel, class))
    }

    /// The condition became true after registering: withdraw, and take the
    /// deferred preemption now that the thread is plainly `Running` again.
    pub fn cancel(self) -> Cancelled {
        let outcome = self.0.cancel();
        crate::preempt::enable();
        outcome
    }

    /// Hand the registration to the blocking pass. The count stays raised —
    /// see [`pass_block`].
    fn into_raw(self) -> RawTicket<'q> {
        self.0
    }
}

/// The blocking pass: commit the wait ticket **inside** the pass, after the
/// mailbox drain, and park on the same pass (spec §8.1's phase 2).
///
/// The commit cannot happen at the call site. A remote waker that claims a
/// task whose word already reads `Blocked` posts `Msg::Wake` to the task's home
/// CPU — which is this one — and the pass's own drain would consume that
/// message before the task is in `parked`, where `handle_wake` would find
/// nothing and drop it. Committing after the drain puts the claim on one side
/// or the other of it: an earlier claim finds `Committing` and posts nothing, so
/// the commit itself observes it and refuses to park; a later claim's message
/// arrives behind the drain and is handled by the next pass, which finds the
/// task parked.
///
/// **Returns on every path, and one of them changed.** A retire that catches a
/// thread mid-registration used to turn the block into an exit and never come
/// back; since the cancellable kill the `Commit::Killed` arm below is
/// `dispose_none` — the thread keeps its stack, unwinds it, and takes the
/// cancel from its next `completion::wait`. There is no disposition here that
/// does not return.
pub fn pass_block(ticket: Ticket<'_>, deadline: Option<Nanos>) {
    // No `preempt::disable()` of its own: the ticket has held the count raised
    // since the registration published `Committing`, and that guard *is* this
    // pass's bracket. The window and the pass are one continuous preempt-off
    // region, which is the truth; taking a second level here would leave one
    // for the resuming context to discharge and one for nobody.
    let ticket = ticket.into_raw();
    crate::preempt::clear_need_resched();
    drain_irqs();
    let now = HW.now();
    let (action, registration) = with_cpu(|cpu| {
        let pass = SchedPass::begin(cpu, env(&PreemptOff(())), now);
        if let Some(current) = pass.cpu().running() {
            check_stack_canary(current.ext());
            current.ext().handle.publish(current.acct(), None);
        }
        match ticket.commit() {
            Commit::Parked(committed, registration) => (
                pass.dispose_block(committed, deadline).finish(),
                Some(registration),
            ),
            // A wake landed between registration and commit: do not park, do
            // not switch (spec §8.1). The pass still runs to its disposition,
            // because the quantum may have expired while we were deciding.
            Commit::AlreadyWoken => (pass.dispose_none().finish(), None),
            // A retire landed while this thread was deciding to park. **The
            // thread keeps running and unwinds** — it does not exit here, and
            // that is §7.2: this kernel does not unwind, so a switch that
            // never returns abandons every guard on this stack. The
            // registration is already withdrawn by `commit`, the word is back
            // at `Running`, and the caller's next `completion::wait` reports
            // the cancel that sends it home.
            Commit::Killed => (pass.dispose_none().finish(), None),
        }
    });
    charge_cpu_time(now);
    with_cpu(|cpu| {
        if let Some(current) = cpu.running() {
            current.ext().handle.publish(current.acct(), Some(now));
        }
    });
    execute(action);
    crate::preempt::enable_no_resched();
    if let Some(registration) = registration {
        // Whatever ended the park, the node must leave the queue before this
        // thread can register anywhere else — otherwise a later `wake_one` on
        // the old queue would be satisfied by a waiter that is not waiting.
        registration.finish();
    }
}

/// Per-CPU busy time, for `sysinfo`. Derived from the same `now` the pass used,
/// so it cannot disagree with the task's own charge.
fn charge_cpu_time(now: Nanos) {
    let cpu = percpu::cpu_id() as usize;
    static LAST: [CpuTime; MAX_CPUS] = [const { CpuTime(AtomicU64::new(0)) }; MAX_CPUS];
    let last = LAST[cpu].0.swap(now.0, Ordering::Relaxed);
    if last != 0 && percpu::current_tid().is_some() {
        CPU_TIME_NS[cpu]
            .0
            .fetch_add(now.0.saturating_sub(last), Ordering::Relaxed);
    }
}

fn execute(action: Action<KernelPayload>) {
    match action {
        // SAFETY: the token came from `finish`, which built it from live
        // Box-backed task records; those records outlive the switch because the
        // only way to free one is `Hw::release`, which runs in a later pass.
        Action::Run(token) => unsafe { HW.switch(token) },
        Action::Resume => {}
        Action::Idle(token) => {
            // The final look, with interrupts off. A message that landed after
            // the pass's own check raised the doorbell, and its producer saw
            // SLEEPING and sent the IPI; taking that IPI here as an ordinary
            // interrupt and then halting is exactly B4, so re-check first.
            //
            // Not `Machine::irq_guard`: both exits must *set* IF — the halt
            // because `sti;hlt` is one atom, the stay-awake exit because panic
            // recovery reaches the idle loop with IF already 0. A guard would
            // restore, and restoring 0 on the stay-awake exit is a CPU that
            // leaves the pass deaf.
            crate::arch::cpu::disable_interrupts();
            let cpu = CpuId(percpu::cpu_id());
            let awake = cpus().get(cpu).doorbell().kick_pending()
                || crate::preempt::need_resched()
                || crate::irq_ring::any_pending_self()
                || !with_cpu(|c| c.mailbox_is_empty())
                // A CPU with nothing left to run is the moment the i8042's
                // "the pin has never asserted" verdict stops being premature:
                // before it, silence only says the boot is still busy. A
                // wall-clock deadline cannot serve, because the driver is only
                // reached from inside a pass and the machine the verdict exists
                // for reaches `Boot: complete` and then has nothing to do — so
                // no pass would run to notice the deadline and the line would
                // never appear at all. Self-clearing on the same argument as the
                // ring above: the next pass emits the line and moves the state
                // on, so this costs one trip round the loop and never a spin.
                || crate::drivers::i8042::verdict_due()
                // **No log condition survives L6, and its absence is the
                // point.** Two used to be here. `log_ring::has_pending` went at
                // L3, when the commit started posting `klogd`'s wake and the
                // halt became refusable by the doorbell like any other runnable
                // task's; `log_file_flush_due` goes here, with the kernel file
                // sink it asked about. What writes `/log` now is a userland
                // process made runnable through the mailbox, so a CPU with a
                // log to write is a CPU with something in its run queue and the
                // three conditions above already refuse the halt for it.
                // `idle_loop_is_the_declared_body` is what keeps a fourth from
                // being quietly re-added.
                //
                // A root-hub port whose connect state the driver has not
                // finished acting on. The connect edge that started it was the
                // last interrupt that controller has to give — a device sitting
                // still in a port produces nothing further — so no wake is
                // coming and the one-shot timer is armed for parked *tasks*,
                // which a driver's deferred work is not. Bounded and
                // self-clearing like the three above, but over a longer
                // interval: USB 2.0 §7.1.7.3's 100 ms of debounce, or the
                // transfer deadline behind a port that will not reset. It costs
                // an idle CPU the halt, never a pass — anything runnable is
                // still picked, because this decides only whether to sleep.
                || crate::drivers::xhci::port_work_pending();
            if awake {
                crate::arch::cpu::enable_interrupts();
                drop(token);
                return;
            }
            HW.idle_wait(token);
        }
    }
}

/// Consume this CPU's `irq_ring` records (spec §11 stage 2) and turn them into
/// wakes. Runs at the top of every pass, before the mailbox drain, so a wake
/// posted here is in the run queue by the time the pass picks.
fn drain_irqs() {
    // First in the function, so the stamp means "this CPU reached a pass" and
    // not "this CPU got all the way through one".
    #[cfg(feature = "boot-actuators")]
    crate::heartbeat::note_pass();
    // xHCI (keyboard/mouse): the controller poll dispatches HID reports, which
    // wake the keyboard/mouse queues from inside the driver.
    crate::drivers::xhci::poll_if_pending();
    // The i8042's bytes are already in kernel memory when the IRQ returns;
    // this turns them into events and wakes.
    crate::drivers::i8042::service();
    // Ctrl+Alt+D. Here rather than at the keystroke, which is decoded under
    // whichever driver's guard produced it: this walks the scheduler and logs
    // a line per parked thread, and both drivers are done above.
    if crate::keyboard::take_dump_request() {
        super::dump::request();
    }
    // A CPU cannot read a sibling's `CpuSched`, so the dump reaches every CPU
    // by asking, and this is where each one answers.
    super::dump::serve_if_owed();
    // And this is where the report it painted goes back on the panel if whoever
    // owns the screen has drawn over it. One clock read per pass while nothing
    // is held, and nothing at all once the hold expires.
    crate::drivers::panic_console::hold_report();

    if crate::irq_ring::take(crate::irq_ring::IrqSource::Net).is_some() {
        crate::net::wake_waiters();
        let watchers = crate::net::inbox_watchers();
        if !watchers.is_empty() {
            crate::inbox::complete_pending_for_event(
                &watchers,
                crate::inbox::Source::Network,
            );
        }
    }
    if crate::irq_ring::take(crate::irq_ring::IrqSource::Audio).is_some() {
        // One wait queue for both backends: an over-wake costs a recheck, and a
        // second queue would have to be chosen by whichever driver bound —
        // which is a fact the parking side does not have.
        crate::sched::waitqs::wake_device(&crate::sched::waitqs::AUDIO_WATCH);
        for (watchers, source) in [
            (
                crate::drivers::virtio_sound::inbox_watchers(),
                crate::inbox::Source::VirtioSound,
            ),
            (crate::drivers::hda::inbox_watchers(), crate::inbox::Source::Hda),
        ] {
            if !watchers.is_empty() {
                crate::inbox::complete_pending_for_event(&watchers, source);
            }
        }
    }
}

/// Leave the current stack for this CPU's idle stack and never come back.
/// Boot and AP bring-up enter the scheduler here.
pub fn enter_idle_loop() -> ! {
    percpu::set_current_tid(None);
    percpu::set_current_pid(None);
    // SAFETY: irreducible — `set_kernel_stack` reaches its `PerCpu` through
    // `gs:[0]`, so its `# Safety` is that the caller is the CPU that GS base
    // belongs to. It is: this runs on the CPU it is putting into the idle loop,
    // after `percpu::init_bsp`/`init_ap` gave that CPU its GS base, and the
    // argument comes from `idle_stack_top`, which reads the same `PerCpu`. What
    // it writes is the two words every Ring 3 → Ring 0 entry takes its stack
    // from, and the value is the stack the `mov rsp` below is about to stand
    // on — nothing safe can express "this is the stack I am about to be on".
    unsafe { percpu::set_kernel_stack(percpu::idle_stack_top()) };
    // SAFETY: irreducible — `activate` writes CR3, whose `# Safety` is that the
    // tables are live. `kernel_cr3` is the one address space this kernel builds
    // at boot and never frees, and it is the space this function's own code and
    // stack are mapped in, so the write cannot unmap what executes it. The idle
    // context carries no user address space, which is the whole reason this is
    // the space to be in.
    unsafe { crate::mm::paging::kernel_cr3().activate() };
    let sp = percpu::idle_stack_top();
    // SAFETY: irreducible — a stack pointer cannot be moved from Rust, and this
    // is the one function that has to. It is sound because nothing on the
    // outgoing stack is live past it: `enter_idle_loop` returns `!` and its
    // caller is boot or AP bring-up, whose frames nothing ever unwinds; `sp` is
    // this CPU's own idle stack top, just installed above; and `options
    // (noreturn)` is the truth, because `jmp` is the last instruction and
    // `idle_loop` is `-> !`.
    unsafe {
        asm!(
            "mov rsp, {sp}",
            // Terminate the frame chain, and leave the zero return-address
            // slot a `call` would have left. `idle_loop` is entered by `jmp`,
            // so its frame is the topmost on this stack and `rbp + 8` — where
            // `kernel_backtrace` reads the return address — was the unmapped
            // page above a 16 KiB idle stack. A fatal panic taken on an idle
            // CPU therefore faulted inside `crash_report` while printing its
            // own backtrace; that fault's report faulted the same way, and it
            // ended in a double fault with the panel carrying seven pages of
            // cascade and not one line of the reason. The one context a
            // machine-stopped panic is raised from was the one context that
            // could not say why.
            //
            // `push` also leaves `rsp` where the ABI expects it at a function
            // entry, which jumping to the raw top does not.
            "xor ebp, ebp",
            "push rbp",
            "jmp {func}",
            sp = in(reg) sp,
            func = in(reg) idle_loop as *const () as usize,
            options(noreturn),
        );
    }
}

extern "C" fn idle_loop() -> ! {
    loop {
        // The idle loop and not a pass: the state it stages is a CPU that never
        // reaches one.
        #[cfg(feature = "boot-actuators")]
        if crate::actuator::dump_deaf_cpu() {
            super::dump::deaf_window();
        }
        // The idle loop for the same reason, from the other side: the CPU that
        // storms is the one with nothing to run, and the CPU under observation
        // is whichever one is spinning on `syscall` from Ring 3.
        #[cfg(feature = "boot-actuators")]
        if crate::actuator::syscall_window_nmi() {
            crate::nmi_gate::storm();
        }
        // Here and not from a syscall: the panic handler recovers rather than
        // paints when a userland thread is current, and this context has none.
        #[cfg(feature = "boot-actuators")]
        if crate::drivers::panic_console::probe_due() {
            panic!("metal-panic-probe: a fatal report over a desktop that owns the screen");
        }
        crate::scheduler::log_health();
        crate::scheduler::reap_poisoned();
        // `pass` below covers this too; it is here so a CPU that reaches the
        // loop and then halts has run every hook first, rather than leaving one
        // queued behind an interrupt that may be 102 s away.
        crate::object::drain_zero_handles();
        // A heartbeat is a record like any other now, so it reaches the wire on
        // the commit rather than waiting for a sink that used to run in the
        // statement below it. What that statement was — the log file's flush —
        // is gone with the file (§8.1), and the idle loop no longer touches a
        // filesystem, a volume or a controller at all.
        #[cfg(feature = "boot-actuators")]
        crate::heartbeat::poll();
        pass(Dispose::None);
    }
}

/// The running task's rendezvous word, cloned so the caller can hold it across
/// its own block without borrowing the `CpuSched`.
pub fn current_shared() -> Option<Arc<KShared>> {
    try_with_cpu(|cpu| cpu.running().map(|t| t.shared().clone())).flatten()
}

/// The running task's cross-CPU face, which is where its completion inbox
/// lives. `None` on a CPU with no task: boot, and the idle loop.
pub fn current_handle() -> Option<Arc<crate::sched::payload::TaskHandle>> {
    try_with_cpu(|cpu| cpu.running().map(|t| t.ext().handle.clone())).flatten()
}

/// The same face, borrowed rather than cloned, for a reader that does not
/// outlive the peek. `None` on a CPU with no task, exactly as above.
///
/// It exists because [`current_handle`]'s `Arc::clone` is two uncontended
/// read-modify-writes, and `scheduler::Operation::established` asks this
/// question on every park token minted in the machine — the hot-path atomic
/// `tests/CLAUDE.md` names as the one TCG prices unlike hardware.
pub fn with_current_handle<R>(f: impl FnOnce(&crate::sched::payload::TaskHandle) -> R) -> Option<R> {
    try_with_cpu(|cpu| cpu.running().map(|t| f(t.ext().handle.as_ref())))?
}

/// Whether the running task has been killed — one relaxed load, no clone.
///
/// Read on every return to Ring 3, which is why it takes no `Arc`: a refcount
/// on that path is the read-modify-write §16.2 prices at hundreds of
/// microseconds under TCG.
pub fn current_kill_pending() -> bool {
    try_with_cpu(|cpu| cpu.running().is_some_and(|t| t.shared().kill_pending())).unwrap_or(false)
}

pub fn current_cpu() -> CpuId {
    CpuId(percpu::cpu_id())
}

/// The address space the running task runs in.
///
/// **`None` means "no task is running", never "this task has no address
/// space"** — the second reading stopped existing when the payload's field did
/// (`KernelPayload::address_space`). Boot and an idle CPU are the two answers.
pub fn current_address_space() -> Option<PageTables> {
    try_with_cpu(|cpu| cpu.running().map(|t| t.ext().address_space.clone())).flatten()
}

pub fn with_current_acct<R>(
    f: impl FnOnce(&toyos_sched::task::TaskAccounting) -> R,
) -> Option<R> {
    try_with_cpu(|cpu| cpu.running().map(|t| f(t.acct()))).flatten()
}

pub fn set_current_rt(permanent: bool) {
    with_cpu(|cpu| cpu.set_current_rt(permanent));
}

pub fn boost_current(until: Nanos) {
    with_cpu(|cpu| cpu.boost_current(until));
}

pub fn current_is_rt() -> bool {
    try_with_cpu(|cpu| cpu.running().is_some_and(|t| t.rt().is_rt())).unwrap_or(false)
}

pub fn ready_len() -> usize {
    try_with_cpu(|cpu| cpu.ready_len()).unwrap_or(0)
}

pub fn parked_len() -> usize {
    try_with_cpu(|cpu| cpu.parked().count()).unwrap_or(0)
}

/// Killed threads on this CPU that are unwinding or waiting to.
///
/// **The dump's fourth container, and it had no caller at all.**
/// `CpuSched::dying_len`'s own doc says it exists "for a dump that has to say
/// where every task is", and until this one nothing asked: a dying task's state
/// word reads `Ready`, so the process-table census counted it, the CPU half
/// could not see it, and `unheld = claimed − scheduled` reported a task nothing
/// would ever run — on a healthy machine, for up to a quantum, on every thread
/// teardown. That verdict is the whole reason the dump exists.
pub fn dying_len() -> usize {
    try_with_cpu(|cpu| cpu.dying_len()).unwrap_or(0)
}

/// Every dying thread on this CPU, in the order the pick will take them.
pub fn for_each_dying(mut f: impl FnMut(TaskId)) -> bool {
    try_with_cpu(|cpu| {
        for task in cpu.dying() {
            f(task.ext().id);
        }
    })
    .is_some()
}

/// The thread this CPU has loaded, if any.
pub fn running_id() -> Option<TaskId> {
    try_with_cpu(|cpu| cpu.running().map(|t| t.ext().id)).flatten()
}

/// One parked task, flattened for a reader outside the scheduler.
///
/// Flattened here because a `ParkedView` borrows the `CpuSched`, and nothing
/// outside this file may hold that borrow — that a CPU's state is reachable
/// only from that CPU is the property the whole core is built on.
pub struct ParkedInfo {
    pub id: TaskId,
    pub class: toyos_sched::task::WaitClass,
    pub deadline: Option<u64>,
    /// When the park began.
    pub since: u64,
    pub rt: bool,
}

/// Walk this CPU's parked tasks. `false` means a pass owns the state right
/// now, and a diagnostic does not wait for one.
pub fn for_each_parked(mut f: impl FnMut(ParkedInfo)) -> bool {
    try_with_cpu(|cpu| {
        for parked in cpu.parked() {
            f(ParkedInfo {
                id: parked.ext().id,
                class: parked.class(),
                deadline: parked.deadline().map(|n| n.0),
                since: parked.since().0,
                rt: parked.is_rt(),
            });
        }
    })
    .is_some()
}

/// Tail of the first switch into a fresh task, called by
/// `process_start`/`thread_start` before the first `iretq`.
///
/// There is no lock to release and no outgoing task to park — the pass that
/// switched here ended before the switch. All that is owed is the other half of
/// that pass's preempt-count bracket, which this context now inherits.
pub extern "sysv64" fn trampoline_entry() {
    crate::preempt::enable_no_resched();
    crate::arch::idt::kernel_exit_to_user_check();
}

const STACK_CANARY: u64 = 0xDEAD_BEEF_CAFE_BABE;

/// What every untouched word of a task's kernel stack holds under
/// `heap-tripwire`.
///
/// The same byte `arch::percpu` fills the idle and IST1 stacks with, for the
/// same reason: a zero is a value a stack legitimately writes, so a zeroed
/// stack cannot tell untouched from written and has no depth to report.
#[cfg(feature = "heap-tripwire")]
const STACK_FILL: u8 = 0xA5;
#[cfg(feature = "heap-tripwire")]
const STACK_FILL_WORD: u64 = u64::from_ne_bytes([STACK_FILL; 8]);

/// The layout `alloc_kernel_stack` asks for, named once so the tripwire reads
/// back the bands that request was given.
#[cfg(feature = "heap-tripwire")]
fn stack_layout() -> core::alloc::Layout {
    core::alloc::Layout::from_size_align(KERNEL_STACK_SIZE, 4096)
        .expect("the kernel stack layout")
}

/// Rungs of the depth ladder, in bytes above the bottom of the stack.
///
/// Descending, so the depths they report *ascend*: a word at `bottom + rung`
/// that is no longer [`STACK_FILL_WORD`] says the stack reached within `rung`
/// of its own bottom, which is `KERNEL_STACK_SIZE - rung` bytes used. Touched
/// is monotone up the stack, so the first rung still holding fill ends the
/// walk and the one before it is the high water.
///
/// **Read rather than walked.** The exact depth is a `partition_point` over
/// 16,384 words, and this runs on every pass — which `kernel/CLAUDE.md` says is
/// an audio change. Nine volatile reads bound the high water to a band, which
/// is what the question needs: a stack that never leaves the top 16 KiB is not
/// what is writing the heap, and one that reaches the bottom 4 KiB is.
#[cfg(feature = "heap-tripwire")]
const DEPTH_RUNGS: [usize; 9] = [
    124 * 1024, 120 * 1024, 112 * 1024, 96 * 1024, 64 * 1024,
    32 * 1024, 16 * 1024, 8 * 1024, 4 * 1024,
];

/// The deepest any task kernel stack has been, in bytes used.
///
/// **An atomic that is never logged from where it is written.** The first draft
/// of this logged the high water the moment it rose, from inside
/// `check_stack_canary` — which runs inside `with_cpu`'s exclusive region — and
/// **four of the first twelve-wide storm arm's guests wedged**, every one of
/// them in the spawn burst with the previously-logged rung as its last line,
/// and every one recorded as a hang rather than a panic. `sched-tripwire`'s
/// three arms had no hang between them. `crate::log!` is not a leaf: the log's
/// own readiness path reaches `driver::pass` — `post_readiness+0x98` above
/// `pass+0x94` is in one of the captures this class is built from — so a log
/// emitted from inside a pass can re-enter the pass. `sched-tripwire`'s own
/// `log!` gets away with it only because a `panic!` follows it and it never has
/// to return. The number is read by [`stack_high_water`] from the crash report
/// instead, which is the channel a storm reads anyway.
#[cfg(feature = "heap-tripwire")]
static DEEPEST: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The deepest any task kernel stack has been and the stack it is measured
/// against, or `None` from a kernel that carries no ladder.
///
/// For `hw::report_contexts`, which runs on every kernel crash. A `Some(0)` is
/// a reading and not an absence: it says no task stack ever reached the
/// shallowest rung, which is 4 KiB used.
#[cfg(feature = "heap-tripwire")]
pub fn stack_high_water() -> Option<(usize, usize)> {
    Some((DEEPEST.load(Ordering::Relaxed), KERNEL_STACK_SIZE))
}

/// See the `heap-tripwire` arm above. A kernel without the ladder has no
/// reading to report, which is not the same as a reading of zero.
#[cfg(not(feature = "heap-tripwire"))]
pub fn stack_high_water() -> Option<(usize, usize)> {
    None
}

pub fn write_stack_canary(stack: &OwnedAlloc) {
    // Painted before the canary word, so the canary survives the fill rather
    // than the other way round.
    //
    // SAFETY: `stack` is a fresh `OwnedAlloc::new(KERNEL_STACK_SIZE, 4096)`
    // that nothing else has seen yet, so this writes exactly its own bytes.
    #[cfg(feature = "heap-tripwire")]
    unsafe { core::ptr::write_bytes(stack.ptr(), STACK_FILL, KERNEL_STACK_SIZE) };
    // SAFETY: the same fresh allocation, whose length is `KERNEL_STACK_SIZE`
    // and whose alignment is 4096, so eight bytes at offset zero are its own
    // and aligned. **Irreducible where it is cheap to be reducible**: the
    // bounded form is `OwnedAlloc::slice(8).write::<u64>(0, …)`, which is an
    // `unsafe fn` too — `KernelSlice`'s accessors all are — so it would trade
    // this block for that one and add an assert. What decides it is the
    // *reader* below rather than this writer: the two have to have the same
    // shape, and the reader runs inside `with_cpu` on every pass.
    unsafe { *(stack.ptr() as *mut u64) = STACK_CANARY };
}

fn check_stack_canary(payload: &KernelPayload) {
    // SAFETY: `payload.kernel_stack` is the `OwnedAlloc` `write_stack_canary`
    // painted at spawn, alive for as long as the task is — the payload owns it
    // and `Hw::release` is the only drop — so the word this reads is the one
    // that was written, or the overflow this exists to name.
    //
    // **Irreducible here for a reason that is not about this expression.** It
    // runs inside `with_cpu`'s exclusive region on *every* pass, which is the
    // kernel's hottest path and one the idle loop is on; `KernelSlice` would
    // bound the read and would still be an `unsafe fn` call, so the trade is a
    // per-pass `assert!` bought for no block removed.
    let canary = unsafe { *(payload.kernel_stack.ptr() as *const u64) };
    if canary != STACK_CANARY {
        panic!(
            "KERNEL STACK OVERFLOW: tid={} canary={:#x} expected={:#x}",
            payload.id.1, canary, STACK_CANARY
        );
    }
    #[cfg(feature = "stack-witness")]
    check_stack_ownership(payload);
    #[cfg(feature = "heap-tripwire")]
    stack_depth(payload);
}

/// Does every Ring 3 → Ring 0 entry this CPU can take land on the stack of the
/// task this CPU is actually running, and is this CPU standing on it?
///
/// **The question a corrupted word cannot answer and this can.** A stray-write
/// class chased across 2026-08-19..21 kept producing *kernel text* words inside
/// kernel data — a deterministic `0xffff8000_7cae_3310` over `MscDevice::block`,
/// a stack's own `stack_top + 16` one word above its top — and a mid-function
/// text address in a data field is a **return address**, which means something
/// executed with that address as its stack pointer. There are exactly two words
/// in this
/// machine that aim an execution at a stack it did not grow: `kernel_rsp`, which
/// `syscall` loads, and `tss.rsp0`, which every interrupt from Ring 3 loads.
/// `Hw::switch` writes both from the incoming context's `kernel_stack_top`, so
/// either a switch left them naming the wrong task or that field is itself a
/// victim — and both cases are one comparison away, *here*, before the entry
/// that would use them happens, instead of a boot later at whatever the write
/// landed on.
///
/// The third comparison is the converse: this CPU is executing a pass, so its
/// own `rsp` must be inside the stack of the task the pass says is running. It
/// costs a register read and catches the case where the entry stacks are right
/// and the *execution* is on the wrong one.
///
/// Two loads, a register read and three compares per pass. It is a reader and
/// decides nothing, which is why a kernel carrying it schedules exactly as one
/// without it — and why the arm it belongs to is still not the arm a rate was
/// measured on (`heap-sweep`'s note).
///
/// **It has never fired, and the writer it was built for was something else.**
/// 18,064 boots across 2026-08-21's arms and 7,059 more the day the class was
/// closed: `kernel_rsp` and `tss.rsp0` always agreed with the running task's own
/// stack top, and no pass ever ran on a foreign `rsp`. The text words in data
/// fields came from a `memcpy` running *backwards* — `arch::entry`'s `cld` — and
/// not from an execution standing on the wrong stack. Kept because it answers a
/// question nothing else can, and because a negative this wide is worth having.
#[cfg(feature = "stack-witness")]
fn check_stack_ownership(payload: &KernelPayload) {
    let bottom = payload.kernel_stack.ptr() as u64;
    let top = bottom + KERNEL_STACK_SIZE as u64;
    // SAFETY: a pass runs on the CPU whose GS base is its own `PerCpu`.
    let (kernel_rsp, rsp0) = unsafe { percpu::entry_stacks() };
    let rsp = crate::arch::cpu::read_rsp();
    if kernel_rsp == top && rsp0 == top && rsp <= top && rsp > bottom {
        return;
    }
    panic!(
        "STACK WITNESS: cpu{} is passing on tid={} whose stack is \
         [{bottom:#018x}, {top:#018x}) — kernel_rsp={kernel_rsp:#018x} \
         (off by {}), tss.rsp0={rsp0:#018x} (off by {}), rsp={rsp:#018x} \
         ({} bytes below the top). A Ring 3 entry takes its stack from one of \
         those two words, so one that is not this task's top aims the next \
         entry's return addresses into memory another execution owns.",
        percpu::cpu_id(),
        payload.id.1,
        kernel_rsp.wrapping_sub(top) as i64,
        rsp0.wrapping_sub(top) as i64,
        top.wrapping_sub(rsp) as i64,
    );
}

/// The heap bands around this task's kernel stack, and how deep it has been.
///
/// **Both ends, which is what makes a red say when.** The canary above is one
/// word at the bottom of the usable stack and it is read at every pass already;
/// what it cannot see is a frame that stepped *over* it — a `memcpy` at a
/// computed offset, a red zone — and what nothing here could see before is a
/// stack that ran off its bottom into the neighbouring chunk. Under
/// `heap-tripwire` there is no neighbouring chunk to reach: 4096 bytes of head
/// band stand between, and the four words of it that sit immediately below the
/// lowest usable stack word — the first thing an overflow writes — are read
/// here, with the tail band's four for company.
#[cfg(feature = "heap-tripwire")]
fn stack_depth(payload: &KernelPayload) {
    let bottom = payload.kernel_stack.ptr();
    // SAFETY: the running task's own stack, allocated by `alloc_kernel_stack`
    // with exactly `stack_layout()`. It is running on this CPU, so nothing is
    // freeing it.
    unsafe { crate::mm::check_heap_bands(bottom, stack_layout(), "kernel stack") };
    let mut used = 0;
    for rung in DEPTH_RUNGS {
        // SAFETY: `rung < KERNEL_STACK_SIZE`, so this reads a word of that
        // same stack.
        let word = unsafe { core::ptr::read_volatile(bottom.add(rung).cast::<u64>()) };
        if word == STACK_FILL_WORD {
            break;
        }
        used = KERNEL_STACK_SIZE - rung;
    }
    // Recorded and not logged. See [`DEEPEST`].
    DEEPEST.fetch_max(used, Ordering::Relaxed);
}

/// The outgoing half of [`context_switch`]: seven words onto the stack this CPU
/// is leaving, the address of them into the outgoing context, and the incoming
/// context's saved `rsp` into the register file.
///
/// A macro rather than nine lines, because [`context_switch`] exists in two
/// builds and the instruction sequence must be **one** text: an instrument that
/// changed the switch it instruments would measure itself.
macro_rules! switch_save {
    () => {
        "pushfq
         push rbp
         push rbx
         push r12
         push r13
         push r14
         push r15
         mov [rdi], rsp
         mov rsp, rsi"
    };
}

/// The incoming half: the seven words this class keeps being killed by, and the
/// `ret` that is the last instruction able to say anything at all.
macro_rules! switch_restore {
    () => {
        "pop r15
         pop r14
         pop r13
         pop r12
         pop rbx
         pop rbp
         popfq
         ret"
    };
}

/// Callee-saved register save/restore. Unchanged from the old scheduler — the
/// switch was never the part that was wrong.
#[cfg(not(feature = "switch-witness"))]
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn context_switch(old_rsp: *mut u64, new_rsp: u64) {
    naked_asm!(switch_save!(), switch_restore!());
}

/// The same switch with [`crate::hw::switch_witness_verify`] between the stack
/// pointer moving and the first `pop`.
///
/// **The window between the check and the pop, and it measured to zero.**
/// `hw::check_switch_frame` reads `ctx.rsp`, validates it and the return slot,
/// and returns; the frame is popped a few hundred instructions later — past the
/// preempt swap, the TSS handover, a `mov cr3` and a `wrfsbase` — and nothing in
/// between tested it. In 6,901 twelve-wide storm boots on 2026-08-21, against 20
/// deaths of the class in those same boots, this fired **not once**: no word of a
/// frame ever changed in the window and `ctx.rsp` never moved under it. So
/// neither "another CPU wrote the frame after the check" nor "this CPU's own
/// interrupt path spilled onto it" is what was killing the machine, and the
/// writer was found one layer out — `arch::entry`'s `cld`.
///
/// The call is placed after `mov rsp, rsi` and not before it deliberately: the
/// subject is the incoming frame read *through the register the machine will
/// actually use*, so what it reports is where the machine is standing and not
/// what a field says. That is what lets it tell the two apart now that
/// `hw::check_switch_frame` returns the word it validated — the stack pointer
/// is that word, and a `ctx.rsp` written after the check moves the field and
/// nothing else.
///
/// It is sound to `call` here. The return address lands eight bytes below the
/// frame, which is inside the incoming task's own kernel stack — `switch-witness`
/// turns on `stack-witness` for exactly that reason, whose third test refuses a
/// `ctx.rsp` that is not inside the stack its own `kernel_stack_top` names. The
/// callee is an ordinary `extern "C"` function, so the six registers about to be
/// popped are preserved by the ABI and every register it may clobber is dead
/// here: `rdi` was consumed by `mov [rdi], rsp`, `rsi` by `mov rsp, rsi`, and a
/// resumed context's caller-saved registers are dead across the `call
/// context_switch` it returns to (`loader::start`'s three trampolines take their
/// arguments in `r12`/`r13`/`r14`, which are callee-saved).
#[cfg(feature = "switch-witness")]
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn context_switch(old_rsp: *mut u64, new_rsp: u64) {
    naked_asm!(
        switch_save!(),
        "mov rdi, rsp",
        "call {verify}",
        switch_restore!(),
        verify = sym crate::hw::switch_witness_verify,
    );
}

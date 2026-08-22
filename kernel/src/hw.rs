//! `KernelHw` — the kernel's side of the scheduler-core hardware boundary
//! (spec §10.1).
//!
//! Everything here is x2APIC, TSC or a single instruction. Nothing here
//! decides anything: no queue is consulted, no state machine advances, no
//! ordering-sensitive protocol lives below this line. That is the whole
//! contract — the simulator replaces this file and nothing else.
//!
//! **[`Hw::switch`] loads the incoming context's `rsp` exactly once.**
//! [`check_switch_frame`] returns the word it validated and that returned value
//! is what reaches `context_switch`; reading `ctx.rsp` a second time would emit a
//! second load, because the `mov cr3` between the two is an `asm!` with a memory
//! clobber that LLVM may not forward across — and the machine would then stand on
//! a word the guard never saw.

use core::arch::asm;

use toyos_sched::cpu::SleepToken;
use toyos_sched::cpu::RunToken;
use toyos_sched::hw::{CpuId, Hw, Kicker, Machine, Nanos, TraceEvent};
use toyos_sched::task::{TaskAccounting, TaskKey};

use crate::arch::{apic, cpu, percpu};
use crate::sched::driver::context_switch;
use crate::sched::payload::{KernelCtx, KernelPayload};

/// The one instance. Zero-sized: every effect is on a model-specific register
/// of the CPU that calls it, or a targeted ICR write addressed by argument —
/// there is no per-machine state a second value could hold.
pub static HW: KernelHw = KernelHw;

pub struct KernelHw;

/// The scheduler's clock reads, as raw nanoseconds — where the kernel's `u64`
/// timestamps meet the core's [`Nanos`].
pub fn now_ns() -> u64 {
    HW.now().0
}

/// RAII interrupt gate. Restores the caller's `IF` rather than setting it
/// unconditionally, so nesting inside an already-closed region is safe.
#[must_use = "the interrupt gate closes when the guard drops"]
pub struct IrqGuard {
    rflags: u64,
}

impl IrqGuard {
    pub fn close() -> Self {
        let rflags: u64;
        // SAFETY: three instructions that read and write nothing but `RFLAGS`
        // and one stack slot they push and immediately pop — no `nostack`, so
        // the compiler keeps `%rsp` valid and leaves no red zone for the push
        // to land in. `cli` cannot fail in Ring 0, and closing interrupts is
        // always sound: what it costs is latency, and the `Drop` below is what
        // bounds that.
        //
        // Irreducible, and it is why `arch::cpu::disable_interrupts` is not
        // enough on its own: the *saving* of the caller's `IF` and the `cli`
        // have to be one uninterruptible sequence, or an interrupt between the
        // `pushfq` and the `cli` records a flag word this CPU no longer has.
        // Two safe calls cannot express that; one `asm!` block can.
        unsafe {
            asm!("pushfq", "pop {}", "cli", out(reg) rflags, options(nomem));
        }
        Self { rflags }
    }
}

impl Drop for IrqGuard {
    fn drop(&mut self) {
        // SAFETY: `rflags` is the word this guard's own `close` read out of
        // `RFLAGS` on this CPU, so `popfq` restores a flag word the machine
        // produced rather than one anything computed — including the caller's
        // `IF`, which is the whole point (a nested guard restores "closed").
        // Same stack-slot argument as `close`.
        //
        // Irreducible for `close`'s reason, in reverse: restoring a saved flag
        // word is not `sti`, and `arch::cpu` offers no unconditional-restore
        // primitive because there is nothing safe to build one out of.
        unsafe {
            asm!("push {}", "popfq", in(reg) self.rflags, options(nomem));
        }
    }
}

impl Kicker for KernelHw {
    fn kick(&self, target: CpuId) {
        apic::kick_cpu(target.0);
    }
}

impl Machine for KernelHw {
    type IrqGuard = IrqGuard;

    fn now(&self) -> Nanos {
        Nanos(crate::clock::nanos_since_boot())
    }

    /// The trait's deadline is absolute; the LAPIC one-shot's initial-count
    /// register is relative, so this samples the clock a second time to
    /// subtract. That second sample is the cost of the mismatch, and it is
    /// the mismatch TSC-deadline mode removes — `IA32_TSC_DEADLINE` takes an
    /// absolute value, so the conversion here becomes ns→TSC scaling with no
    /// clock read at all.
    ///
    /// A deadline already in the past arms the one-shot's floor and fires at
    /// the end of it. Not sooner: "as soon as possible" is an interrupt the
    /// CPU takes before it can retire the instruction that armed it, and the
    /// Ring 0 stub then reloads the same count forever.
    fn set_timer(&self, deadline: Nanos) {
        apic::arm_one_shot(deadline.0.saturating_sub(self.now().0));
    }

    fn stop_timer(&self) {
        apic::stop_timer();
    }

    fn irq_guard(&self) -> IrqGuard {
        IrqGuard::close()
    }

    fn halt(&self) {
        // SAFETY: two instructions that touch no memory and no stack. `sti`
        // and `hlt` are Ring 0 instructions this kernel always runs in, and
        // `hlt` is only ever a wait — the scheduler core calls this having
        // decided this CPU has nothing to run.
        //
        // Irreducible, and it is not `arch::cpu::halt`: that one is `cli; hlt`
        // in a loop, a CPU that never comes back. This is the opposite pair,
        // and the *order* is the whole of it — `sti` unmasks one instruction
        // boundary before `hlt`, so a wake that arrives in between is taken
        // rather than slept through. Two safe calls would put a sequence point
        // where the architecture guarantees there is none.
        unsafe { asm!("sti; hlt", options(nomem, nostack)); }
    }

    /// A remote CPU's `need_resched` byte is not writable from here: `PerCpu`
    /// is reachable only through this CPU's `GS` base, and there is no
    /// registry of sibling `PerCpu` pointers. The kick IPI is the way to say
    /// it — the timer vector's Ring 0 stub sets `need_resched` on arrival and
    /// its Ring 3 path runs the preempt check directly, so a kick *is* a
    /// remote resched request, with an interrupt as the delivery mechanism.
    fn need_resched(&self, cpu: CpuId) {
        if cpu.0 == percpu::cpu_id() {
            crate::preempt::set_need_resched();
        } else {
            self.kick(cpu);
        }
    }

    fn trace(&self, ev: TraceEvent) {
        crate::trace::record(ev);
    }

    /// A diagnostic build refuses full quiescence. That is the whole of
    /// `diag-tick`, and the whole difference between the two builds.
    ///
    /// The default is to sleep until something arrives, which is correct for a
    /// shipping kernel and is what the owner's T14 does: eight boots halted
    /// every CPU at 1.8 s and took no interrupt for as long as 102 s. Everything
    /// the kernel says to whoever is watching it is emitted from the idle loop,
    /// so across that window it said nothing, and the boots that survived wrote
    /// the same file as the boots that froze.
    ///
    /// Arming before the halt and not after the wake: `halt` is `sti; hlt` and
    /// its STI shadow, so a fire that lands in the window between them is taken
    /// rather than slept through. Ordering with the pass's own arming is
    /// [`apic::arm_within`]'s minimum, so this only ever adds wakes.
    fn idle_wait(&self, token: SleepToken) {
        let _consumed = token;
        #[cfg(feature = "boot-actuators")]
        if crate::actuator::diag_tick() {
            apic::arm_within(DIAG_TICK_NS);
        }
        self.halt();
    }
}

/// The longest a CPU may sleep on a `diag-tick` build.
///
/// Comfortably under `heartbeat`'s reporting period rather than equal to it, so
/// a healthy CPU contributes two or three passes to every line. At one wake per
/// line a CPU whose wake landed just the wrong side of the boundary would drop
/// out of the mask, and a field that flickers on a healthy machine cannot be
/// read as "that CPU stopped" on a sick one.
#[cfg(feature = "boot-actuators")]
const DIAG_TICK_NS: u64 = 100_000_000;

/// Which context each CPU last switched onto.
///
/// One relaxed store per switch, and the whole of what it buys is the question
/// [`report_contexts`] answers: **is a sibling standing on this same context —
/// or on this same stack — right now.** That is the difference between a report
/// and a diagnosis, and nothing else the crash path can reach answers it,
/// because a `CpuSched` is `!Sync` and a sibling's is unreadable by
/// construction.
static RUNNING_CTX: [core::sync::atomic::AtomicU64; crate::sched::MAX_CPUS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; crate::sched::MAX_CPUS];

/// Which CPU is standing on which context, printed on **every** kernel crash.
///
/// **The question this answers is the one the whole `BTreeMap`-inside-its-own-
/// insert class turns on, and until now only one crash in the kernel could ask
/// it.** A per-CPU scheduler container reading as a value no sequence of
/// operations on it produces says "something wrote this record"; it does not say
/// *what*, and the one mechanism anyone has written down for it — two CPUs
/// executing on one kernel stack — is decided by exactly two facts: whether two
/// CPUs name one `KernelCtx`, and whether the crashing stack pointer lies inside
/// a stack that belongs to some other CPU's task. Both are here, and neither
/// needs a register dump, so a *Rust panic* can now settle what previously only
/// a `context_switch` fault could hint at.
///
/// `rsp` is the crashing frame's stack pointer — the exception frame's for a
/// fault, the address of a local for a panic; the containment test only needs it
/// to be somewhere in the stack the crash is running on.
///
/// `subject` is the context the flag is asked about: the *incoming* one at
/// [`switch_frame_is_wrong`], which is the pointer #149's diagnosis found two
/// CPUs naming, and this CPU's own everywhere else. `None` means the latter.
///
/// **It reads a sibling's `KernelCtx` and that is deliberate.** The pointers came
/// from this kernel's own switch path and address boxed records in the direct
/// map, which stays mapped whether or not the record has since been freed; the
/// guard is `is_kernel_addr` plus alignment, the same one `check_switch_frame`
/// takes before it reads a frame. Nothing here allocates, locks or formats
/// anything but integers.
///
/// A CPU on its **idle** context contributes no stack range: `idle_ctx`'s
/// `kernel_stack_top` is zero by construction (it is per-CPU and not knowable at
/// the boot-time init that builds the context), so the containment test simply
/// does not fire for one. That is a gap in this report and not a claim.
pub fn report_contexts(rsp: u64, subject: Option<u64>) {
    let me = percpu::cpu_id() as usize;
    let count = (crate::arch::smp::cpu_count() as usize).min(crate::sched::MAX_CPUS);
    let mine = RUNNING_CTX
        .get(me)
        .map_or(0, |slot| slot.load(core::sync::atomic::Ordering::Relaxed));
    let subject = subject.unwrap_or(mine);
    crate::log!("  Contexts: cpu{me} crashed at rsp={rsp:#018x}, asking about ctx {subject:#x}");
    for (cpu, slot) in RUNNING_CTX.iter().enumerate().take(count) {
        let held = slot.load(core::sync::atomic::Ordering::Relaxed);
        if !crate::mm::is_kernel_addr(held) || !held.is_multiple_of(8) {
            crate::log!("  cpu{cpu} is on ctx {held:#x} (never switched, or not a context)");
            continue;
        }
        // SAFETY: `held` is a pointer this kernel's own `Hw::switch` stored, to
        // a boxed `KernelCtx` in the direct map — mapped for the machine's life
        // whether or not the record it belongs to has since been released.
        let ctx = unsafe { &*(held as *const KernelCtx) };
        let top = ctx.kernel_stack_top;
        let same = held == subject && cpu != me;
        let on_its_stack = cpu != me
            && top != 0
            && rsp <= top
            && rsp > top.wrapping_sub(crate::process::KERNEL_STACK_SIZE as u64);
        // **The idle context is named and not numbered.** Its `id` is `None` and
        // its `kernel_stack_top` is zero by construction, so rendering it as a
        // task gives `pid=4294967295 stack_top=0x0` — which reads exactly like a
        // record something has overwritten, and was misread that way the first
        // time this report was used on a storm capture.
        match ctx.id {
            // And a *nonzero* stack top on one is a finding rather than a
            // rendering detail: nothing in the kernel writes that field after
            // `idle_ctx` builds it, so a value there is a write that had no
            // business landing.
            None => crate::log!(
                "  cpu{cpu} is on ctx {held:#x} (its idle context) stack_top={top:#018x} \
                 saved_rsp={:#018x}{}{}",
                ctx.rsp,
                if same { "  <== THE SAME CONTEXT" } else { "" },
                if top == 0 { "" } else { "  <== AN IDLE CONTEXT'S STACK TOP IS ZERO BY CONSTRUCTION" },
            ),
            Some(id) => crate::log!(
                "  cpu{cpu} is on ctx {held:#x} pid={} tid={} stack_top={top:#018x} \
                 saved_rsp={:#018x}{}{}",
                id.0.raw(),
                id.1.raw(),
                ctx.rsp,
                if same { "  <== THE SAME CONTEXT" } else { "" },
                if on_its_stack { "  <== AND THIS CRASH IS ON THAT STACK" } else { "" },
            ),
        }
    }
    // The depth ladder's answer, if this kernel carries one. Zero means either
    // no `heap-tripwire` or no task stack that ever reached the shallowest
    // rung, and the two are told apart by which kernel was booted — a fact the
    // capture already carries. A task kernel stack is 128 KiB of the same
    // dlmalloc arena as the `BTreeMap` nodes this class keeps killing, so how
    // close one has ever come to its own bottom is the first thing to rule out.
    if let Some((used, of)) = crate::sched::driver::stack_high_water() {
        crate::log!("  Task kernel stacks: deepest {used} of {of} bytes");
    }
    // What the heap sweep had covered by the time this crash happened. A death
    // with sweeps behind it and no band fired says the write that killed it was
    // not a bounded overrun of a live allocation; a death with none behind it
    // says only that the sweep never ran.
    if let Some((sweeps, records, overflowed)) = crate::mm::sweep_stats() {
        crate::log!(
            "  Heap sweeps: {sweeps} run, {records} live bands on the last walk{}",
            if overflowed { ", and the page table filled — the walk is incomplete" } else { "" },
        );
    }
}

/// The frame `context_switch` is about to pop, when its return slot is not a
/// return address.
///
/// **The `ret` is the last instruction that can still say what went wrong.**
/// Six pops and a `popfq` run ahead of it, so by the time the CPU faults the
/// register file holds the frame rather than the context: `rip` is a small
/// integer, `rflags` is whatever `popfq` made of a pointer, and the backtrace
/// is empty. Five deaths of that shape are on record in `issues/kernel/` — at
/// `0x1b`, at `0x0`, page-aligned and not — and not one of them could name the
/// task, the stack or the sibling CPU. This is checked before the pop so all
/// three are still readable.
///
/// It is not a debug aid: `0x1b` is `USER_DS`, and a Ring 0 `ret` to a segment
/// selector is the machine dying with the evidence already destroyed. One load
/// and one compare per switch, on a path that has just reloaded CR3.
#[cold]
#[inline(never)]
fn switch_frame_is_wrong(ctx: &KernelCtx, token: &RunToken<KernelPayload>) -> ! {
    let rsp = ctx.rsp;
    let (pid, tid) = ctx.id.map_or((u32::MAX, u32::MAX), |id| (id.0.raw(), id.1.raw()));
    crate::log!(
        "CONTEXT SWITCH ONTO A FRAME THAT IS NOT ONE: cpu={} pid={pid} tid={tid} \
         rsp={:#018x} top={:#018x} (top-rsp={}, and 64 is the entry frame, so a \
         context never saved) preempt={} fs_base={:#018x} incoming key={:?} \
         outgoing key={:?}",
        percpu::cpu_id(),
        rsp,
        ctx.kernel_stack_top,
        ctx.kernel_stack_top.wrapping_sub(rsp) as i64,
        ctx.preempt,
        ctx.fs_base,
        token.incoming().map(|k| k.0),
        token.outgoing().map(|k| k.0),
    );
    report_contexts(rsp, Some(ctx as *const KernelCtx as u64));
    if crate::mm::is_kernel_addr(rsp) && rsp.is_multiple_of(8) {
        const NAMES: [&str; 8] =
            ["r15", "r14", "r13", "r12", "rbx", "rbp", "rflags", "ret"];
        for (i, name) in NAMES.iter().enumerate() {
            let addr = rsp + (i as u64) * 8;
            // SAFETY: inside the incoming task's own kernel stack, whose top is
            // `kernel_stack_top` and whose length is `KERNEL_STACK_SIZE`.
            let word = unsafe { core::ptr::read_volatile(addr as *const u64) };
            crate::log!("  [{addr:#x}] {name:>6} = {word:#018x}");
        }
    }
    panic!(
        "context_switch: the frame about to be restored is not one — its rsp is not a kernel \
         address, or its return slot is not kernel text, or (under `stack-witness`) it is not \
         inside the stack this context's own `kernel_stack_top` names"
    );
}

/// See [`switch_frame_is_wrong`]. Kept tiny so the hot path is a load, a
/// compare and a not-taken branch.
///
/// **It returns the word it validated, and that returned value is what the
/// machine switches onto.** A caller that read `ctx.rsp` again would be
/// standing on a word this never saw: `cr3.activate()` is an `asm!` with a
/// memory clobber, so LLVM may not forward the load across it and a second read
/// is a second load in the emitted text — two `mov (%r14),…` where the guard
/// covers only the first. `#[must_use]` is what makes ignoring the answer a
/// diagnostic rather than a silent reintroduction of that gap.
#[inline]
#[must_use]
fn check_switch_frame(ctx: &KernelCtx, token: &RunToken<KernelPayload>) -> u64 {
    let rsp = ctx.rsp;
    if !crate::mm::is_kernel_addr(rsp) || !rsp.is_multiple_of(8) {
        switch_frame_is_wrong(ctx, token);
    }
    // **Is it this task's own stack, and not merely *a* kernel address.**
    //
    // A guest parked on its shutdown action — the one capture of this class
    // taken with both vCPUs still readable — had cpu1 at `RIP=1b7b9f15ffd23100`
    // with `SS=0`, `DF` set and `RSP=0xffff800000c00680`, which is inside the
    // *per-CPU* region and no stack at all. That is `context_switch`'s tail
    // exactly: `popfq` took garbage flags and `ret` took a garbage return
    // address, sixty-four bytes above an `rsp` of `0xffff800000c00640`. The two
    // tests above passed it, because that address is a kernel address and the
    // word at `+56` was one too, so a green guard was never evidence.
    //
    // An incoming context's `rsp` belongs to the stack its own
    // `kernel_stack_top` names or it belongs to nothing: `alloc_kernel_stack`
    // builds the entry frame at `top - 64` and every later save comes from a
    // `context_switch` running on that same stack. Two compares, and the
    // difference is a report that names the task against a triple fault that
    // says nothing on either channel.
    //
    // **And the idle context is not exempt, which is the whole point.** Its
    // `kernel_stack_top` is zero by construction — per-CPU, and not knowable at
    // the boot-time init that builds the record — but the stack it names is
    // knowable *here*, on the CPU the record belongs to, which is exactly where
    // the arm below this one already reads it to load the TSS. The parked
    // capture's `rsp` was `0xffff800000c00640`, in the per-CPU region, and cpu0's
    // idle stack in that same boot ran to `0xffff800000e21000` — so the frame
    // was in neither a task stack nor an idle one, and a version of this test
    // that skipped `id: None` would have skipped it. A first storm of 7,349
    // boots with the task-only form fired zero times against 19 silent deaths,
    // which is what asking the question of the wrong contexts looks like.
    //
    // `ctx.rsp` on an idle context is always a real one when it is restored: a
    // CPU only ever switches *to* idle after switching away from it, which is
    // what wrote the value; the idle loop itself is entered by jump.
    #[cfg(feature = "stack-witness")]
    {
        let top = match ctx.id {
            Some(_) => ctx.kernel_stack_top,
            None => percpu::idle_stack_top(),
        };
        if rsp > top || rsp <= top - crate::process::KERNEL_STACK_SIZE as u64 {
            switch_frame_is_wrong(ctx, token);
        }
    }
    // SAFETY: `rsp` is a kernel address eight bytes below the top of the
    // incoming task's own kernel stack at the shallowest, so the return slot is
    // mapped.
    let ret = unsafe { core::ptr::read_volatile((rsp + 56) as *const u64) };
    if !crate::mm::is_kernel_addr(ret) {
        switch_frame_is_wrong(ctx, token);
    }
    #[cfg(feature = "switch-witness")]
    switch_witness_capture(ctx, token, rsp);
    rsp
}

/// The seven words `context_switch` pops between [`check_switch_frame`] and its
/// `ret`, copied here and compared there.
///
/// **What is unmeasured, stated exactly.** The check above reads `ctx.rsp`,
/// tests it three ways, reads the word at `+56` and returns it. The frame is
/// popped in [`crate::sched::driver::context_switch`], and between the two lie the
/// preempt-count swap, two per-CPU identity writes, the TSS stack handover, a
/// **`mov cr3`**, a `wrfsbase` and the `RUNNING_CTX` store. Nothing tests the
/// frame across that span, and the class's one parked capture is a `popfq`/`ret`
/// off a frame at `0xffff800000c00640` — inside the per-CPU region, no stack at
/// all — which every check that runs at the *check* would have passed. So either
/// the frame is rewritten in the span, or `ctx.rsp` is, and this separates them:
/// it holds the eight words *and* the pointer, and is compared against the stack
/// pointer the machine is standing on rather than against the field again.
///
/// Per CPU and touched by that CPU alone, in a region where preemption is off and
/// this CPU is the only executor its own switch has — `sched::driver::tripwire`'s
/// argument, one level down.
#[cfg(feature = "switch-witness")]
struct SwitchShadow {
    /// The `ctx.rsp` the check validated.
    rsp: u64,
    /// The eight words at that address at the moment of the check: `r15`,
    /// `r14`, `r13`, `r12`, `rbx`, `rbp`, `rflags`, and the return slot.
    words: [u64; 8],
    /// The incoming context, so the compare can ask whether the *field* moved
    /// as well as whether the frame did.
    ctx: *const KernelCtx,
    /// The **outgoing** context — `context_switch`'s `rdi`, and the record whose
    /// `rsp` field the switch is about to write. A capture of this class taken
    /// from a parked guest has that pointer still in `rdi` at the wild `ret`, so
    /// naming it here is what turns a register dump into a pair of records.
    save: u64,
    /// The stack the incoming context claims, so a report can say where the
    /// frame lies relative to it without a second lookup.
    top: u64,
    incoming: u64,
    outgoing: u64,
}

#[cfg(feature = "switch-witness")]
struct SwitchShadowSlot(core::cell::UnsafeCell<SwitchShadow>);

// SAFETY: indexed by the *calling* CPU's own id, written between
// `check_switch_frame` and the `mov rsp, rsi` of that same CPU's switch and read
// in the instruction after it. No other CPU names this slot, and this CPU cannot
// be preempted across the window — `pass` raised the preempt count and the
// incoming context's own count is loaded before the switch.
#[cfg(feature = "switch-witness")]
unsafe impl Sync for SwitchShadowSlot {}

#[cfg(feature = "switch-witness")]
static SWITCH_SHADOW: [SwitchShadowSlot; crate::sched::MAX_CPUS] = [const {
    SwitchShadowSlot(core::cell::UnsafeCell::new(SwitchShadow {
        rsp: 0,
        words: [0; 8],
        ctx: core::ptr::null(),
        save: 0,
        top: 0,
        incoming: u64::MAX,
        outgoing: u64::MAX,
    }))
}; crate::sched::MAX_CPUS];

#[cfg(feature = "switch-witness")]
fn switch_witness_capture(ctx: &KernelCtx, token: &RunToken<KernelPayload>, rsp: u64) {
    // SAFETY: this CPU's own slot; see the `Sync` justification above.
    let shadow = unsafe { &mut *SWITCH_SHADOW[percpu::cpu_id() as usize].0.get() };
    shadow.rsp = rsp;
    shadow.ctx = ctx as *const KernelCtx;
    shadow.save = token.save_ptr() as u64;
    shadow.top = match ctx.id {
        Some(_) => ctx.kernel_stack_top,
        None => percpu::idle_stack_top(),
    };
    shadow.incoming = token.incoming().map_or(u64::MAX, |k| k.0);
    shadow.outgoing = token.outgoing().map_or(u64::MAX, |k| k.0);
    for (i, word) in shadow.words.iter_mut().enumerate() {
        // SAFETY: `check_switch_frame` has just established that `rsp` is a
        // kernel address, eight-aligned, and inside the stack this context's own
        // `kernel_stack_top` names — and it already read the eighth word.
        *word = unsafe { core::ptr::read_volatile((rsp + (i as u64) * 8) as *const u64) };
    }
}

/// The compare, from inside `context_switch` with the stack pointer already
/// moved and the first `pop` one instruction away.
///
/// `rsp` is the machine's own stack pointer, handed over in `rdi`, and the field
/// is re-read here beside it. **The two are separate questions and were one
/// until the single load existed.** `rsp == shadow.rsp` says the word the
/// machine is standing on is the word the check validated — which is now
/// `Hw::switch`'s invariant rather than a hope, because `check_switch_frame`
/// returns that word and nothing reads the field again. `field == shadow.rsp`
/// says the separate thing: that nothing wrote `ctx.rsp` in the window at all.
/// Before the invariant a moved field *was* a moved stack pointer — the switch
/// re-read it across the `mov cr3`, which LLVM may not forward a load over — so
/// the two answers could not come apart, and `switch-witness-mutate-rsp` is the
/// build in which they do.
///
/// # Safety
/// Called only from [`crate::sched::driver::context_switch`], with `rsp` equal
/// to the stack pointer and the shadow of this CPU's own pending switch filled.
#[cfg(feature = "switch-witness")]
pub(crate) unsafe extern "C" fn switch_witness_verify(rsp: u64) {
    // SAFETY: this CPU's own slot; see the `Sync` justification above.
    let shadow = unsafe { &*SWITCH_SHADOW[percpu::cpu_id() as usize].0.get() };
    let mut now = [0u64; 8];
    for (i, word) in now.iter_mut().enumerate() {
        // SAFETY: the frame the machine is standing on, which the check
        // established is inside a kernel stack.
        *word = unsafe { core::ptr::read_volatile((rsp + (i as u64) * 8) as *const u64) };
    }
    // SAFETY: a `KernelCtx` this kernel's own pass produced, whose record is
    // alive until a later pass releases it.
    let field = unsafe { core::ptr::read_volatile(&raw const (*shadow.ctx).rsp) };
    if rsp == shadow.rsp && field == shadow.rsp && now == shadow.words {
        return;
    }
    switch_window_is_wrong(rsp, field, &now, shadow);
}

/// A word of the incoming frame — or the pointer to it — changed between the
/// check and the pop.
#[cfg(feature = "switch-witness")]
#[cold]
#[inline(never)]
fn switch_window_is_wrong(rsp: u64, field: u64, now: &[u64; 8], shadow: &SwitchShadow) -> ! {
    const NAMES: [&str; 8] = ["r15", "r14", "r13", "r12", "rbx", "rbp", "rflags", "ret"];
    crate::log!(
        "SWITCH WINDOW: cpu{} the frame is not the one that was checked — checked \
         rsp={:#018x}, standing on rsp={:#018x} ({}), the incoming ctx {:#x} now says \
         {field:#018x} ({}), outgoing ctx (rdi) {:#018x}; incoming key={} outgoing key={}; \
         the incoming stack is [{:#018x}, {:#018x})",
        percpu::cpu_id(),
        shadow.rsp,
        rsp,
        if rsp == shadow.rsp { "THE SAME" } else { "MOVED" },
        shadow.ctx as u64,
        if field == shadow.rsp { "unchanged" } else { "CHANGED SINCE THE CHECK" },
        shadow.save,
        shadow.incoming,
        shadow.outgoing,
        shadow.top.wrapping_sub(crate::process::KERNEL_STACK_SIZE as u64),
        shadow.top,
    );
    for (i, name) in NAMES.iter().enumerate() {
        let checked = shadow.rsp + (i as u64) * 8;
        let standing = rsp + (i as u64) * 8;
        // SAFETY: both are eight-aligned kernel addresses inside a stack this
        // CPU has just been executing on or is about to.
        let there_now = unsafe { core::ptr::read_volatile(checked as *const u64) };
        crate::log!(
            "  {name:>6}: [{checked:#x}] was {:#018x}, is {there_now:#018x}{} | \
             [{standing:#x}] is {:#018x}",
            shadow.words[i],
            if there_now == shadow.words[i] { "" } else { "  <== THE CHECKED FRAME WAS WRITTEN" },
            now[i],
        );
    }
    report_contexts(rsp, Some(shadow.ctx as u64));
    panic!(
        "context_switch: the seven words it is about to pop are not the seven words \
         `check_switch_frame` validated, or the stack pointer it is standing on is not \
         the one that was checked"
    );
}

/// The negative control: stage the defect [`switch_witness_verify`] exists to
/// catch, in the window it exists to watch, and require it to fire.
///
/// **An instrument that has never fired has not been shown to be able to.** Both
/// arms write exactly once, at the [`MUTATE_AT`]th switch of the boot, so the
/// machine reaches a state where the log is up and the report is readable rather
/// than dying on the first switch a CPU takes.
///
/// * `switch-witness-mutate-frame` writes one word of the incoming frame — the
///   `rbx` slot — which is the "another execution wrote this frame" arm, and the
///   one no check before this one could see.
/// * `switch-witness-mutate-rsp` moves `ctx.rsp` up by eight *after* the check
///   has validated it. It stages the double-load hazard, and since
///   [`check_switch_frame`] returns the word it validated it is **the negative
///   control for that single load**: its verdict is not whether the witness
///   fires but which of the two clauses in [`switch_witness_verify`] the report
///   names. A switch that re-read the field reports `MOVED` and stands eight
///   bytes up its own stack; one that carries the validated value reports `THE
///   SAME` with the field `CHANGED SINCE THE CHECK`, and the seven words agree.
///   Eight and not garbage on purpose — the frame stays inside the same kernel
///   stack, so the arm is decided by a word of the report and not by which way
///   the machine happened to die.
///
/// # Safety
/// A mutation build is not a kernel anybody boots for any other purpose.
#[cfg(any(feature = "switch-witness-mutate-frame", feature = "switch-witness-mutate-rsp"))]
unsafe fn switch_witness_mutate(restore: *const KernelCtx) {
    /// Switches into the boot before the one write.
    ///
    /// **Small because it was measured.** A boot of the storm's shape reaches
    /// `compositor: ready` with fewer than three hundred context switches behind
    /// it — a `MUTATE_AT` of 300 produced a clean boot, which is what a control
    /// that never fires looks like whether or not the instrument works. Eight is
    /// past the three kernel threads and inside the first dispatches, and it is
    /// reached by every boot there is.
    const MUTATE_AT: u64 = 8;
    static SWITCHES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    if SWITCHES.fetch_add(1, core::sync::atomic::Ordering::Relaxed) != MUTATE_AT {
        return;
    }
    // SAFETY: a live `KernelCtx` this pass produced, and a build that exists to
    // corrupt it.
    let rsp = unsafe { (*restore).rsp };
    #[cfg(feature = "switch-witness-mutate-frame")]
    // SAFETY: the `rbx` slot of the frame `check_switch_frame` has just
    // validated, inside the incoming task's own kernel stack.
    unsafe {
        core::ptr::write_volatile((rsp + 32) as *mut u64, 0xdead_beef_dead_beef)
    };
    #[cfg(feature = "switch-witness-mutate-rsp")]
    // SAFETY: as above, and the field is this context's own.
    unsafe {
        core::ptr::write_volatile(&raw const (*restore).rsp as *mut u64, rsp + 8)
    };
}

impl Hw for KernelHw {
    type Payload = KernelPayload;

    /// Load the incoming task's machine state, then hand the stacks over.
    ///
    /// Everything this needs is in the two contexts the token names, and that
    /// is deliberate: the pass that produced the token has already ended, so
    /// there is no `CpuSched` left to consult and nothing scheduler-related to
    /// do on either side of the switch.
    ///
    /// The order is forced. `fs_base` and the preempt count are live per-CPU
    /// state, so the outgoing context has to capture them before anything is
    /// reloaded; the percpu identity, the TSS stack and CR3 must all be the
    /// incoming task's *before* the stack pointer moves, because after
    /// `context_switch` this frame no longer exists.
    ///
    /// **The outgoing `rsp` is the last thing written, and everything above is
    /// a window.** `context_switch`'s `mov [rdi], rsp` is what makes the
    /// outgoing context resumable; until it retires, that context still names
    /// the stack pointer from the previous switch away — or, for a task that
    /// has never been switched away, `alloc_kernel_stack`'s entry frame. The
    /// pass that produced this token has already ended, so it is the *core*
    /// that has to keep another CPU out of that window; `answer_steal_requests`
    /// is where it does.
    unsafe fn switch(&self, token: RunToken<KernelPayload>) {
        let save = token.save_ptr();
        let restore = token.restore_ptr();
        // SAFETY: both pointers came from `SchedPass::finish`, which formed
        // them from live Box-backed task records (or this CPU's own idle
        // context). A record is only freed by `release`, which runs in a later
        // pass — i.e. never while its context is the one being switched.
        //
        // `cpu::wrfsbase` asks its caller to own the FS base it installs, and
        // this is the one place in the kernel that does: `incoming.fs_base` was
        // either `rdfsbase`'d off this same register when that task was switched
        // away (the line below), or built by `loader::tls` inside the task's own
        // mapped TLS block — so it is canonical by construction, and the thread
        // it belongs to is the one this switch is making current.
        unsafe {
            (*save).fs_base = cpu::rdfsbase();
            (*save).preempt = crate::preempt::count();
            let incoming: &KernelCtx = &*restore;
            // **The only load of `incoming.rsp` this switch has.** See
            // [`check_switch_frame`]: reading the field again below would put a
            // second `mov (%r14),…` after the `mov cr3`, and the guard above
            // covers only the first.
            let rsp = check_switch_frame(incoming, &token);
            #[cfg(any(
                feature = "switch-witness-mutate-frame",
                feature = "switch-witness-mutate-rsp"
            ))]
            switch_witness_mutate(restore);
            crate::preempt::set_count(incoming.preempt);
            percpu::set_current_tid(incoming.id.map(|id| id.1));
            percpu::set_current_pid(incoming.id.map(|id| id.0));
            match incoming.id {
                Some(_) => {
                    // Here and not in the pass: this arm is the one place a
                    // *task* rather than the idle context becomes what a CPU is
                    // running, which is what `ran=` has to count for a machine
                    // that schedules but runs nothing to be visible at all.
                    #[cfg(feature = "boot-actuators")]
                    crate::heartbeat::note_dispatch();
                    percpu::set_kernel_stack(incoming.kernel_stack_top);
                    incoming.cr3.activate();
                    cpu::wrfsbase(incoming.fs_base);
                }
                // The idle context. Its stack top is per-CPU and therefore not
                // knowable at the boot-time init that builds the context, so it
                // is read here, on the CPU it belongs to.
                None => {
                    percpu::set_kernel_stack(percpu::idle_stack_top());
                    incoming.cr3.activate();
                }
            }
            RUNNING_CTX[percpu::cpu_id() as usize]
                .store(restore as u64, core::sync::atomic::Ordering::Relaxed);
            context_switch(&raw mut (*save).rsp, rsp);
        }
    }

    /// The finalize sink. Reached exactly once per task, from the pass after
    /// the one that killed it, which by construction runs on another stack —
    /// so dropping the payload here frees a kernel stack nothing stands on and
    /// releases the address-space `Arc` for the one and only time.
    ///
    /// It is also where a retirer's wait ends. The announcement is deliberately
    /// the *last* thing: `retire_task` returns to a caller about to free memory
    /// the dead thread's page tables mapped, and what makes that safe is not
    /// that the thread stopped running but that this drop already happened.
    fn release(&self, _key: TaskKey, payload: KernelPayload, acct: TaskAccounting) {
        let handle = payload.handle.clone();
        handle.finalize(acct);
        drop(payload);
        handle.publish_released();
    }
}

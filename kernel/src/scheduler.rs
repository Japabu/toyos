//! The kernel-facing scheduler API (spec §4's `kernel/src/sched/mod.rs`).
//!
//! The scheduler itself is `toyos-sched`, driven by `kernel/src/sched/`. This
//! file is *only* a surface: no decision, no state transition and no
//! ordering-sensitive step happens here.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use hashbrown::HashMap;
use toyos_sched::fair::{ShareState, QUANTUM_NS};
use toyos_sched::hw::{Machine, Nanos};
use toyos_sched::task::{WaitClass, WakeCause, WakeReason};

use crate::arch::percpu;
use crate::completion::{self, Cancel, Outcome, Subject};
use crate::hw::HW;
use crate::pipe::PipeId;
use crate::process::{self, Pid, Tid};
use crate::sched::driver::{self, cpus, preempt_off, Dispose, NewTask};
use crate::sched::payload::{KShare, KShared, KWaitQueue, KernelLock, ThreadSched};
use crate::sched::reap_gate::ReapGate;
use crate::sched::waitqs;
use crate::sync::Lock;
use crate::time::{Cadence, Deadline, Duration, Tripwire};
use crate::DirectMap;

pub use crate::sched::driver::{
    current_address_space, enter_idle_loop, in_pass as in_schedule_self, total_cpu_ns,
    write_stack_canary, Ticket,
};
pub use crate::sched::MAX_CPUS;

/// Spec §6.4's lock-across-switch tripwire.
///
/// A `sync::Lock` guard raises the preempt count and keeps it raised until it
/// drops, so a count *above* what the calling context is entitled to means a
/// spinlock is held. Reaching a switching scheduler entry that way parks the
/// lock on a stack nothing will return to, and every other CPU that takes it
/// then spins into `Lock::lock`'s 500M-spin DEADLOCK panic — which names the
/// victim and never the culprit. This names the culprit, at its own call site.
///
/// `#[track_caller]` all the way down is what makes the message point outside
/// this file; without it every trip reports the same three lines.
#[track_caller]
fn assert_baseline(baseline: u32) {
    let depth = crate::preempt::count();
    assert!(
        depth == baseline,
        "scheduler entered while a lock is held: preempt depth {depth}, baseline {baseline}",
    );
}

/// The depth an *unnested* trap handler runs at: one level, raised by the entry
/// asm (`arch/syscall.rs`, `common_entry`) and lowered on the way out.
///
/// Each entry raises its own level, so a fault taken inside a syscall runs at
/// two — routine, not hypothetical (a demand-paging fault on a user page the
/// handler touches). No asserting entry is reachable from there today: every
/// kernel-mode fault funnels to `schedule_no_return`, which deliberately does
/// not assert. The first demand-paging path that parks instead of spinning, or
/// any decision to kill a kernel-faulting process through `process::exit`,
/// breaks that and trips this on a nested trap holding no lock at all. The
/// check establishes `depth != baseline`; the message names the cause that
/// motivates it, and a nested trap is the other way to get there.
const BASELINE_TRAP: u32 = 1;

/// The depth the deferred-preempt poll runs at. Zero, and not `BASELINE_TRAP`,
/// because all three routes into it are *past* the entry level: the Ring 3
/// timer stub (`arch/idt/timer.rs`) never raises one, `kernel_exit_to_user_check`
/// (`arch/idt/mod.rs`) runs after the `lock sub`, and `preempt::enable`'s slow
/// path only calls in at zero. The idle loop reaches it through the third —
/// `reap_poisoned`'s `PROCESS_TABLE` guard drop — not as a route of its own.
const BASELINE_IRQ_EXIT: u32 = 0;

/// The depth a *blocking* site is entitled to, which is not the same for every
/// context and was assumed to be until a kernel thread existed.
///
/// [`BASELINE_TRAP`] for a user thread: `common_entry`'s `lock add` covers the
/// whole of every syscall and exception, so a park reached from one starts a
/// level up. **Zero for a kernel thread's body**, which is not a trap at all —
/// `driver::trampoline_entry` discharged the single level `spawn` put in its
/// context and nothing has raised one since.
///
/// Reading the entitlement from the context rather than assuming the trap is
/// what keeps §6.4's tripwire a tripwire for both: a kernel thread that parks
/// holding a `Lock` still trips it, one level lower.
///
/// **Answered from `sched::kthread`'s rows and never from the `CpuSched`.**
/// This runs on every blocking call in the machine and `prepare_wait` has not
/// raised the preempt count yet, so a reader that walked the running task
/// would be aliasing the `&mut CpuSched` a preempting pass takes.
fn blocking_baseline() -> u32 {
    if crate::sched::kthread::current_is_kernel_thread() {
        0
    } else {
        BASELINE_TRAP
    }
}

/// The right to give the CPU back.
///
/// `specs/completion-architecture-spec.md` §6. Made once per trap entry and
/// once per kernel-thread body; [`Parkable::of_current`] asserts the context's
/// baseline preempt depth, so a caller holding a spinlock cannot make one. Not
/// `Copy`, not `Clone`, and never stored in a struct: it is threaded down the
/// call chain by reference, and that is the whole mechanism.
///
/// **What the token delivers is a compile-time property about the *context*,
/// and nothing about which locks are held.** A function with no `Parkable` in
/// scope cannot park, cannot take a sleep lock, and cannot call anything that
/// does — transitively, through the whole call graph. That is why
/// `sched::dump`, `panic_console`, every ISR and every `Drop` impl are
/// structurally unable to block: none of them can make one.
///
/// **It is not a borrow rule.** §6.2 records the first draft's proposal — a
/// `&mut Parkable` for `wait` so that a live sleep guard would make a park a
/// compile error — and why it is wrong: three of this design's own sections
/// require a sleep lock to be *held* across a park, which is the entire point
/// of giving the CPU back during a device round trip. What still catches a
/// *spinlock* held across a park is the runtime assertion here and at the park
/// (RT1, §6.3), because `Lock::lock` takes no token and must not.
///
/// The first consumers are C3's `completion::wait` and C5's `SleepLock::lock`.
/// C1 builds the token and puts it where the assertion already was, so the
/// failure names the entry rather than the park.
pub struct Parkable(());

impl Parkable {
    /// Assert that this context may park, and mint the proof.
    ///
    /// There is no `Parkable::boot()` and no spin fallback: a primitive that
    /// silently degrades to a spin depending on invisible context is the
    /// sentinel class the root `CLAUDE.md` forbids. Boot has no token because
    /// boot has no current task, and code that runs there takes `try_lock`.
    #[track_caller]
    pub fn of_current() -> Parkable {
        assert_baseline(blocking_baseline());
        Parkable(())
    }
}

/// Process-scoped thread identity. Tids are per-process, so the scheduler
/// needs the pair to name a thread system-wide.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(pub Pid, pub Tid);

impl TaskId {
    pub fn pack(self) -> u64 {
        self.1.raw() as u64 | (self.0.raw() as u64) << 32
    }
    pub fn unpack(v: u64) -> Self {
        Self(Pid::from_raw((v >> 32) as u32), Tid::from_raw(v as u32))
    }
}

/// The running task, or `None` where there is no task: boot, and an idle CPU.
///
/// **Two per-CPU reads and no lock**, which is a requirement rather than a
/// nicety: `SleepLock::lock` calls this on every acquire, with preemption still
/// on, so a reader that asked the `CpuSched` which task is running would alias
/// the `&mut CpuSched` a preempting pass takes. `sched::kthread::current_row`
/// reads the same two words for the same reason and says so at more length.
pub fn current_task() -> Option<TaskId> {
    match (percpu::current_pid(), percpu::current_tid()) {
        (Some(pid), Some(tid)) => Some(TaskId(pid, tid)),
        _ => None,
    }
}

impl core::fmt::Display for TaskId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}", self.0, self.1)
    }
}

/// Pid → share. Touched at spawn and at process teardown only; the *charge*
/// path reaches the share through the task that owns it, which is what retires
/// the old scheduler's one-hot-lock-per-charge (spec §9.1).
static SHARES: Lock<Option<HashMap<Pid, Arc<KShare>>>> = Lock::new(None);

pub fn init() {
    *SHARES.lock() = Some(HashMap::new());
    driver::init();
}

/// The share a new task of `pid` joins. Created `NonRunnable { lag: 0 }` so
/// that the adopting CPU's `enter_runnable` produces exactly the old
/// `new_runnable(frontier)` state: vruntime at the frontier, refcount one.
fn share_for(pid: Pid) -> Arc<KShare> {
    let mut guard = SHARES.lock();
    let map = guard.as_mut().expect("scheduler not initialized");
    map.entry(pid)
        .or_insert_with(|| {
            Arc::new(KShare::new(KernelLock::new(ShareState::NonRunnable {
                lag: 0,
            })))
        })
        .clone()
}

fn share_of(pid: Pid) -> Option<Arc<KShare>> {
    SHARES.lock().as_ref()?.get(&pid).cloned()
}

/// The process is gone from the table. Live tasks keep their `Arc` alive, so
/// a thread still finishing its exit path can still be charged.
pub fn remove_vruntime(pid: Pid) {
    if let Some(map) = SHARES.lock().as_mut() {
        map.remove(&pid);
    }
}

pub fn process_vruntime(pid: Pid) -> u64 {
    share_of(pid).map_or(0, |s| s.vruntime(driver::frontier()))
}

pub fn process_lag(pid: Pid) -> i64 {
    share_of(pid).map_or(0, |s| s.lag())
}

pub fn global_min_vruntime() -> u64 {
    driver::frontier().get()
}

/// Build and place a new task. The caller supplies everything but the share.
pub fn enqueue_new(
    id: TaskId,
    kernel_stack: crate::process::OwnedAlloc,
    entry_rsp: u64,
    address_space: crate::process::PageTables,
    fs_base: u64,
) -> ThreadSched {
    driver::spawn(NewTask {
        id,
        kernel_stack,
        entry_rsp,
        address_space,
        fs_base,
        share: share_for(id.0),
    })
}

/// Phase 1 of the wait handshake: register the running thread on `queue`.
/// The caller must then re-check its condition and either cancel the ticket or
/// block on it — registering *before* the re-check is what closes the
/// check-then-block window.
///
/// The ticket holds preemption off until it is consumed; the re-check may take
/// whatever locks it needs, and the deferred request is served by the block or
/// by the cancel. See [`Ticket`].
///
/// The baseline assertion is [`Parkable::of_current`]'s, which is RT1: the
/// token is minted where the decision to park is made, so a trip names that
/// site. The proof is dropped again here because nothing below takes one yet —
/// C3's `completion::wait` and C5's `SleepLock::lock` are what thread it.
#[must_use = "a wait ticket must be blocked on or cancelled"]
#[track_caller]
pub fn prepare_wait(queue: &KWaitQueue, cancel: Cancel, class: WaitClass) -> Ticket<'_> {
    let _parkable = Parkable::of_current();
    Ticket::register(queue, cancel, class)
}

/// Phase 2: park the running thread on the queue it registered with.
///
/// Taking the ticket by value is the whole point: a park that reaches the
/// machine without a registration behind it is the lost-wake window, and there
/// is no other way to construct one.
///
/// **The deadline is a [`Deadline`] and no longer a `u64` whose zero means
/// "forever".** That convention was invisible at a call site and inverted by
/// `specs/completion-architecture-spec.md` §14.1's absolute form, where zero is
/// simply the past; a site left passing `0` through that change becomes a busy
/// loop rather than a compile error. [`Deadline::never`] is the one that does
/// not arm a timer, and every other value arms one — including
/// [`Deadline::passed`], which fires at the next pass.
#[track_caller]
pub fn block_on(ticket: Ticket<'_>, deadline: Deadline) {
    // One level above the calling context's baseline: the ticket has held the
    // registration window's own level since `prepare_wait`, and `pass_block`
    // inherits it.
    assert_baseline(blocking_baseline() + 1);
    driver::pass_block(ticket, (!deadline.is_never()).then(|| Nanos(deadline.nanos())));
}

/// Give the CPU up voluntarily, keeping the claim on it: the pass decides
/// whether anything else deserves the quantum.
///
/// **The thirty-three lines that used to stand here were the deleted
/// `scheduler::wait_until`'s, and the commit that deleted the function left its
/// doc behind on the next item.** They described a register/re-check/park loop
/// this function has never had, named `wake_task` — deleted as code in the same
/// commit — and said a retire mid-park "turns the block into an exit", which
/// `pass_block`'s `Commit::Killed` arm inverted to `dispose_none` nineteen lines
/// away. The loop they describe is real and lives in `completion::wait_until`,
/// where the argument belongs; it is not restated here.
#[track_caller]
pub fn yield_now() {
    assert_baseline(BASELINE_TRAP);
    driver::pass(Dispose::Yield);
}

/// Unified preempt entry — the Ring 3 timer path, `kernel_exit_to_user_check`
/// and the `preempt::enable` slow path all funnel through here. The pass
/// itself decides whether the running thread keeps the CPU (quantum expiry or
/// an RT task in the band); this only asks it to look.
#[track_caller]
pub fn do_preempt() {
    if in_schedule_self() {
        return;
    }
    assert_baseline(BASELINE_IRQ_EXIT);
    crate::preempt::clear_need_resched();
    if percpu::current_tid().is_none() {
        // No thread on this CPU: either the idle loop, which passes every
        // iteration anyway, or boot, which has no `CpuSched` yet — an ISR that
        // raised the request during device init would otherwise reach the
        // machine before it exists. The request is moot, not deferred.
        return;
    }
    crate::trace::trace(crate::trace::Kind::Preempt, 0);
    driver::pass(Dispose::None);
}

/// A killed thread's last safe point: the return to Ring 3.
///
/// **This is what answers §7.2's "what reaps a killed task that never parks
/// again".** The pick used to reap a killed task before it could be
/// dispatched; now it dispatches it, so a thread killed while running in
/// userland would run for ever if nothing stopped it here. What stops it is
/// the boundary itself: the kernel stack is provably empty at this point —
/// that is what makes it the boundary — so the exit takes nothing with it, and
/// the timer interrupt bounds how long a Ring 3 loop can put it off.
///
/// **Called at the last exit boundary and from every one of them**, which is
/// two corrections to the shape this landed with:
///
/// * `kernel_exit_to_user_check` called it *once, above* its resched loop. That
///   loop enables interrupts and gives the CPU away for a whole pass, so a
///   retire landing inside it reached Ring 3 unobserved. It is now the loop's
///   own condition, so the check is the last thing before the return.
/// * The Ring 3 timer stub did not call the epilogue at all — and
///   `apic::kick_cpu` sends TIMER_VECTOR, so that stub is where a retire's own
///   IPI lands. A thread killed while running in userland was preempted,
///   queued in the dying list, picked straight back off it and returned to
///   Ring 3, once per tick, unbounded. `arch::idt::timer` now runs the same
///   epilogue every other Ring 3 return runs.
///
/// What is left is one instant wide: the kill bit is a remote CPU's plain
/// atomic and can be raised after this load and before the `iretq`, with IF=0
/// in between. That thread reaches Ring 3 with the kill pending and comes back
/// through this boundary on the retire's own `Urgency::Preempt` kick — which
/// **follows** the bit rather than preceding it. `retire::begin` sets KILL with
/// a locked read-modify-write in `claim_retire`, and `RetireTicket::post` is
/// what pushes the message and issues the kick, so the kick cannot be consumed
/// by a CPU that has not yet seen the bit. The reverse order is what would
/// break the bound, not what establishes it: an IPI taken in Ring 0 ahead of a
/// bit nobody can see leaves no interrupt in flight once the bit appears, and
/// the victim sits in Ring 3 until an unrelated tick.
/// `scheduler-core-spec.md` invariant 7 states that bound, and
/// `toyos-sched/src/retire.rs`'s module header states the same order.
///
/// One relaxed load per return to userland, which is the whole cost. The
/// baseline is `BASELINE_IRQ_EXIT`: every entry stub discharges its own level
/// before calling the epilogue this runs in, and the Ring 3 timer stub takes
/// no level at all.
#[track_caller]
pub fn exit_if_killed() {
    if !driver::current_kill_pending() {
        return;
    }
    assert_baseline(BASELINE_IRQ_EXIT);
    // **Nothing else, and that is the point.** This is the reap the pick used
    // to do, moved onto the victim's own stack — not an exit the thread chose.
    // The retirer owns every book: it marked the thread, it publishes the
    // process's exit, it frees the mappings, and it is parked on
    // `released()`, which `Hw::release` answers when this pass drops the
    // payload. A `mark_thread_zombie` here would be a second teardown racing
    // that one, with an exit code nobody asked for.
    driver::pass(Dispose::Exit);
    unreachable!("exit_if_killed: returned from the exit pass");
}

#[track_caller]
pub fn exit_current(code: i32) -> ! {
    assert_baseline(BASELINE_TRAP);
    {
        let mut guard = process::PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        let tid = percpu::current_tid().unwrap();
        let pid = percpu::current_pid().unwrap();
        process::mark_thread_zombie(table, pid, tid, code);
    }
    driver::pass(Dispose::Exit);
    unreachable!("exit_current: returned from the exit pass");
}

/// Claim one specific thread's rendezvous word and post its wake.
///
/// **The whole of what a completion post does after it has stored its
/// record**, and the only wake path left that names a task rather than a
/// queue: `wake_task(TaskId)` — the pid/tid lookup that went with the parking
/// lot — is deleted with it, because a watcher already holds what it needs.
///
/// No §6.4 baseline assert here or on any other wake path: a wake posts a
/// message and never switches, and waking from *inside* a lock is the protocol
/// rather than a violation of it (§8.1's claim-and-post happens under the waitq
/// leaf lock, and `KernelLock` is documented as a legal mailbox producer for
/// exactly that reason).
///
/// `true` means **this** call won the claim, which is the only sense in which
/// it woke anybody: a task whose word another waker or its own deadline has
/// already taken is already on its way back to its own code, and a second
/// caller reporting it as woken counts one thread twice.
/// [`completion::post_n`] is the one caller that reads the answer.
pub fn wake_sched(shared: &Arc<KShared>, boost: Option<Nanos>) -> bool {
    let cause = match boost {
        Some(until) => WakeCause::boosted(WakeReason::Woken, until),
        None => WakeCause::new(WakeReason::Woken),
    };
    preempt_off(|p| toyos_sched::waitq::wake_direct(shared, cause, cpus(), &HW, p))
}

/// Wake pipe readers, lending each an RT window if the writer holds one
/// (spec §8.5). The pipe is also marked, so a reader that was runnable rather
/// than blocked at write time takes the window at its own consume point.
pub fn wake_pipe_readers(pipe_id: PipeId) {
    let Some(end) = crate::pipe::readers_queue(pipe_id) else {
        return;
    };
    if driver::current_is_rt() {
        crate::pipe::set_rt_boost_pending(pipe_id);
        completion::post_boosted(Subject::of(&end.watch), Outcome::Ready, boost_window());
    } else {
        completion::post(Subject::of(&end.watch), Outcome::Ready);
    }
}

pub fn wake_pipe_writers(pipe_id: PipeId) {
    if let Some(end) = crate::pipe::writers_queue(pipe_id) {
        completion::post(Subject::of(&end.watch), Outcome::Ready);
    }
}

/// How long a lent RT priority lasts: a wall-clock bound on time *held*
/// (spec §8.5, invariant I9), one quantum wide.
pub fn boost_window() -> Nanos {
    HW.now().after(QUANTUM_NS)
}

/// Grant the running thread the window its producer left on a pipe.
pub fn boost_current_rt_inherited() {
    driver::boost_current(boost_window());
}

/// `SYS_RT_ENTER`. Gated at the dispatch site on `Rights::RT`, not here — this
/// must stay callable from kernel init. That right is spec §9.4's privilege
/// gate, and it is endowed per manifest rather than won: what gated the band
/// before was a sound-device claim, which is not a privilege at all.
pub fn set_current_rt(enable: bool) {
    driver::set_current_rt(enable);
}

/// Block on a futex word unless it already changed. Returns whether it parked.
///
/// Registering before reading the word is the whole protocol: a `futex_wake`
/// that runs after the registration either claims the ticket or finds the
/// waiter parked, and one that ran before it stored the new value before the
/// registration — so the read below sees it.
///
/// **The word is named twice**, by the user address the caller passed and by
/// the physical address it translated to, because the token is the physical one
/// and nothing pins the frame behind it. `AddressSpace::unmap` ends every wait
/// armed on a frame it is giving back (`waitqs::revoke_futex_range`), and the
/// re-translation below is the other half of that fence — see the predicate.
#[track_caller]
pub fn futex_wait(
    addr: crate::UserAddr,
    phys_addr: DirectMap,
    expected: u32,
    deadline: Deadline,
) -> bool {
    let parkable = Parkable::of_current();
    // The value check is the predicate, and it runs *after* the arm — which is
    // the same ordering the registration gave it, and the reason the
    // wake-generation protocol this used to need is not coming back (§23's
    // rejection 3).
    //
    // **The translation is re-derived rather than trusted, and that is what
    // closes the window between the caller's translation and this arm.** An
    // `munmap` on a sibling CPU takes the address-space lock, clears the entry
    // and only then walks the futex buckets, so a translation that still names
    // the same frame was taken before that clear — and the revoke that follows
    // it is therefore guaranteed to find this arm. A translation that answers
    // anything else means the unmap already went past, this arm is one no post
    // will ever reach, and the load below would be through a frame the PMM has
    // reissued. It costs a per-CPU lookup, one leaf lock and a three-level walk
    // per *wake check* — a path that has already paid a park and a context
    // switch, and one an uncontended futex never enters at all.
    let read = || {
        let Some(pt) = current_address_space() else {
            return true;
        };
        let same_frame =
            pt.lock().translate(addr).is_some_and(|now| now.phys() == phys_addr.phys());
        if !same_frame {
            return true;
        }
        let word = unsafe { *phys_addr.as_ptr::<u32>() };
        word != expected
    };
    let _ = completion::wait_until(
        &parkable,
        completion::Subject::of(waitqs::futex_watch(phys_addr)),
        completion::Token::new(phys_addr.phys()),
        WaitClass::Futex,
        deadline,
        read,
    );
    true
}

/// Wake up to `count` waiters on this futex word, and answer how many.
///
/// **Both halves are the ABI's** (`toyos-abi/src/syscall.rs`'s `futex_wake`:
/// "wake up to `count` threads waiting on `addr`, returns number of threads
/// woken"), and the completion cutover briefly honoured neither: the count
/// went to a bucket queue nothing registered on any more — so the return was
/// provably always 0 — and the actual wake was an uncounted `post` that told
/// *every* waiter on the shared bucket, turning `pthread_cond_signal` into a
/// broadcast.
///
/// [`completion::post_n`] is the whole of the fix, and the token is why it
/// needs no second channel (§23's rejection 3 forbids one): the waiter arms
/// with its word's physical address, so the walk names the word and not the
/// 64-way bucket it hashes into. A waiter of a different word is not woken and
/// does not count against `count` — which is stronger than the queue this
/// replaces, where a shared bucket could spend a single wake on the wrong
/// waiter and leave the intended one parked.
pub fn futex_wake(phys_addr: DirectMap, count: usize) -> u64 {
    completion::post_n(
        completion::Subject::of(waitqs::futex_watch(phys_addr)),
        completion::Outcome::Ready,
        completion::Token::new(phys_addr.phys()),
        count,
    ) as u64
}

/// Retire a thread and wait until its record is gone.
///
/// The retire itself is one message (spec §7.6): the sticky kill bit plus
/// `Msg::Retire` to the CPU the state word names. **Whichever CPU ends up
/// owning the task then *schedules* it** — `specs/completion-architecture-spec.md`
/// §7.2 — because this kernel does not unwind and a discarded stack takes every
/// guard on it. A parked victim is woken into that CPU's dying list, a queued
/// one is moved into it, a running one is asked for a safe point, and one in
/// transit is adopted and dispatched. It dies by its own `die`, at the first
/// safe point its own unwind reaches. Nothing scans anything and nobody spins.
///
/// The *wait* is what the callers need and why this is not fire-and-forget:
/// process teardown frees memory the dead thread's page tables still map, so
/// it may not run until that thread's payload — kernel stack and address-space
/// reference — is dropped. That happens in `Hw::release`, which announces
/// itself here. Waiting for the state word to read `Dead` would be too weak,
/// and the reason survives §7.2 with a different mechanism behind it: `Dead` is
/// published by the victim's own `dispose_exit`, and the payload it leaves as
/// that CPU's zombie is freed by the **next** pass, because a pass cannot free
/// the stack it is standing on.
///
/// The short block deadline is a liveness backstop, not a poll: the wake is a
/// message like any other, and a lost one must fail loudly rather than hang.
#[track_caller]
pub fn retire_task(sched: &ThreadSched) {
    // Also on the early-return path below, where no park happens and the two
    // asserts inside the wait would never run.
    assert_baseline(BASELINE_TRAP);
    if let (Some(pid), Some(tid)) = (percpu::current_pid(), percpu::current_tid()) {
        if let Some(handle) = driver::current_shared() {
            assert!(
                !Arc::ptr_eq(&handle, &sched.shared),
                "retire_task: cannot retire self ({})",
                TaskId(pid, tid),
            );
        }
    }
    if sched.handle.released() {
        return;
    }
    preempt_off(|p| {
        toyos_sched::retire::begin(&sched.shared).post(cpus(), &HW, p);
    });
    /// How often the retirer looks again while it waits. A re-poll rate and
    /// not a bound: what actually ends this wait is the release wake, and this
    /// is the liveness backstop's step.
    const RECHECK: Cadence = Cadence::every(
        Duration::from_millis(50),
        "two hundred re-polls inside the tripwire, on a thread that is otherwise parked",
    );
    /// **Superseded in whole by `specs/scheduling-reservations-spec.md` §8, and
    /// kept until that design lands because a constant with a broken derivation
    /// is still the thing this kernel runs.** Two of the terms below are known
    /// wrong and neither is repairable by moving the number: the prologue count
    /// is an undercount by a factor the constant cannot absorb, and the
    /// real-time factor prices a deferral that is bounded per corpse rather than
    /// per CPU. Each says so where it is stated, rather than being re-derived
    /// into a form that fails the same way again.
    ///
    /// **Re-derived twice at C3+C4, and the second time because the first was
    /// below its own sum.** What this bounds is no longer "an IPI, one remote
    /// pass and a release": since `specs/completion-architecture-spec.md` §7.2
    /// the victim is *scheduled* rather than reaped, so the wait covers every
    /// hop between the claim and `Hw::release`. Term by term, from the tree:
    ///
    /// * **8 s — four pass prologues at 2 s each.** `sched::driver::pass` opens
    ///   with `drain_irqs()`, which calls `xhci::poll_if_pending()` *before* the
    ///   mailbox drain; below that poll sits `msc::bind`, a disk arriving after
    ///   boot, and `wait/mod.rs` names it "the one door, and the only blocking
    ///   thing a scheduler pass can still reach" — on `xhci::USB_TIMEOUT_NS` =
    ///   2,000,000,000 ns while holding `XHCI`. This is the term that made the
    ///   struck 1 s fire on the owner's T14 at 949 s of uptime with doom
    ///   exiting, and it has nothing to do with §7.2:
    ///   `specs/issues/kernel/scheduler-pass-blocks-in-xhci.md` is open and says
    ///   in terms that "`retire_task`'s bound is measuring the USB bus".
    ///
    ///   **Four named passes, and the count is an undercount rather than a
    ///   bound — superseded, not re-derived.** The named chain is: the pass the
    ///   retire's `Urgency::Preempt` kick buys, which drains `Msg::Retire`; the
    ///   pass that dispatches the corpse once the CPU is free of whatever was
    ///   running; the corpse's *own* exit pass, which is `exit_if_killed`'s
    ///   `driver::pass(Dispose::Exit)` and is a separate `pass()` call paying
    ///   the same prologue; and the pass that frees the zombie. But every chunk
    ///   boundary inside the unwind is itself a `pass()` call paying the same
    ///   prologue, and an instrumented count of one 10 ms unwind under this
    ///   crate's own driving loop found **twenty** — so under the premise this
    ///   bullet states, one corpse alone prices at 40 s and nine at 360 s, both
    ///   far above the constant. The other horn is no better: `poll_if_pending`
    ///   early-returns unless an xHCI interrupt is pending or port work is due,
    ///   and only `try_lock`s, so "every pass pays the prologue unconditionally"
    ///   is false as written and the term that dominates this number rests on
    ///   it. Neither horn is fixed by a larger constant, which is why
    ///   `specs/scheduling-reservations-spec.md` §8 declines to price this term
    ///   at all and names the pass, not the wait, as what has to change.
    /// * **20 ms — two quanta.** One for the victim's CPU to be free to pick
    ///   the dying task (a running fair task keeps the CPU to its quantum end),
    ///   one for the pass that releases the zombie to arrive.
    /// * **990 ms — the unwind, stretched by a saturated real-time band.** The
    ///   unwind itself is what §7.2 added: `?`-ing `completion::Cancelled` out
    ///   of every wait the thread was inside, dropping the guards on the way,
    ///   `process::teardown_resources`, and `ops::close_all` over up to
    ///   `MAX_HANDLES` = 4,096 handles. On *this* tree that is CPU-bounded work
    ///   and not a wait — `wait_transfer` still spins and there is no park on a
    ///   disk transfer until C7 (spec §20.4). **Its length is an estimate and
    ///   says so**: 4,096 closes plus a teardown, against a scheduler pass
    ///   budget (`toyos_sched::cpu::MAX_PASS_NS`) of 200 µs, is priced here at
    ///   one quantum — 10 ms of the victim's own CPU time.
    ///
    ///   The real-time band multiplies it by 11 — a factor whose derivation is
    ///   a `k = 1` argument, and **superseded rather than re-derived**.
    ///   `toyos_sched::cpu::DYING_AGE_NS` makes that band's precedence over the
    ///   dying list a deferral bounded *per corpse*: an aged corpse takes one
    ///   `DYING_CHUNK_NS` per `DYING_AGE_NS + DYING_CHUNK_NS`, so one 10 ms
    ///   unwind costs 110 ms of wall clock when the band never empties. What
    ///   that argument does not carry is the CPU: k aged corpses take k
    ///   consecutive chunks, and a band that briefly empties dispatches a corpse
    ///   with no grant and restamps it, throwing the accumulated age away. The
    ///   factor is therefore not a worst case in either direction.
    ///   `specs/scheduling-reservations-spec.md` §8 replaces it with a rate —
    ///   the dying server's own reservation — which reaches the same 110 ms and
    ///   reaches it for every k. **The struck derivation priced this term at
    ///   nothing** — "a machine that spends this tripwire on RT service is a
    ///   machine whose RT workload is the fault" — and that was not a
    ///   derivation, it was the reason the tripwire was reachable from one
    ///   spinning `Rights::RT` thread.
    ///
    ///   Times `1 + peers`, because one CPU runs one unwind at a time and this
    ///   victim waits out the corpses queued ahead of it. Priced at `peers = 8`.
    ///   **The provenance that number used to carry was impossible in this
    ///   kernel and is corrected here**: "one process's threads torn down
    ///   together onto one CPU" cannot happen, because `kill_process` and the
    ///   exit path both loop over a process's tids calling `retire_task`, which
    ///   blocks until the victim has been released — so one teardown holds at
    ///   most one corpse at a time. The producer of `peers > 0` is *concurrent
    ///   independent retirers*: separate killer threads retiring separate
    ///   victims that happen to share a CPU. Nothing bounds how many, which is
    ///   the whole of the filed defect
    ///   `specs/issues/kernel/retire-tripwire-is-not-queue-shaped.md`, and 8 is
    ///   a chosen number rather than a measured or derived one.
    ///
    /// 9.01 s of derived terms, and 10 s is the next round number above it —
    /// 990 ms of margin, which is one whole unwind's worth. **The margin buys
    /// nine further corpses at 110 ms each, and that is not the same quantity as
    /// the crossing point**: with 8.02 s of fixed terms the sum is
    /// 8.02 s + 0.110 s × N, which reaches the priced 9.01 s at N = 9 and first
    /// reaches the constant at N = 18. The two readings were conflated wherever
    /// this figure was repeated. The dominant term remains a filed defect and
    /// not a property of this wait: close the xHCI issue and 8 s of this number
    /// goes with it.
    ///
    /// **C7 owes this constant another look**: a sleep lock parked on a device
    /// puts a fifth `USB_TIMEOUT_NS` inside the unwind.
    const GIVE_UP: Tripwire = Tripwire::absurd(
        Duration::from_secs(10),
        "four pass prologues on xHCI's own 2 s deadline, two quanta, and an unwind \
         the real-time band may stretch elevenfold; past this the wake was lost",
    );
    let give_up = Deadline::at(crate::clock::now() + GIVE_UP.duration());
    let parkable = Parkable::of_current();
    // Armed on the victim, which is what `publish_released` posts to. The wait
    // is uncancellable (§7.4): a killed retirer cannot propagate a cancel with
    // the retire half done, and what bounds it is the tripwire above.
    let Some(armed) = completion::arm(
        completion::Subject::of(sched.handle.watch()),
        completion::Token::new(sched.shared.key().0),
        WaitClass::Other,
    ) else {
        panic!("retire_task: no current task to park");
    };
    while !sched.handle.released() {
        if give_up.reached(crate::clock::now()) {
            panic!(
                "retire_task: task not released after {}: {:?}",
                GIVE_UP.duration(),
                sched.shared.state()
            );
        }
        let _record = completion::wait_uncancellable(
            &parkable,
            &armed,
            Deadline::at(crate::clock::now() + RECHECK.duration()),
        );
    }
}

/// Per-CPU hand-off slot for a thread that died in panic recovery. The panic
/// path may hold any lock, so it may do nothing but store here; the idle loop
/// is the thread's only cleanup site.
static POISONED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];

/// Whether [`reap_poisoned`] has anything to do. Raised by both sites that make
/// work for it — a thread poisoned below, and a process publishing its exit
/// ([`crate::object::process::ProcessObject::publish_exit`], which is what makes
/// a table entry collectable) — and claimed by whichever idle trip takes the
/// work. `sched::reap_gate` carries the argument.
static REAP_GATE: ReapGate = ReapGate::new();

/// Tell the idle loop there is a table entry to collect.
///
/// Called *after* the object's `finished` flag is stored, so the gate's release
/// is what publishes it to the reaper.
pub fn note_reapable() {
    REAP_GATE.raise();
}

pub fn poison_tid(id: TaskId) {
    let cpu = percpu::cpu_id() as usize;
    let Some(slot) = POISONED.get(cpu) else {
        crate::log!("poison_tid: cpu {cpu} >= MAX_CPUS — {id} will never be reaped");
        return;
    };
    let prev = slot.swap(id.pack(), Ordering::Release);
    // After the slot is written, never before: the gate's release is what
    // carries it to the CPU that claims the work.
    REAP_GATE.raise();
    if prev != u64::MAX {
        crate::log!(
            "poison_tid: cpu {cpu} slot still held {} — its waiter is stranded",
            TaskId::unpack(prev)
        );
    }
}

/// Zombify threads that died in panic recovery, collect the entries of
/// processes that have published their exit, and wake whoever was joining them.
/// Called from the idle loop, which is the one context that provably holds none
/// of the locks the panicking thread may have been holding.
///
/// **Nothing to reap costs no lock.** This took `PROCESS_TABLE` unconditionally
/// until 2026-08-14, so every CPU with nothing to run held it for a slice of
/// every trip round the idle loop — against a crash report whose
/// `process::with_user_symbols` may only `try_lock` that table, and which
/// therefore lost the faulting function's name whenever the two met. The gate
/// is the whole of the fix on this side; `sched::reap_gate` argues why a raise
/// cannot be lost, and `process::with_user_symbols` documents what the reader
/// now says when it loses anyway.
pub(crate) fn reap_poisoned() {
    if !REAP_GATE.take() {
        return;
    }
    let mut wakes: [Option<process::PoisonWake>; MAX_CPUS] = [const { None }; MAX_CPUS];
    // Both are dropped after the guard: an entry's drop reaches
    // `remove_vruntime`, and a process whose teardown never ran still holds its
    // whole `ProcessData` here.
    let reaped;
    {
        let mut guard = process::PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        reaped = process::reap_finished(table, unsafe { process::IdleProof::new_unchecked() });
        for (slot, wake) in POISONED.iter().zip(wakes.iter_mut()) {
            let raw = slot.load(Ordering::Relaxed);
            if raw == u64::MAX {
                continue;
            }
            let id = TaskId::unpack(raw);
            *wake = process::zombify_poisoned(table, id.0, id.1);
            slot.store(u64::MAX, Ordering::Relaxed);
        }
    }
    drop(reaped);
    for wake in wakes.into_iter().flatten() {
        match wake {
            // The thread that died is the subject a joiner armed on.
            process::PoisonWake::Joiner(pid, tid) => {
                if let Some(sched) = process::thread_sched(pid, tid) {
                    completion::post(
                        completion::Subject::of(sched.handle.watch()),
                        completion::Outcome::Gone(completion::Reason::Closed),
                    );
                }
            }
            // The code a killed process gets: nobody asked for this exit, and
            // the accounting the teardown would have taken was never taken.
            process::PoisonWake::Process(object) => {
                let stats = toyos_abi::syscall::ProcessStats {
                    pid: object.pid().raw(),
                    ..Default::default()
                };
                object.publish_exit(crate::object::process::Exit { code: -1, stats })
            }
        }
    }
}

/// The panic path's exit: the faulted thread's context is unusable, so it dies
/// where it stands. Its record becomes this CPU's zombie and is released by the
/// next pass, which by then runs on another stack.
///
/// The one switching entry with no §6.4 baseline assert, deliberately: a
/// panicking thread may hold any lock — that is the situation, not a bug to
/// trip over — and measurement finds this entry at both baselines. Asserting
/// here would turn every panic-with-a-lock into a double panic and lose the
/// report. The dying context's depth leaves with it, since `Hw::switch` loads
/// the incoming context's own.
pub fn schedule_no_return() -> ! {
    if in_schedule_self() {
        crate::log!("schedule_no_return: panicked inside a pass, cannot rejoin");
        crate::arch::apic::halt_all_cpus();
    }
    if percpu::current_tid().is_none() {
        enter_idle_loop();
    }
    driver::pass(Dispose::Exit);
    unreachable!("schedule_no_return: returned from the exit pass");
}

/// Cumulative CPU time for a thread, published by its owning CPU at each end of
/// a pass (see `TaskHandle`). A running thread's live slice is added by the
/// reader, so the number does not stand still between passes.
pub fn task_cpu_ns(sched: &ThreadSched) -> u64 {
    sched.handle.cpu_ns()
}

pub fn task_sched_state(sched: &ThreadSched) -> u8 {
    sched.sched_state()
}

/// Flush the running thread's blocked/runqueue counters into process
/// accounting. Reads the live task's own record — a local access, which is the
/// only kind a `!Sync` `CpuSched` admits.
pub fn flush_current_stats(acct: &mut process::ProcessAccounting) {
    driver::with_current_acct(|a| crate::sched::payload::merge_accounting(a, acct));
}

/// How often an idle CPU may say what it is holding, and how often the machine
/// may say what it has allocated.
///
/// One number for both because they are one kind of thing: a periodic snapshot
/// of occupancy, taken from the idle loop, by a machine whose only channel may
/// be a log file on the stick it booted from. The occupancy of the run queues
/// and the occupancy of the page pools are read together or not at all.
///
/// A [`Cadence`] — how often a thing may be re-done, and what makes that rate
/// affordable — and *not* a deadline. It rate-limits an opportunistic check on
/// a CPU that is already awake; converting it into something a CPU is woken for
/// would add a wake to a machine with nothing to run, which is an audio change.
const SNAPSHOT_INTERVAL: Cadence = Cadence::every(
    Duration::from_secs(10),
    "one clock read and one relaxed compare per idle trip, on a CPU already awake",
);

/// `sched-fast-health`'s cadence, and the same kind of thing at a rate no
/// shipped machine pays: the actuator exists because telling a CPU that spins
/// through idle from one that halts cleanly needs two prints to compare — the
/// `trips=` counter inside each line is not itself rate-limited, only the print
/// carrying it is — and no guest test program this suite runs lives past
/// [`SNAPSHOT_INTERVAL`] once, let alone the two prints a comparison needs.
const FAST_SNAPSHOT_INTERVAL: Cadence = Cadence::every(
    Duration::from_millis(200),
    "an actuator no boot arms; a test that needs two prints buys them for one boot",
);

/// Which of the two cadences this boot took, read once per idle trip.
fn snapshot_interval_ns() -> u64 {
    if crate::actuator::sched_fast_health() {
        FAST_SNAPSHOT_INTERVAL.nanos()
    } else {
        SNAPSHOT_INTERVAL.nanos()
    }
}

/// When each CPU may next print its own line. Per CPU rather than global: which
/// CPUs reach idle is most of what the line says, and one global deadline would
/// let whichever CPU won the race speak for all of them.
static NEXT_HEALTH: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// How many times each CPU has passed through idle since boot, counted on
/// every trip rather than only the ones that print.
///
/// This is the counter the rate limit above took out of the log line, kept
/// but not silenced: incrementing it costs one relaxed `fetch_add`, nothing
/// like the print it used to gate, so it carries no part of the feedback loop
/// `log_health`'s doc comment describes. What it restores is the signal
/// `i8042_quarantine` needs — "a keyboard, not a CPU" is a claim about
/// whether a CPU is halting, and a rate-limited *count of lines* cannot tell
/// a CPU that halts between rare wakes from one that spins between rare
/// prints: both produce one line per [`snapshot_interval_ns`], because the
/// print is what the rate limit throttles. The number inside the line is
/// what still moves at two different speeds — this one.
static IDLE_TRIPS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// A snapshot of this CPU's run queues, at most once per
/// [`snapshot_interval_ns`], plus the machine's page pools on the same cadence.
///
/// Called from the idle loop on every trip, and the cadence is a wall clock
/// because a trip is not a unit of time. It used to be one line per 1000 trips
/// of a counter shared by every CPU, and a CPU that declines to sleep — the log
/// ring owes bytes, an xHCI port is inside its debounce — goes round that loop
/// at memory speed: measured on the USB profiles, bursts of one line every 3–4
/// ms from each of two CPUs, about 320 lines a second, 292 of them in one run
/// of the xHCI family. Worse than the volume is the feedback: every line is
/// bytes the ring owes, and bytes the ring owes is one of the conditions that
/// stops the CPU sleeping. The line was printing because the machine was awake
/// and keeping it awake by printing.
///
/// The line is kept rather than deleted because `parked` is not readable
/// anywhere else without a message round trip (`dump_blocked` reaches only the
/// calling CPU, and only on a keystroke), and on the machine with no serial
/// port an occasional occupancy line in `kernel.log` is the only account of the
/// scheduler there is. What it must not be read as is a heartbeat: it comes
/// from a CPU passing through idle, so a quiet machine prints nothing and a gap
/// is not evidence.
pub fn log_health() {
    let now = crate::hw::now_ns();
    let cpu = percpu::cpu_id();
    let Some(next_health) = NEXT_HEALTH.get(cpu as usize) else { return };
    // Unconditional and every trip, unlike the print below: this is the part
    // of the old per-trip counter that is safe to keep unthrottled, because
    // nothing downstream of it runs more often for having moved.
    let trips = IDLE_TRIPS
        .get(cpu as usize)
        .map_or(0, |t| t.fetch_add(1, Ordering::Relaxed) + 1);
    if now >= next_health.load(Ordering::Relaxed) {
        next_health.store(now + snapshot_interval_ns(), Ordering::Relaxed);
        let ready = driver::ready_len() + usize::from(percpu::current_tid().is_some());
        let parked = driver::parked_len();
        // **`dying` is on the line because this line is the whole account.** On
        // the machine with no serial port an occasional occupancy line in
        // `kernel.log` is the only thing that says where the scheduler's tasks
        // are, and a container it does not name is one nobody can ask about
        // — `sched::dump` needs a keystroke, which that machine has nowhere to
        // send.
        let dying = driver::dying_len();
        crate::log!(
            "sched: cpu={} ready={} dying={} parked={} current={:?} trips={}",
            cpu,
            ready,
            dying,
            parked,
            percpu::current_tid(),
            trips,
        );
    }

    static NEXT_PMM_DUMP: AtomicU64 = AtomicU64::new(0);
    let next = NEXT_PMM_DUMP.load(Ordering::Relaxed);
    if next == 0 {
        NEXT_PMM_DUMP.store(now + snapshot_interval_ns(), Ordering::Relaxed);
    } else if now >= next
        && NEXT_PMM_DUMP
            .compare_exchange(
                next,
                now + snapshot_interval_ns(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
    {
        crate::mm::pmm::dump_stats();
    }
}


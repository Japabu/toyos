//! `KernelHw` — the kernel's side of the scheduler-core hardware boundary
//! (spec §10.1).
//!
//! Everything here is x2APIC, TSC or a single instruction. Nothing here
//! decides anything: no queue is consulted, no state machine advances, no
//! ordering-sensitive protocol lives below this line. That is the whole
//! contract — the simulator replaces this file and nothing else.

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
        unsafe {
            asm!("pushfq", "pop {}", "cli", out(reg) rflags, options(nomem));
        }
        Self { rflags }
    }
}

impl Drop for IrqGuard {
    fn drop(&mut self) {
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
/// One relaxed store per switch, and the whole of what it buys is the line
/// `switch_frame_is_wrong` prints: **is a sibling standing on this same
/// context right now.** That question is the difference between a report and a
/// diagnosis — nothing else the crash path can reach answers it, because a
/// `CpuSched` is `!Sync` and a sibling's is unreadable by construction.
static RUNNING_CTX: [core::sync::atomic::AtomicU64; crate::sched::MAX_CPUS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; crate::sched::MAX_CPUS];

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
    let me = ctx as *const KernelCtx as u64;
    for (cpu, slot) in RUNNING_CTX.iter().enumerate().take(crate::arch::smp::cpu_count() as usize)
    {
        let held = slot.load(core::sync::atomic::Ordering::Relaxed);
        crate::log!(
            "  cpu{cpu} is on ctx {held:#x}{}",
            if held == me { "  <== THE SAME CONTEXT" } else { "" }
        );
    }
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
    panic!("context_switch: the restored frame's return address is not kernel text");
}

/// See [`switch_frame_is_wrong`]. Kept tiny so the hot path is a load, a
/// compare and a not-taken branch.
#[inline]
fn check_switch_frame(ctx: &KernelCtx, token: &RunToken<KernelPayload>) {
    let rsp = ctx.rsp;
    if !crate::mm::is_kernel_addr(rsp) || !rsp.is_multiple_of(8) {
        switch_frame_is_wrong(ctx, token);
    }
    // SAFETY: `rsp` is a kernel address eight bytes below the top of the
    // incoming task's own kernel stack at the shallowest, so the return slot is
    // mapped.
    let ret = unsafe { core::ptr::read_volatile((rsp + 56) as *const u64) };
    if !crate::mm::is_kernel_addr(ret) {
        switch_frame_is_wrong(ctx, token);
    }
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
        unsafe {
            (*save).fs_base = cpu::rdfsbase();
            (*save).preempt = crate::preempt::count();
            let incoming: &KernelCtx = &*restore;
            check_switch_frame(incoming, &token);
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
            let rsp = incoming.rsp;
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

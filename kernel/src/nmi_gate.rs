//! An NMI delivered into the window where CPL is 0 and `rsp` is not.
//!
//! **The one machine state `arch::idt`'s IST2 row exists for, and the one
//! nothing outside the guest can stage.** `SYSCALL` switches no stack, so three
//! instructions of `arch::syscall`'s entry and one of its exit run at CPL 0 with
//! the user's `rsp`; an exception taken there builds its frame on a user page
//! from CPL 0, SMAP refuses the write, and the `#PF` escalates to `#DF`. Which
//! instruction an asynchronous interrupt lands on is decided inside the guest by
//! the guest's own timing — there is no QEMU device, machine property or monitor
//! command that aims one — so the actuator is the only instrument, and it fakes
//! nothing: another CPU really sends the NMI, the victim really takes it
//! wherever it is, and what is counted is where the CPU's own frame says it was.
//!
//! Three counters per CPU, and the classification is the defect's own signature
//! rather than a symbol range:
//!
//! - **window** — a Ring 0 frame whose saved `rsp` is a user address. The only
//!   code in this kernel that runs that way is the entry window and the
//!   `pop rsp`/`sysretq` pair, so this counts arrivals *in* the window without
//!   knowing where either one begins.
//! - **ring3** — the frame was Ring 3, which is the same victim's user loop and
//!   is what the expected window count is derived against (`tests/common/faults.rs`).
//! - **ring0** — everything else, which on an aimed storm is the victim inside
//!   the syscall it was making.
//!
//! **Which accelerator is running the guest decides whether the window is
//! reachable at all, and only one of them can.** Under TCG, QEMU checks for a
//! pending interrupt between translation blocks and `syscall` ends one, so an
//! NMI pending across it is delivered at `syscall_entry+0` — the dev host reads
//! 36 to 47 window arrivals per 3,000. Under KVM an NMI to a running vCPU is a
//! host kick, a VM exit and an injection at the next VM entry, and that entry is
//! never one of those three instructions: **0 of 6,000 on the hosted lane**
//! (run 32584121311, two boots, 2,451 and 438 of the same NMIs arriving in
//! Ring 3, so the aim was right and the delivery point is simply elsewhere).
//! That is a fact about the instrument. What KVM still witnesses is the machine
//! taking 3,000 aimed NMIs with IST2 in place and going on working, and what
//! proves the *window* on both is the `nmi-without-ist` control's `#DF`.
//!
//! **The storm is triggered by the victim and aimed at it, and neither used to
//! be true.** It armed at three seconds of wall clock and sprayed every sibling;
//! the wall clock is two clocks with no handshake — on a loaded shard the
//! spinner started later than the instant, the storm fired at an idle machine,
//! and the one shot was spent (run 32582884567, 134 s of a test watching a
//! machine that had already answered). A CPU's syscall count is the victim's own
//! signal that it is spinning, and it is also which CPU to aim at.
//!
//! `nmi-nested` is the second arm and it stages the *other* hazard: an NMI
//! handler that returns early through `iretq` un-masks NMIs while still standing
//! on IST2 (SDM Vol. 3A §6.7.1). It is staged rather than asserted — a real
//! pending NMI, a real `iretq`, a real second entry on the same stack — so what
//! it measures is whether `nmi_entry`'s re-entrancy check fires or whether the
//! frame is quietly overwritten.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::{apic, percpu};
use crate::sched::MAX_CPUS;

static SEEN: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Syscalls each CPU has taken, which is how the storm finds its victim.
static SYSCALLS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static WINDOW: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static RING3: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Where the first window arrival was, for the report to symbolize.
static FIRST_WINDOW_RIP: AtomicU64 = AtomicU64::new(0);

/// Called from `arch::idt::nmi`'s handler, on every NMI, before anything else.
///
/// Three relaxed adds and a compare. It may not do more: it runs on IST2 with
/// the rules that module's header states — no lock, no allocation, nothing that
/// can fault — and `log!` in particular is what the whole facility exists to
/// stay out of.
pub fn observe(rip: u64, cs: u64, rsp: u64) {
    if !crate::actuator::syscall_window_nmi() {
        return;
    }
    let me = percpu::cpu_id() as usize;
    if me >= MAX_CPUS {
        return;
    }
    // Release, and the sender's load is Acquire: the storm below paces itself on
    // this word, so it is a handshake and not only a statistic.
    SEEN[me].fetch_add(1, Ordering::Release);
    if toyos_userbound::Ring::of_cs(cs).is_user() {
        RING3[me].fetch_add(1, Ordering::Relaxed);
        return;
    }
    if !crate::mm::is_kernel_addr(rsp) {
        WINDOW[me].fetch_add(1, Ordering::Relaxed);
        let _ = FIRST_WINDOW_RIP.compare_exchange(
            0,
            rip,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }
}

/// Un-mask NMIs from inside the NMI handler, with one already pending.
///
/// **`nmi-nested`'s whole content, and it is the hazard rather than a verdict.**
/// The architecture blocks NMI delivery until the handler's `iretq`, so the only
/// way a second one can enter on IST2 is an `iretq` executed early — which is
/// what a handler that faults gets for free, and what Linux's nested-NMI
/// machinery is built around. Here it is written out: send this CPU an NMI,
/// which latches while blocked, then `iretq` to the next instruction of this
/// same function, which clears the block. The second NMI enters immediately,
/// with `nmi_active` still raised.
///
/// One shot per boot. The state it stages ends in a halt either way — either the
/// entry's check fires, or the outer frame is gone and nothing is coming back.
pub fn stage_nested_if_armed() {
    if !crate::actuator::nmi_nested() {
        return;
    }
    static STAGED: AtomicBool = AtomicBool::new(false);
    if STAGED.swap(true, Ordering::AcqRel) {
        return;
    }
    apic::send_nmi(percpu::cpu_id());
    // SAFETY: irreducible — "return through `iretq` without leaving the
    // function" is not expressible in Rust, and it is the whole of what this
    // stages. The frame is built from this CPU's own `ss`, `rsp`, `RFLAGS` and
    // `cs`, and the `rip` is the label below, so the `iretq` lands on the next
    // instruction with `rsp` exactly where it was: control flow and the stack
    // are unchanged, and the one observable effect is the NMI block clearing.
    // No `nomem`/`nostack`: the block pushes five words and the delivery it
    // admits may observe any memory.
    unsafe {
        core::arch::asm!(
            "mov {tmp}, rsp",
            "xor {seg:e}, {seg:e}",
            "mov {seg:x}, ss",
            "push {seg}",
            "push {tmp}",
            "pushfq",
            "mov {seg:x}, cs",
            "push {seg}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "iretq",
            "2:",
            tmp = out(reg) _,
            seg = out(reg) _,
        );
    }
}

/// Syscalls one CPU must have taken before the storm believes a victim is
/// spinning on that CPU.
///
/// **The trigger, and it is the victim's own work rather than a clock.** The
/// storm used to arm at a wall-clock instant three seconds into the boot, on the
/// assumption that the harness had started the spinner by then — two clocks with
/// no handshake between them. On a loaded shard the spinner started *after* that
/// instant, so the storm fired at an idle machine, reported nothing, and left
/// `FIRED` set: `syscall_window_nmi` sat for 134 s watching a machine that had
/// already done the one thing it was waiting for (run 32582884567). This
/// counter is what closes it — a CPU that has taken a million syscalls is a CPU
/// with a program on it in the entry window as often as a program can be, and no
/// daemon on an idle machine reaches it.
///
/// The spinner measured 5.3 million syscalls a second on the hosted lane
/// (53,150,000 in 10.008 s, 188 ns each, run 32582884567), so this is reached
/// about 190 ms after it starts and cannot be reached before it does.
const SPINNING_SYSCALLS: u64 = 1_000_000;

/// How many NMIs the storm will send before giving up on the window.
///
/// A ceiling and not a schedule: the loop stops at [`ENOUGH`] arrivals, and this
/// is what bounds a boot where the victim stopped spinning halfway through.
const MAX_NMIS: u64 = 3_000;

/// Window arrivals that end the storm. More than one, because one is a fact and
/// a rate is a measurement; few enough that a healthy boot spends milliseconds
/// here.
const ENOUGH: u64 = 64;

/// How long one NMI gets to be taken before the next goes out. An NMI needs
/// nothing of the target but the interrupt itself — `sched::dump`'s probe budgets
/// a millisecond for one — and this only paces the storm: an NMI that misses it
/// is counted as sent and shows up in the difference.
const DELIVERY_BUDGET_NS: u64 = 100_000;

/// One syscall on this CPU, counted only while the actuator is armed.
///
/// Called from `arch::syscall::syscall_dispatch`, which is every syscall in the
/// machine, so it is a relaxed load and a predictable branch and nothing else —
/// and in a shipping kernel this module does not exist and the call is not
/// compiled at all.
pub fn note_syscall() {
    if !crate::actuator::syscall_window_nmi() {
        return;
    }
    let me = percpu::cpu_id() as usize;
    if me < MAX_CPUS {
        SYSCALLS[me].fetch_add(1, Ordering::Relaxed);
    }
}

/// The sibling CPU with the most syscalls behind it, and how many.
///
/// Recomputed every round rather than fixed at the start: the scheduler may move
/// the spinner, and a storm aimed at where it used to be is a storm at an idle
/// CPU.
fn victim(me: usize, cpus: usize) -> Option<(usize, u64)> {
    SYSCALLS
        .iter()
        .enumerate()
        .take(cpus)
        .filter(|&(cpu, _)| cpu != me)
        .map(|(cpu, n)| (cpu, n.load(Ordering::Relaxed)))
        .max_by_key(|&(_, n)| n)
}

/// Storm the CPU that is spinning on `syscall` and report where the NMIs landed.
///
/// Called from the idle loop, once, by whichever CPU reaches it first **while a
/// sibling is already inside the window as often as a program can be**. The idle
/// loop rather than a pass, for `dump::deaf_window`'s reason: the storming CPU
/// has no task and the CPU under observation is the one that does — and
/// whichever CPU, not cpu0, because the scheduler decides where the spinner
/// runs. `syscall-window-nmi` implies `diag-tick` so that a quiet CPU keeps
/// reaching the loop rather than sleeping through the whole run.
///
/// **The arming condition is the victim's own syscall count and not a clock**
/// ([`SPINNING_SYSCALLS`] carries what that cost when it was a clock), so the
/// storm cannot fire before there is something to storm — and `FIRED` is swapped
/// only once that is true, which is what keeps a premature look from consuming
/// the one shot.
pub fn storm() {
    static FIRED: AtomicBool = AtomicBool::new(false);

    let cpus = (crate::arch::smp::cpu_count() as usize).min(MAX_CPUS);
    let me = percpu::cpu_id() as usize;
    if cpus < 2 {
        return;
    }
    match victim(me, cpus) {
        Some((_, taken)) if taken >= SPINNING_SYSCALLS => {}
        _ => return,
    }
    if FIRED.swap(true, Ordering::AcqRel) {
        return;
    }

    // The victim's own syscall count either side of the storm. **This is how a
    // reader knows the victim went on running Ring 3 code through all of it**,
    // and it is the one witness that costs nothing: waiting for the spinner's
    // own last line would cost its whole spin on every run, and a delivered
    // count alone cannot tell a CPU that kept working from one that stopped
    // after the first NMI.
    let spun_before: u64 = SYSCALLS.iter().map(|n| n.load(Ordering::Relaxed)).sum();

    let mut sent = 0u64;
    while sent < MAX_NMIS {
        // Aimed, not broadcast: every NMI that goes to an idle sibling is a
        // sample of the idle loop, and what is being measured is a window three
        // instructions wide on the CPU that is executing it.
        let Some((cpu, _)) = victim(me, cpus) else { break };
        let seen = &SEEN[cpu];
        let before = seen.load(Ordering::Acquire);
        apic::send_nmi(cpu as u32);
        sent += 1;
        // Wait for it to be *taken* before sending the next. Two NMIs in flight
        // at one CPU are one NMI plus one latched, so an unpaced storm measures
        // the APIC's latch rather than the victim's timing.
        let deadline = crate::clock::nanos_since_boot().saturating_add(DELIVERY_BUDGET_NS);
        while seen.load(Ordering::Acquire) == before
            && crate::clock::nanos_since_boot() < deadline
        {
            core::hint::spin_loop();
        }
        if total(&WINDOW) >= ENOUGH {
            break;
        }
    }

    let spun: u64 = SYSCALLS
        .iter()
        .map(|n| n.load(Ordering::Relaxed))
        .sum::<u64>()
        .saturating_sub(spun_before);
    report(sent, spun, cpus);
}

fn total(counter: &[AtomicU64; MAX_CPUS]) -> u64 {
    counter.iter().map(|c| c.load(Ordering::Relaxed)).sum()
}

/// One line the gate reads, plus one per CPU that saw anything.
///
/// Every count is written after the word that names it, so the reader takes the
/// field from the word and not from a position in the line — and the total is
/// **last**, after the per-CPU lines and after the symbolized `rip`, because it
/// is what a reader waits for: a drain that ends on the summary has everything
/// under it already.
fn report(sent: u64, spun: u64, cpus: usize) {
    for cpu in 0..cpus {
        let seen = SEEN[cpu].load(Ordering::Relaxed);
        if seen == 0 {
            continue;
        }
        let window = WINDOW[cpu].load(Ordering::Relaxed);
        let ring3 = RING3[cpu].load(Ordering::Relaxed);
        log!(
            "syscall-window-nmi: cpu={cpu} seen={seen} window={window} ring3={ring3} ring0={}",
            seen - window - ring3,
        );
    }
    let rip = FIRST_WINDOW_RIP.load(Ordering::Relaxed);
    if rip != 0 {
        log!("syscall-window-nmi: the first window arrival was here:");
        crate::symbols::resolve_kernel(rip);
    }
    let seen = total(&SEEN);
    let window = total(&WINDOW);
    let ring3 = total(&RING3);
    log!(
        "syscall-window-nmi: sent={sent} seen={seen} window={window} ring3={ring3} ring0={} \
         spun={spun}",
        seen - window - ring3,
    );
}

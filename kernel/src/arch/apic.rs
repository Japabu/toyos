use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::{cpu, percpu};
use crate::log;

// x2APIC MSR addresses (base 0x800 + xAPIC_offset >> 4)
const IA32_APIC_BASE_MSR: u32 = 0x1B;
const X2APIC_ID: u32 = 0x802;
const X2APIC_SVR: u32 = 0x80F;
const X2APIC_EOI: u32 = 0x80B;
const X2APIC_ICR: u32 = 0x830;
const X2APIC_LVT_TIMER: u32 = 0x832;
const X2APIC_TIMER_INIT: u32 = 0x838;
const X2APIC_TIMER_CURRENT: u32 = 0x839;
const X2APIC_TIMER_DIVIDE: u32 = 0x83E;

pub const TIMER_VECTOR: u8 = 0x20;

/// Calibrated LAPIC timer ticks per 10ms (computed on BSP, reused by APs).
static TIMER_TICKS: AtomicU32 = AtomicU32::new(0);

/// Set once during BSP init. Guards IPI sends before APIC is ready.
static X2APIC_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable x2APIC mode on this CPU. Sets global enable (bit 11) + x2APIC (bit 10)
/// in IA32_APIC_BASE, then software-enables via SVR.
fn enable_x2apic() {
    let mut base = cpu::rdmsr(IA32_APIC_BASE_MSR);
    base |= (1 << 11) | (1 << 10);
    cpu::wrmsr(IA32_APIC_BASE_MSR, base);

    let svr = cpu::rdmsr(X2APIC_SVR);
    cpu::wrmsr(X2APIC_SVR, svr | (1 << 8) | 0xFF);
}

/// Initialize the BSP's Local APIC in x2APIC mode.
pub fn init() {
    enable_x2apic();
    X2APIC_ENABLED.store(true, Ordering::Release);
    log!("LAPIC: x2APIC enabled (ID {})", id());
}

/// Enable the AP's local APIC in x2APIC mode.
pub fn init_ap() {
    enable_x2apic();
}

pub fn id() -> u32 {
    cpu::rdmsr(X2APIC_ID) as u32
}

/// Send INIT IPI to the specified APIC ID.
pub fn send_init(apic_id: u32) {
    // ICR: destination in high 32 bits, 0x4500 = delivery INIT, level assert
    cpu::wrmsr(X2APIC_ICR, ((apic_id as u64) << 32) | 0x4500);
}

/// Send Startup IPI (SIPI) with the given vector (trampoline page number).
pub fn send_sipi(apic_id: u32, vector: u8) {
    cpu::wrmsr(X2APIC_ICR, ((apic_id as u64) << 32) | 0x4600 | vector as u64);
}

/// Send EOI.
#[inline]
pub fn eoi() {
    cpu::wrmsr(X2APIC_EOI, 0);
}

/// Send an IPI to **this** CPU (self shorthand), for the one caller that needs
/// an interrupt whose delivery time is decided by `IF` alone.
///
/// `log-nested-emit` (§9.2) is that caller: it sends this from inside `emit`,
/// and the whole verdict is *when* it lands — after the publication bracket
/// with §2.3a's guard, inside the body copy without it. No device interrupt can
/// serve, because nothing about a device's timing is under a test's control.
#[cfg(feature = "boot-actuators")]
pub fn send_self(vector: u8) {
    if !X2APIC_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    // Destination shorthand = self (0b01 << 18), fixed delivery, level assert.
    cpu::wrmsr(X2APIC_ICR, 0x0004_4000 | vector as u64);
}

/// Send an IPI to all CPUs except self (shorthand destination).
fn ipi_all_excluding_self(vector: u8) {
    // destination shorthand = all-excluding-self (0b11 << 18), fixed delivery
    cpu::wrmsr(X2APIC_ICR, 0x000C_0000 | vector as u64);
}

/// Ask every other CPU to flush its TLB. The *asking* only — `arch::tlb` owns
/// the protocol that turns it into an answer, and nothing outside that module
/// may send this vector: a flush request nobody waits for is exactly the defect
/// M3 removed, and a second sender would reintroduce it one call at a time.
pub(super) fn tlb_ipi() {
    if X2APIC_ENABLED.load(Ordering::Relaxed) {
        ipi_all_excluding_self(0xFE);
    }
}

/// Send the timer-vector IPI to one CPU, waking it if halted. Targeted
/// x2APIC ICR write (destination APIC id in the high 32 bits, fixed
/// delivery, level assert) — broadcast kicks preempt every sibling per
/// wake and cannot scale.
pub fn kick_cpu(cpu_id: u32) {
    if !X2APIC_ENABLED.load(Ordering::Relaxed) { return; }
    let apic_id = crate::arch::smp::apic_id_for(cpu_id);
    cpu::wrmsr(X2APIC_ICR, ((apic_id as u64) << 32) | 0x4000 | TIMER_VECTOR as u64);
}

/// Send an NMI to one CPU. Same targeted write as [`kick_cpu`] with delivery
/// mode NMI (0x400) instead of a vector, and it exists for the one question
/// [`kick_cpu`] cannot answer: a CPU that spins with interrupts disabled never
/// takes a kick, so a kick that goes unanswered does not distinguish "wedged"
/// from "not listening". An NMI is not maskable by `IF`, so it does.
///
/// Diagnostic only — `sched::dump` sends it to a CPU that has already failed to
/// answer a kick. Nothing on a working path may use it: an NMI can land between
/// any two instructions, including inside a critical section this kernel has no
/// way to make NMI-safe.
pub fn send_nmi(cpu_id: u32) {
    if !X2APIC_ENABLED.load(Ordering::Relaxed) { return; }
    let apic_id = crate::arch::smp::apic_id_for(cpu_id);
    cpu::wrmsr(X2APIC_ICR, ((apic_id as u64) << 32) | 0x4400);
}

/// How long a dying machine gives `/bin/logd` to put the report on `/log`.
///
/// **Re-derived at L6, because what it bounds changed kind and not only size.**
/// It used to bound the idle loop, which goes round in microseconds, doing one
/// FAT append and a sync itself. What it bounds now is a *userland process*
/// being scheduled: a wake reaching whichever CPU logd is parked on, its
/// `SYS_LOG_READ`, a page-cache write-back, a FAT append, the device's own
/// cache flush, and a second `SYS_LOG_READ` to publish `durable`. Every one of
/// those is a step the old number did not cover.
///
/// It stays at half a second, and the reason is what the number is *for* rather
/// than what it now contains: it is not a prediction of how long the write
/// takes, it is what a machine with nobody left to do the writing pays on its
/// way down. Half a second against the ~460 ms the panel paint costs on the T14
/// anyway, and a machine whose logd is alive and schedulable finishes far
/// inside it. `screen_fatal_halt_composited`'s `/log` half is what says so on
/// every run.
const LOG_FILE_DRAIN_NANOS: u64 = 500_000_000;

/// Does `/log` still owe this boot the report?
///
/// **One predicate now, where it was two.** The kernel's own file sink is gone
/// (§8.1), and with it the pair of questions — "has the cursor moved" and "is a
/// flush between the ring and the device" — that had to be asked together
/// because the first went false in the middle of the second. `LOG_DURABLE_NS`
/// has no such gap by construction: `/bin/logd` publishes it *after* the
/// `fsync` returns, so the word going past a record's timestamp means that
/// record is on the stick and not that somebody has started putting it there.
fn owed(want: u64) -> bool {
    crate::log::user::durable_ns() < want
}

/// Give `/bin/logd` a chance to put this report on the stick before the machine
/// stops.
///
/// **The panic path writes nothing itself, and that is the design.** A
/// panic-time write would need the VFS lock, the file cache lock, the kernel
/// heap, the volume's device lock and the xHCI lock, and a panicking thread may
/// hold any of them. `try_lock` does not rescue it either: a spinlock's
/// `try_lock` fails for its own holder, so the cases where the report matters
/// most are exactly the ones it would decline, and the heap is not try-able at
/// all. That is "sometimes writes and sometimes hangs", which is worse than the
/// panel alone. **After L6 it is not even available**: the writer is a userland
/// process and the kernel has no path to `/log` to take.
///
/// What this does instead takes no lock, allocates nothing and touches no
/// device: **it waits.** The report is already committed in the shards, the halt
/// IPI has not gone out yet, and `emit` has already posted `klogd`'s wake — so
/// `klogd` drains, posts the log's readiness source, and `/bin/logd` is made
/// runnable by the ordinary mechanism rather than by anything on this path. The
/// dying CPU spins on one relaxed atomic against a deadline and lets the rest of
/// the machine do the writing.
///
/// It cannot deadlock and it cannot make a panic worse: [`owed`] is a load, the
/// deadline is absolute, and every outcome ends in the same `halt_all_cpus` tail
/// that ran before. A machine where nothing can write — no scheduler able to
/// pick logd, a logd that died earlier in the boot, a logd that has given up on
/// the volume, no `/log` at all — pays the bound and halts with the panel as the
/// only copy and a line saying so. Those are §6.6's cases and §6.6's subject:
/// they are what a pstore would cover and this cannot.
///
/// Placed before the halt IPI rather than after, because after it there is
/// nobody left to do the writing.
fn wait_for_log_file() {
    // **Only where the panel is the only channel.** This exists because a T14
    // has no serial port, so the file is the sole copy of a fatal report. A
    // machine with a working console has already got that report off the box
    // through `panic_flush`, and making it wait here buys a duplicate at the
    // price of delaying every step below it — `render`, the drain, and
    // `page_forever`.
    //
    // The price is measured, not assumed: without this guard `screen_pager_keys`
    // failed alone and reproducibly with `0 page moves over 30 keystrokes`,
    // because the pager had not started by the time the host began injecting.
    // A diagnostic that perturbs the path it is diagnosing is worth less than
    // the delay it costs, and on a machine with serial it is worth nothing.
    if crate::drivers::serial::has_console() {
        return;
    }
    // **What is waited for is sampled once, here.** The report's own records
    // are already committed — `panic_console::capture` ran before this — so the
    // newest committed timestamp *is* the report's, and taking it once means a
    // sibling that keeps logging on its way down cannot extend this wait
    // indefinitely by moving the target.
    let want = crate::log::read::newest_committed_at_ns();
    if !owed(want) {
        return;
    }
    // Wake them first. A sibling with nothing to run is sitting in `sti; hlt`,
    // so waiting for the machine to schedule logd waits for something that is
    // not happening — the LAPIC timer is one-shot and a quiet machine may have
    // none armed. This is the ordinary wake IPI, the same one a scheduler wake
    // sends, and the halt IPI has not gone out yet, so there is still somebody
    // to wake.
    let cpus = crate::arch::smp::cpu_count();
    let me = percpu::cpu_id();
    for cpu in 0..cpus {
        if cpu != me {
            kick_cpu(cpu);
        }
    }
    let deadline = crate::clock::nanos_since_boot().saturating_add(LOG_FILE_DRAIN_NANOS);
    while owed(want) {
        if crate::clock::nanos_since_boot() >= deadline {
            // Reaches the panel only on the fatal paths that paint the live
            // ring rather than a snapshot taken before this ran, and reaches
            // serial on a machine that has one. On a T14 mid-panic it is the
            // honest record for whoever reads the *next* boot's log and finds
            // no report in this one.
            crate::log!("panic: the report did not reach /log in {LOG_FILE_DRAIN_NANOS}ns; the panel is the only copy");
            return;
        }
        core::hint::spin_loop();
    }
}

/// Halt all CPUs. Sends halt IPI to all other CPUs, then flushes any
/// pending log output to the serial backend, then halts self.
///
/// The flush bypasses both the log-ring lock and the serial backend lock
/// (`panic_flush`): the halt IPI is an ordinary maskable fixed-delivery
/// vector, so a sibling with IF=1 may halt mid-critical-section still
/// holding either lock, and a sibling spinning with IF=0 (both locks are
/// cli-guarded) only halts once it re-enables interrupts. Taking the locks
/// normally could therefore deadlock; `panic_flush` first waits out live
/// holders, then bypasses the wedged ones.
pub fn halt_all_cpus() -> ! {
    wait_for_log_file();
    if X2APIC_ENABLED.load(Ordering::Relaxed) {
        cpu::wrmsr(X2APIC_ICR, 0x000C_0000 | 0xFD);
    }
    // Before the flush, not after. Rendering consumes nothing and has no
    // unbounded loop, so putting it ahead of the proven channel costs the
    // screen and never the serial report if it goes wrong — and it means a
    // line arriving on serial proves the paint already finished.
    let painted = crate::drivers::panic_console::render();
    unsafe { crate::drivers::serial::panic_flush(); }
    // After the flush, because the flush is the deepest this path ever goes:
    // it is where the drain buffer lives, and a check placed before it would
    // certify a stack depth the report had not yet reached. No-op unless this
    // CPU is on IST1.
    percpu::ist1_report();
    // And the pager strictly after it, for the same reason inverted: it *is*
    // an unbounded loop, so it may only run once the serial report is out.
    // Only the CPU that painted enters it; every other one halts below.
    if painted {
        crate::drivers::panic_console::page_forever();
    }
    super::cpu::halt();
}

/// Calibrate the LAPIC timer on the BSP. Requires HPET.
/// Does not start the timer — the scheduler arms one-shot timers on demand.
pub fn init_timer() {
    // Divide by 1 for maximum resolution
    cpu::wrmsr(X2APIC_TIMER_DIVIDE, 0b1011);

    // Masked one-shot mode for calibration
    cpu::wrmsr(X2APIC_LVT_TIMER, 1 << 16);
    cpu::wrmsr(X2APIC_TIMER_INIT, 0xFFFF_FFFF);

    let start = crate::clock::nanos_since_boot();
    while crate::clock::nanos_since_boot() - start < 10_000_000 {}
    let elapsed = crate::clock::nanos_since_boot() - start;

    let remaining = cpu::rdmsr(X2APIC_TIMER_CURRENT) as u32;
    let ticks_elapsed = 0xFFFF_FFFFu32.wrapping_sub(remaining);
    let ticks_10ms = (ticks_elapsed as u64 * 10_000_000 / elapsed) as u32;

    cpu::wrmsr(X2APIC_TIMER_INIT, 0);
    TIMER_TICKS.store(ticks_10ms, Ordering::Release);
    // Fallback for any Ring 0 fire before the scheduler arms its first quantum.
    let percpu = unsafe { &*percpu::percpu_ptr() };
    percpu.last_armed_ticks.store(OneShot::ticks(ticks_10ms as u64).0, Ordering::Relaxed);
    log!("LAPIC timer: {} ticks/10ms", ticks_10ms);
}

/// AP timer init — calibration was done on the BSP, nothing to start.
pub fn init_timer_ap() {}

/// The shortest interval this kernel will ask the one-shot for, and therefore
/// the resolution of every deadline in the system.
///
/// **A CPU that takes the timer interrupt again before it can retire the
/// instruction after the one that armed it never retires anything again.** The
/// Ring 0 stub reloads whatever was last armed with no Rust in the path, so a
/// count too small to outlast the interrupt it schedules is not one late tick
/// but a livelock nothing recomputes its way out of. Ring 3 can ask for one — a
/// deadline already past when the pass arms it — and did (`#156`, observed on
/// the owner's T14).
///
/// Policy, not physics, and it sits between two bounds. Below: an interrupt
/// entry and `iretq`, which is what the interval has to be worth more than for
/// the interrupted code to get any of the CPU at all. Above: `QUANTUM_NS`,
/// which this is a thousandth of, so no scheduling decision can feel it.
const MIN_ONE_SHOT_NS: u64 = 10_000;

/// A count the CPU can make progress under, and the only thing that reaches
/// `X2APIC_TIMER_INIT` or `last_armed_ticks`.
///
/// The floor is in the constructor rather than at the three call sites because
/// the assembly stub reloads the remembered count with no Rust in the path: an
/// arm that could not be survived is a state this module has to be unable to
/// name, not one each caller has to remember not to ask for.
struct OneShot(u32);

impl OneShot {
    fn ticks(ticks: u64) -> Self {
        let per_10ms = TIMER_TICKS.load(Ordering::Relaxed) as u64;
        let floor = (MIN_ONE_SHOT_NS * per_10ms / 10_000_000).max(1);
        // Zero is the register's "stopped", so it is `stop_timer`'s word and
        // never a count — `min` alone would let a calibration this small write
        // it. `u32::MAX` is the register.
        Self(ticks.clamp(floor, u32::MAX as u64) as u32)
    }

    fn after(nanos: u64) -> Self {
        let per_10ms = TIMER_TICKS.load(Ordering::Relaxed) as u128;
        Self::ticks((nanos as u128 * per_10ms / 10_000_000) as u64)
    }

    /// Program it, and remember it for the Ring 0 stub's reload.
    fn arm(self) {
        cpu::wrmsr(X2APIC_TIMER_DIVIDE, 0b1011);
        // An AP reaches its first idle before it has ever run a task, so before
        // this register has ever been written — and an LVT resets masked.
        cpu::wrmsr(X2APIC_LVT_TIMER, TIMER_VECTOR as u64);
        let percpu = unsafe { &*percpu::percpu_ptr() };
        percpu.last_armed_ticks.store(self.0, Ordering::Relaxed);
        cpu::wrmsr(X2APIC_TIMER_INIT, self.0 as u64);
    }
}

/// Arm a one-shot timer to fire after `nanos` nanoseconds, or after
/// [`MIN_ONE_SHOT_NS`] if that is longer.
pub fn arm_one_shot(nanos: u64) {
    OneShot::after(nanos).arm();
    crate::trace::trace(crate::trace::Kind::TimerArm, nanos as u32);
}

/// Shorten this CPU's armed interval to at most `nanos`, arming it if the last
/// pass left it stopped.
///
/// The minimum against what is already armed is what keeps this close to a pure
/// addition: a parked task's deadline is never pushed out by more than
/// [`MIN_ONE_SHOT_NS`], and all the scheduler ever sees is extra passes — which
/// it already tolerates, since a kick IPI is one.
///
/// Traces nothing, unlike [`arm_one_shot`]: no scheduler deadline is being set
/// here, and a `TimerArm` record would make the trace say one was.
#[cfg(feature = "boot-actuators")]
pub fn arm_within(nanos: u64) {
    let want = OneShot::after(nanos);
    // A running count never reaches zero without the fire that reloads it, so
    // zero is `stop_timer` and not an expiry an instant away.
    let remaining = cpu::rdmsr(X2APIC_TIMER_CURRENT) as u32;
    let ticks = if remaining == 0 { want.0 } else { want.0.min(remaining) };
    OneShot::ticks(ticks as u64).arm();
}

/// Stop the timer. No more interrupts until re-armed.
///
/// Zero in both places, which is the hardware's word for a stopped counter and
/// the Ring 0 stub's for "do not re-arm" — the one count that is not a
/// [`OneShot`], because it is not an interval.
pub fn stop_timer() {
    let percpu = unsafe { &*percpu::percpu_ptr() };
    percpu.last_armed_ticks.store(0, Ordering::Relaxed);
    cpu::wrmsr(X2APIC_TIMER_INIT, 0);
    crate::trace::trace(crate::trace::Kind::TimerStop, 0);
}

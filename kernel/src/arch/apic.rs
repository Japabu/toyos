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

/// Send an IPI to all CPUs except self (shorthand destination).
fn ipi_all_excluding_self(vector: u8) {
    // destination shorthand = all-excluding-self (0b11 << 18), fixed delivery
    cpu::wrmsr(X2APIC_ICR, 0x000C_0000 | vector as u64);
}

/// Flush TLB on all other CPUs. No-op if x2APIC not yet initialized.
pub fn tlb_shootdown() {
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

/// How long a dying machine gives its siblings to put the report on `/log`.
///
/// The idle loop goes round in microseconds and a flush is one FAT append plus
/// a sync, so a machine with a healthy CPU left finishes far inside this; the
/// bound is what a machine with none pays, once, on its way down. Half a second
/// against the ~460 ms the panel paint costs on the T14 anyway.
const LOG_FILE_DRAIN_NANOS: u64 = 500_000_000;

/// Does the log volume still owe this boot bytes?
///
/// **Both halves, and the second is not belt-and-braces.** The ring's own
/// predicate goes false at `drain_to_file`, which is before `flush_file` and
/// `sync_mount` have put anything on the device — so waiting on it alone
/// returns mid-write and the halt IPI then stops the CPU doing the writing.
/// Measured: the wait was satisfied, no timeout line was printed, and the
/// report was still absent from the file.
fn owed() -> bool {
    crate::drivers::log_ring::file_has_pending() || crate::log_file::flush_in_progress()
}

/// Give the log sink a chance to put this report on the stick before the
/// machine stops.
///
/// **The panic path still writes nothing itself, and that is the design.**
/// `log_file`'s own module doc rules a panic-time flush out on locks alone —
/// it would need this module's lock, the VFS lock, the file cache lock, the
/// kernel heap, the volume's device lock and the xHCI lock, and a panicking
/// thread may hold any of them. `try_lock` does not rescue it either: a
/// spinlock's `try_lock` fails for its own holder, so the cases where the
/// report matters most are exactly the cases it would decline, and the heap is
/// not try-able at all. That is "sometimes writes and sometimes hangs", which
/// is worse than the panel alone.
///
/// What this does instead takes no lock, allocates nothing and touches no
/// device: **it waits.** The report is already in the log ring, the halt IPI has
/// not gone out yet, and every other CPU's idle loop is still running
/// `log_file::poll`, which is the ordinary, proven path to the stick. So the
/// dying CPU spins on one relaxed atomic against a deadline and lets a healthy
/// sibling do the write.
///
/// It cannot deadlock and it cannot make a panic worse: `file_has_pending` is a
/// load, the deadline is absolute, and every outcome ends in the same
/// `halt_all_cpus` tail that ran before. A machine where no CPU can flush —
/// the VFS lock stranded, the sink disabled, no `/log` at all — pays the bound
/// and halts exactly as it used to, with the panel as the only copy and a line
/// saying so.
///
/// Placed before the halt IPI rather than after, because after it there is
/// nobody left to do the writing.
fn wait_for_log_file() {
    if !owed() {
        return;
    }
    // Wake them first. A sibling with nothing to run is sitting in `sti; hlt`
    // and is not going round its idle loop at all, so waiting for it to flush
    // waits for something that is not happening — the LAPIC timer is one-shot
    // and a quiet machine may have none armed. This is the ordinary wake IPI,
    // the same one a scheduler wake sends, and the halt IPI has not gone out
    // yet, so there is still somebody to wake.
    let cpus = crate::arch::smp::cpu_count();
    let me = percpu::cpu_id();
    for cpu in 0..cpus {
        if cpu != me {
            kick_cpu(cpu);
        }
    }
    let deadline = crate::clock::nanos_since_boot().saturating_add(LOG_FILE_DRAIN_NANOS);
    while owed() {
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
    percpu.last_armed_ticks.store(ticks_10ms, Ordering::Relaxed);
    log!("LAPIC timer: {} ticks/10ms", ticks_10ms);
}

/// AP timer init — calibration was done on the BSP, nothing to start.
pub fn init_timer_ap() {}

/// Arm a one-shot timer to fire after `nanos` nanoseconds.
pub fn arm_one_shot(nanos: u64) {
    let ticks_10ms = TIMER_TICKS.load(Ordering::Relaxed) as u64;
    let ticks = (nanos as u128 * ticks_10ms as u128 / 10_000_000) as u64;
    let ticks = ticks.clamp(1, u32::MAX as u64) as u32;
    cpu::wrmsr(X2APIC_TIMER_DIVIDE, 0b1011);
    cpu::wrmsr(X2APIC_LVT_TIMER, TIMER_VECTOR as u64);
    let percpu = unsafe { &*percpu::percpu_ptr() };
    percpu.last_armed_ticks.store(ticks, Ordering::Relaxed);
    cpu::wrmsr(X2APIC_TIMER_INIT, ticks as u64);
    crate::trace::trace(crate::trace::Kind::TimerArm, nanos as u32);
}

/// Stop the timer. No more interrupts until re-armed.
pub fn stop_timer() {
    let percpu = unsafe { &*percpu::percpu_ptr() };
    percpu.last_armed_ticks.store(0, Ordering::Relaxed);
    cpu::wrmsr(X2APIC_TIMER_INIT, 0);
    crate::trace::trace(crate::trace::Kind::TimerStop, 0);
}

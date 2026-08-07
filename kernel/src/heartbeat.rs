//! One line every quarter second saying the machine is still running, and
//! which CPUs still reach a scheduler pass.
//!
//! # The gap this closes
//!
//! A T14 that freezes at boot leaves a log **byte-identical** to a healthy
//! one's. Ten logs off the owner's stick: seven of 214 lines and three of 216,
//! the three long ones differing only by two `compositor: frames=` reports, and
//! every log's last kernel line at ~1.33 s whether that boot lived or died.
//! Nothing that fails leaves a trace on its way down, and nothing that survives
//! leaves one either — a live idle desktop and a dead machine produce the same
//! file, because the last thing either has to say is the same.
//!
//! So the log cannot answer *was this boot alive at time T*, and every
//! conclusion about the freeze has had to come from a photograph of a panel or
//! from `metal-panic-probe` firing once at claim + 5 s. This makes the answer
//! continuous: a frozen boot's log ends at the last heartbeat before it died,
//! which is the **time** of death rather than only the fact of it.
//!
//! # What the per-CPU field means, exactly
//!
//! `passes=` counts the CPUs that reached [`note_pass`] since the previous
//! heartbeat, and `mask=` names them. That is **"reached a scheduler pass"**,
//! not "is alive": the LAPIC timer is one-shot, so an idle CPU with nothing
//! parked on it can legitimately sit halted across several heartbeats and
//! contribute nothing. A healthy machine therefore prints varying counts, and
//! the reading that matters is the shape of the *end* — whether the mask thins
//! out CPU by CPU over several heartbeats, which is a local cause spreading, or
//! goes from full to nothing between two lines, which is a global one.
//!
//! # Where it runs, and why it is the idle loop
//!
//! [`poll`] is called from `sched::driver::idle_loop`, immediately before
//! `log_file::poll`, so the line it appends is flushed by the very next
//! statement through the path that already exists. No second flush mechanism,
//! and nothing here takes a lock: a heartbeat that could block would be a
//! diagnostic that stops for the reason it exists to report.
//!
//! The consequence is worth stating rather than hiding: **a heartbeat that
//! stops means no CPU reached the idle loop**, which on a machine saturated
//! with runnable work is not the same as death. On the machine this is for it
//! is the same — that desktop composites twice per two-second window and its
//! CPUs are in the idle loop the rest of the time, and `metal-panic-probe`
//! fired from exactly there at 6.164 s on the boot that lived. It is the same
//! predicate the probe answered once, asked four times a second.
//!
//! # Cost
//!
//! Four lines a second, about 60 bytes each. Against `log_file`'s 1 MiB
//! rotation that is a new part roughly every 70 minutes and sixteen of them
//! kept, so a diagnostic session of any length anyone will sit through fits.
//! **That is a diagnostic budget and not a shipping one**, which is why this is
//! a feature and not a default: a machine nobody is watching should not spend
//! its log volume saying nothing happened.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::percpu;
use crate::sched::MAX_CPUS;

/// How often a line is emitted. Short enough that the time of death is
/// localised to a quarter second, long enough that the log is readable and the
/// flush path is not the load.
const PERIOD_NS: u64 = 250_000_000;

/// When the last line was emitted, and the reference point every CPU's stamp is
/// compared against. 0 until the first heartbeat.
static LAST_AT: AtomicU64 = AtomicU64::new(0);

/// Per-CPU: when this CPU last reached a scheduler pass. Never reset — the
/// comparison is against [`LAST_AT`], so a CPU that stops updating simply stops
/// appearing in the mask, which is the signal.
static TICKED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// This CPU reached a scheduler pass. Called from `drain_irqs`, which is the
/// top of every pass on every CPU.
///
/// One relaxed store of a clock read the caller's own path is about to take
/// anyway. It is deliberately *not* in the timer ISR: what the freeze violates
/// is reaching a pass, and an interrupt taken by a CPU that then fails to
/// schedule would report that CPU as healthy.
pub fn note_pass() {
    let cpu = percpu::cpu_id() as usize;
    if cpu < MAX_CPUS {
        TICKED[cpu].store(crate::clock::nanos_since_boot().max(1), Ordering::Relaxed);
    }
}

/// Emit a heartbeat if one is due. Called from the idle loop, immediately
/// before the log sink's own poll.
pub fn poll() {
    let now = crate::clock::nanos_since_boot();
    let last = LAST_AT.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < PERIOD_NS {
        return;
    }
    // One CPU per period. A CAS rather than a store, because on eight CPUs
    // several reach this in the same microsecond and eight identical lines a
    // period would bury the field they are printed for.
    if LAST_AT
        .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    let mut mask = 0u64;
    let mut count = 0u32;
    for (cpu, stamp) in TICKED.iter().enumerate() {
        // `>= last` and not `> last`: the first heartbeat has `last == 0` and
        // every CPU that has ever run qualifies, which is what makes line one
        // a baseline rather than an empty mask.
        if stamp.load(Ordering::Relaxed) >= last {
            mask |= 1 << cpu;
            count += 1;
        }
    }
    log!(
        "heartbeat: t={}.{:03}s passes={}/{} mask={:#04x}",
        now / 1_000_000_000,
        (now % 1_000_000_000) / 1_000_000,
        count,
        crate::arch::smp::cpu_count().min(MAX_CPUS as u32),
        mask
    );
}

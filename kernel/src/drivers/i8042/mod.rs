//! The i8042 PS/2 controller: the ThinkPad's built-in keyboard, and (from
//! the next commit) its TrackPoint.
//!
//! **Init treats the machine as untrusted.** Firmware and an embedded
//! controller are not kernel code; CLAUDE.md's corollary applies literally.
//! Every wait is bounded against the wall clock, nothing panics, and a
//! controller that does not answer costs the keyboard and never the boot.
//! Each failure is one short line, because on a machine with no UART those
//! lines are read off the next boot checkpoint's repaint of the log tail.
//!
//! **The ISR reads the device, which no other ISR here does.** Every other
//! device has a DMA ring its consumer can re-derive from, so those handlers
//! only timestamp. The i8042 has a one-byte output buffer and will not
//! assert another edge until it is read, so draining in the ISR is the only
//! correct shape rather than a shortcut. That makes the prohibitions below
//! load-bearing rather than stylistic:
//!
//! - **No `Lock`.** `Lock::lock` disables preemption but not interrupts
//!   (`sync.rs`), so an ISR taking a lock a thread on the same CPU holds
//!   self-deadlocks. The handler touches neither `PS2`, nor the key/mouse
//!   queues, nor the I/O APIC. The prohibition binds every *other* ISR too:
//!   `drain` holds those locks in thread context, so any handler that reached
//!   them — the timer tick's device poll was the one that could — wedges the
//!   CPU rather than this one misbehaving.
//! - **No allocation.** `VecDeque::push_back` reaches the allocator, and a
//!   panic holding the allocator lock wedges the recovered CPU.
//! - **No `log!`.** It is ISR-safe, and it is still banned: at key-repeat
//!   rates it is noise, and the ring lock is a same-CPU spin for nothing.
//! - **No wake.** Waking enters the scheduler and possibly sends an IPI.
//! - **No unbounded loop.** A controller with OBF stuck high would spin a
//!   CPU with IF=0 forever; hence `ISR_BURST` and the quarantine.
//!
//! This module imports neither `alloc` nor `sync::Lock` into the ISR's
//! reach: the byte ring is a static of atomics, and everything that needs a
//! lock lives behind `service`, which runs in thread context only.
//!
//! **Delivery is pinned to one CPU** (`IRQ_CPU`, physical destination), which
//! is what makes the ISR the sole reader of port 0x60 and the byte ring a
//! genuine single-producer queue. Two CPUs taking these interrupts would
//! race on a one-byte register. Input is ~100 Hz; there is no load argument
//! for spreading it.
//!
//! The corollary binds the *drain*, which runs on whichever CPU entered the
//! scheduler: any polled port I/O it wants to do — the aux re-enable is the
//! only one — has to happen on `IRQ_CPU` with interrupts off there. Nothing
//! weaker works. Masking the redirection entries does not: it stops neither an
//! ISR already executing nor a vector already latched in that CPU's LAPIC, and
//! an edge asserted on a masked edge-triggered entry is dropped outright.

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

use toyos_ps2::{KeyDecoder, KeyOutcome, MouseDecoder, MouseOutcome};

use crate::arch::cpu::{inb, outb};
use crate::arch::idt::I8042_VECTOR;
use crate::irq_ring::IrqSource;
use crate::log;
use crate::sync::Lock;
use crate::time::{Budget, Cadence, Duration};
use super::ioapic::{self, Gsi};

mod tally;

use tally::{Carried, Tally};

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
const COMMAND: u16 = 0x64;

const OBF: u8 = 1 << 0;
const IBF: u8 = 1 << 1;
const AUXB: u8 = 1 << 5;

/// `IAPC_BOOT_ARCH` bit 1, "the motherboard has a port 60/64 keyboard
/// controller" (ACPI 6.5 table 5.10; ACPICA calls it `ACPI_FADT_8042`).
const FADT_8042: u16 = 1 << 1;

/// A port no device decodes floats high, so `0xff` is every status bit set at
/// once: both buffers full, both error flags, the keyboard simultaneously
/// locked and transmitting. No controller produces that, and a machine without
/// one produces nothing else however long it is asked — so this is the value
/// every wait below gives up on rather than waits out. A deadline alone is not
/// enough: it makes a machine with no controller spend the whole init budget in
/// the first wait, and since the probe no longer asks firmware's permission,
/// that machine is now reachable.
const FLOATING_BUS: u8 = 0xFF;

const CMD_READ_CONFIG: u8 = 0x20;
const CMD_WRITE_CONFIG: u8 = 0x60;
const CMD_DISABLE_AUX: u8 = 0xA7;
const CMD_ENABLE_AUX: u8 = 0xA8;
const CMD_TEST_AUX: u8 = 0xA9;
const CMD_SELF_TEST: u8 = 0xAA;
const CMD_TEST_PORT1: u8 = 0xAB;
const CMD_DISABLE_PORT1: u8 = 0xAD;
const CMD_ENABLE_PORT1: u8 = 0xAE;
const CMD_WRITE_AUX: u8 = 0xD4;

const CFG_PORT1_IRQ: u8 = 1 << 0;
const CFG_PORT2_IRQ: u8 = 1 << 1;
const CFG_PORT1_CLOCK_OFF: u8 = 1 << 4;
const CFG_PORT2_CLOCK_OFF: u8 = 1 << 5;
const CFG_TRANSLATE: u8 = 1 << 6;

const ISA_IRQ_KEYBOARD: u8 = 1;
const ISA_IRQ_AUX: u8 = 12;

/// The largest legitimate burst is a 3-byte mouse packet plus a 4-byte
/// extended key sequence. Anything past this is a controller that is not
/// going to stop, and the drain masks its line rather than let it hold a CPU.
const ISR_BURST: usize = 16;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static QUARANTINE: AtomicBool = AtomicBool::new(false);
static KBD_EVENTS: AtomicU32 = AtomicU32::new(0);
static AUX_EVENTS: AtomicU32 = AtomicU32::new(0);
static LOST_EDGES: AtomicU32 = AtomicU32::new(0);
static DROPPED: AtomicU32 = AtomicU32::new(0);
static DROPPED_TOTAL: AtomicU32 = AtomicU32::new(0);
/// Pointer bytes the framer threw away at a packet boundary — how far a desync
/// ran, and the one number that separates a mis-framed pointer from a silent
/// one. A healthy stream discards nothing at all (`toyos-ps2`'s
/// `a_healthy_stream_discards_nothing_and_leaves_no_byte_unaccounted`), so any
/// value here is a real hole in the byte stream and not a resting TrackPoint.
static DISCARDS: AtomicU32 = AtomicU32::new(0);
/// Set-1 overrun codes. A keyboard telling us it lost bytes is a different
/// failure from a keyboard we cannot decode, and neither was counted.
static OVERRUNS: AtomicU32 = AtomicU32::new(0);
static KEYBOARD_GSI: AtomicU32 = AtomicU32::new(u32::MAX);
static AUX_GSI: AtomicU32 = AtomicU32::new(u32::MAX);

/// Every interrupt the pin has delivered, split by what the ISR found behind
/// it. The ISR's only bookkeeping, and the one number that answers the question
/// `init`'s success line cannot: that line says the driver armed the line, not
/// that anything ever came back over it.
///
/// Entries and not bytes, deliberately. `handler_poll` puts bytes in the ring
/// from `init` with interrupts off, so a byte count cannot tell a delivered
/// interrupt from a byte that was already sitting in the output buffer — which
/// is exactly the confusion this counter exists to remove.
///
/// One word rather than two counters, and `tally.rs` is the whole argument:
/// "did anything arrive to decode" used to be a subtraction of two numbers the
/// ISR wrote at either end of its burst, so a reader inside the burst answered
/// it wrongly.
static TALLY: Tally = Tally::new();

/// Bytes taken off the ring, counted in [`pop`] before the byte leaves it.
///
/// **Where it is counted is load-bearing**, and after the drain was where it
/// used to be. The mute verdict's `has_bytes` guard defers the report to the
/// pass that holds the byte; a byte popped and not yet added here is in neither
/// place, so a report from another CPU during the decode said `0 bytes` about a
/// byte that had arrived and was in front of a decoder at that moment. Counted
/// in `pop`, a delivered byte is in the ring or in this number at every instant
/// and the guard means what it says.
static RX_BYTES: AtomicU32 = AtomicU32::new(0);

/// When the pin first asserted. Set in the handler, which is the only place it
/// and the tally can be made to agree — the health line says "first seen" and
/// there is now more than one line, so reading the clock where the line is
/// written dates the second one to when it was printed rather than to the event
/// it reports. Published by the tally's release, so a reader that sees the
/// interrupt counted sees the stamp it belongs to. Never written by
/// `handler_poll`: those bytes came from a poll and no pin asserted for them.
static FIRST_IRQ_NS: AtomicU64 = AtomicU64::new(0);

/// When the pin last asserted. What makes the periodic line's verdict exact
/// however coarse its period is: the line says when the last byte arrived, not
/// when somebody got round to looking.
static LAST_IRQ_NS: AtomicU64 = AtomicU64::new(0);

/// The CPU the vector is pinned to. Two things are only true there: the ISR is
/// the sole reader of port 0x60, and an `irq_ring` record for this source can
/// exist at all (records are strictly per-CPU). Both are load-bearing below.
static IRQ_CPU: AtomicU32 = AtomicU32::new(u32::MAX);

fn is_irq_cpu() -> bool {
    IRQ_CPU.load(Ordering::Relaxed) == crate::arch::percpu::cpu_id()
}

// The health verdict.
//
// `init` reports what it did; nothing reported what happened afterwards. A
// driver that armed the pin and then never received a byte said one green line
// at 0.1 s and nothing ever again, which on a machine whose only channel is the
// panel is indistinguishable from a driver that is working and a user who has
// not typed. Both transitions out of that state are now stated out loud.
//
// Neither is a poll, and neither can be silently dropped:
//
// - The **first interrupt** is reported by the pass that interrupt schedules.
//   It cannot be missed, because the event being reported is what causes the
//   report to run.
// - The **quiet verdict** is reported from the first pass that finds a CPU with
//   nothing left to run (`verdict_due`, wired into the idle loop's pre-halt
//   check). A wall-clock deadline was the obvious alternative and it is the
//   wrong one: `service` only runs inside a scheduler pass, so on the machine
//   this exists for — a diagnostic boot that reaches `Boot: complete` and then
//   has nothing to do — no pass would run to notice the deadline and the line
//   would simply never appear. "The machine has gone still" is also the moment
//   the statement first means anything: before it, "nothing has arrived" only
//   says the boot is still busy.
//
// The cost is one relaxed load per scheduler pass and one per idle entry, both
// of which fall to a single load-and-compare for the life of the boot once the
// verdict is out.

/// `init` never armed the pin, so there is nothing to say.
const HEALTH_OFF: u8 = 0;
/// Armed and watching.
const HEALTH_ARMED: u8 = 1;
/// A CPU has run out of work; the quiet verdict is owed and one more pass will
/// emit it.
const HEALTH_QUIET_DUE: u8 = 2;
/// The quiet verdict is out. Still watching, because an interrupt that arrives
/// afterwards — the owner finally pressing a key — is the answer.
const HEALTH_QUIET_SAID: u8 = 3;
/// The boot verdict is out; from here the counters speak for themselves.
const HEALTH_DONE: u8 = 4;
/// Bytes arrived and decoded to nothing. Said once, and still watching: the
/// byte that completes a sequence is one interrupt away, and a first line
/// reading `1 bytes, 0 keys` on a keyboard that is in fact working must not be
/// the last word.
const HEALTH_MUTE_SAID: u8 = 5;
/// The pin has asserted and **no interrupt has carried a byte** ([`EMPTY_IRQS`]
/// accounts for all of them). A state of its own rather than a variant of
/// [`HEALTH_MUTE_SAID`], because the mute verdict is still owed: the first byte
/// that does arrive and decodes to nothing has to be reported, and a state that
/// had already said "nothing decoded" would swallow it.
const HEALTH_EMPTY_SAID: u8 = 6;

static HEALTH: AtomicU8 = AtomicU8::new(HEALTH_OFF);
static ARMED_NS: AtomicU64 = AtomicU64::new(0);

// The counters, after the verdict.
//
// The verdict was the last word the driver ever said. The ThinkPad T14 lost its
// keyboard, TrackPoint and touchpad 6.6 s into a session — all three are behind
// this controller — and the log's final `i8042:` line is the verdict, printed
// 15 ms before the input died and never revised for the remaining 54 s. What
// that cost is not a missing line. It is that nothing in the log can separate
// **the pin stopped asserting** from **bytes kept arriving and decoded to
// nothing**: opposite defects, in opposite subsystems, and the counters that
// tell them apart were read once and never again.
//
// So the counters repeat. Two properties make that affordable and make the
// answer unambiguous:
//
// - **Only when the pin has asserted since the last line.** A machine nobody is
//   touching says nothing, forever, for two relaxed loads per scheduler pass.
// - **Therefore silence is evidence.** Past the first repeat, no line means no
//   interrupt — not a driver that stopped looking. That is the whole point, and
//   it is only true because the report cannot be skipped for any other reason.
//
// The first repeat is guaranteed rather than gated on a change, because it is
// the one that dates the last byte: `REPORTED_IRQS` starts at 0 and the verdict
// deliberately does not seed it.
//
// No `to_screen`: a repaint is a screenful of framebuffer writes and the ring
// is the primary channel. The panel keeps the verdict, which is the line a
// person standing in front of a dead machine needs.
//
// The period is policy. 10 s is the PMM dump's cadence, which is what the log
// already costs a reader per idle minute, and it bounds the line to one per
// 10 s of *typing* — an idle machine pays nothing.
fn health_period_ns() -> u64 {
    /// A log-rate limit, which is what §3.4's widened `Cadence` covers: what
    /// makes the rate affordable is that an idle machine pays nothing at all.
    const HEALTH: Cadence = Cadence::every(
        Duration::from_secs(10),
        "the PMM dump's own cadence, and one line per 10s of typing",
    );
    if crate::actuator::i8042_fast_health() {
        Duration::from_millis(500).nanos()
    } else {
        HEALTH.nanos()
    }
}
static NEXT_REPORT_NS: AtomicU64 = AtomicU64::new(u64::MAX);
static REPORTED_IRQS: AtomicU32 = AtomicU32::new(0);

/// Bytes the drain did not turn into an event, oldest first.
///
/// `N bytes, 0 keys` is a true statement that names no suspect. 84 of the 256
/// single byte values decode to nothing under set 1, and `handle_key` drops a
/// break for a usage that was not held, so the arithmetic alone cannot separate
/// a keyboard that reset behind our back (`0xAA`, which is left Shift's break
/// under translation) from a late ack (`0xFA`), or from a wire carrying raw
/// set 2, where Enter is `0x5A` and Backspace `0x66` and 23 such codes land on
/// unmapped slots. The byte tells them apart and nothing else does. The aux
/// flag rides along because a lone pointer byte frames no packet and is
/// equally invisible in `0 motion`.
///
/// **A byte a decoder is still holding is not one of these**, and the T14 is
/// why the distinction is written down here rather than left to arithmetic.
/// It reported `6 bytes, 0 keys, 2 motion, no event from [aux 0x08, aux 0x06,
/// aux 0x08, aux 0x0e]` while its pointer was framing perfectly: two whole
/// packets, and the four listed bytes were their heads and first body bytes.
/// A list that names two thirds of a healthy pointer stream is not a list of
/// suspects. `Partial` below is what holds a run until the byte that ends it
/// says whether any of it produced anything.
///
/// Written only from `drain`, which holds `PS2` and is the one place that knows
/// both the byte and whether anything came of it.
const UNEXPLAINED_LEN: usize = 8;
static UNEXPLAINED: [AtomicU16; UNEXPLAINED_LEN] = [const { AtomicU16::new(0) }; UNEXPLAINED_LEN];
static UNEXPLAINED_N: AtomicU32 = AtomicU32::new(0);
const UNEXPLAINED_AUX: u16 = 1 << 8;

fn record_unexplained(byte: u8, aux: bool) {
    let n = UNEXPLAINED_N.fetch_add(1, Ordering::Relaxed) as usize;
    if let Some(slot) = UNEXPLAINED.get(n) {
        slot.store(u16::from(byte) | if aux { UNEXPLAINED_AUX } else { 0 }, Ordering::Relaxed);
    }
}

/// Renders ` no event from [0xe0, aux 0x08],` — and nothing at all when every
/// byte became an event, because a clause naming an empty list is a column of
/// panel width for no information.
struct Unexplained;

impl core::fmt::Display for Unexplained {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let seen = UNEXPLAINED_N.load(Ordering::Relaxed) as usize;
        if seen == 0 {
            return Ok(());
        }
        write!(f, " no event from [")?;
        for (i, slot) in UNEXPLAINED.iter().take(seen).enumerate() {
            let value = slot.load(Ordering::Relaxed);
            if i > 0 {
                write!(f, ", ")?;
            }
            if value & UNEXPLAINED_AUX != 0 {
                write!(f, "aux ")?;
            }
            write!(f, "{:#04x}", value as u8)?;
        }
        if seen > UNEXPLAINED_LEN {
            write!(f, ", +{}", seen - UNEXPLAINED_LEN)?;
        }
        write!(f, "],")
    }
}

fn claim_health(from: u8, to: u8) -> bool {
    HEALTH
        .compare_exchange(from, to, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

/// Called from the idle loop with interrupts off, before a CPU halts: pure
/// atomics, no lock, no port I/O.
///
/// `true` keeps that CPU awake for exactly one more pass, which is what emits
/// the verdict — the same self-clearing shape as the log ring's pre-halt check
/// beside it. It cannot spin: the pass moves the state on, and an inactive
/// driver (quarantined, or one `init` never armed) answers `false` outright, so
/// no path leaves a CPU awake for a report nobody will make.
pub fn verdict_due() -> bool {
    if !ACTIVE.load(Ordering::Relaxed) {
        return false;
    }
    match HEALTH.load(Ordering::Relaxed) {
        HEALTH_ARMED => {
            claim_health(HEALTH_ARMED, HEALTH_QUIET_DUE);
            true
        }
        HEALTH_QUIET_DUE => true,
        _ => false,
    }
}

fn millis_since_boot() -> u64 {
    crate::clock::nanos_since_boot() / 1_000_000
}

fn first_irq_ms() -> u64 {
    FIRST_IRQ_NS.load(Ordering::Relaxed) / 1_000_000
}

/// Say once whether the armed pin has ever asserted. Runs in thread context
/// from `service`, on whichever CPU took the pass; the compare-exchange is what
/// keeps two CPUs from both reporting.
///
/// **"Nothing decoded" is a claim about bytes, so it is only made when bytes
/// have arrived and been accounted for.** Three producers used to make it about
/// something else, and each printed a line naming no byte at all — which is the
/// one shape `Unexplained` exists to replace, and which
/// `i8042_undecoded_bytes` reads as a report about its own injection:
///
/// - An interrupt with no byte behind it. Classified by the ISR and counted
///   apart in [`TALLY`], said in its own words below; the mute verdict stays
///   owed.
/// - An interrupt still *inside* its burst. `carried` used to move on the way
///   in, so a reader between the pin and the first `push_isr` had a count of
///   arrived bytes and no byte anywhere. `tally.rs` is why that is no longer
///   representable: the count moves once, after the burst, and after the bytes
///   are in the ring.
/// - A byte still in the ring, or one popped and not yet counted. `service`
///   drains before it reports, but the pin is live between the two, so an
///   interrupt landing in that gap is counted here with its byte undecoded. One
///   `has_bytes` load defers the report to the pass that has the byte, which is
///   where the report was always meant to be made — and [`RX_BYTES`] is counted
///   in `pop` so that a byte in mid-decode is on one side of that guard rather
///   than neither.
///
/// Between them: `carried > 0` means bytes reached the ring, and a byte that
/// reached the ring is in it or in `RX_BYTES` at every instant, so `N interrupts
/// and 0 bytes, nothing decoded` cannot be printed. The one exception says so
/// itself — a ring overflow drops bytes the ISR had already counted, and
/// `drain` logs that on its own line.
fn report_health(state: u8) {
    let counts = TALLY.read();
    let irqs = counts.irqs();
    if counts.carried > 0 || RX_BYTES.load(Ordering::Relaxed) > 0 {
        if has_bytes() {
            return;
        }
        let keys = KBD_EVENTS.load(Ordering::Relaxed);
        let motion = AUX_EVENTS.load(Ordering::Relaxed);
        // Two lines at most, and the second only when the picture changes from
        // "nothing decoded" to "something did". The first interrupt's own pass
        // is the earliest moment this can be said and also the least settled
        // one: a keystroke is up to six bytes and the pass runs between them,
        // so a report that went straight to DONE would freeze the panel on a
        // half-arrived sequence and never correct itself.
        let next = if keys + motion == 0 { HEALTH_MUTE_SAID } else { HEALTH_DONE };
        if next == state || !claim_health(state, next) {
            return;
        }
        if next == HEALTH_MUTE_SAID {
            log!(
                "i8042: {} interrupts and {} bytes, nothing decoded —{} first seen at {}ms",
                irqs,
                RX_BYTES.load(Ordering::Relaxed),
                Unexplained,
                first_irq_ms()
            );
        } else {
            log!(
                "i8042: the pin asserts — {} interrupts, {} bytes, {} keys, {} motion,{} first seen at {}ms",
                irqs,
                RX_BYTES.load(Ordering::Relaxed),
                keys,
                motion,
                Unexplained,
                first_irq_ms()
            );
        }
        to_screen();
        // Whichever of the two it was, the counters are now on the record and
        // the repeat can start measuring from here.
        arm_repeat();
        return;
    }
    // The pin asserts and nothing has come over it. Said in its own words
    // because neither of the two above is true: "nothing decoded" would be a
    // verdict on bytes that never arrived, and "the pin has never asserted"
    // below is false — this is the third thing a controller can do, and on a
    // machine whose only channel is the panel it is the difference between an
    // init that took its own answers and a keyboard nobody has touched.
    if irqs > 0 {
        if state != HEALTH_EMPTY_SAID && claim_health(state, HEALTH_EMPTY_SAID) {
            log!(
                "i8042: {} interrupts and no byte behind any of them — the output buffer was empty when the ISR read it, first seen at {}ms",
                irqs,
                first_irq_ms()
            );
            to_screen();
            arm_repeat();
        }
        return;
    }
    if state == HEALTH_QUIET_DUE && claim_health(HEALTH_QUIET_DUE, HEALTH_QUIET_SAID) {
        log!(
            "i8042: armed at {}ms, idle at {}ms, 0 interrupts — the pin has never asserted (kbd GSI {}, aux GSI {})",
            ARMED_NS.load(Ordering::Relaxed) / 1_000_000,
            millis_since_boot(),
            KEYBOARD_GSI.load(Ordering::Relaxed) as i64,
            AUX_GSI.load(Ordering::Relaxed) as i64
        );
        to_screen();
    }
}

fn arm_repeat() {
    NEXT_REPORT_NS.store(crate::clock::nanos_since_boot() + health_period_ns(), Ordering::Relaxed);
}

/// Say the counters again, at most once per [`health_period_ns`] and only when
/// the pin has asserted since the last line.
///
/// Thread context, from `service`. On a settled machine it is two relaxed loads
/// and a compare; the compare-exchange is what keeps two CPUs in the same pass
/// from both reporting, and is reached only when a line is actually owed.
fn report_counters() {
    let counts = TALLY.read();
    let irqs = counts.irqs();
    if irqs == REPORTED_IRQS.load(Ordering::Relaxed) {
        return;
    }
    let now = crate::clock::nanos_since_boot();
    let next = NEXT_REPORT_NS.load(Ordering::Relaxed);
    if now < next
        || NEXT_REPORT_NS
            .compare_exchange(next, now + health_period_ns(), Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    REPORTED_IRQS.store(irqs, Ordering::Relaxed);
    // `empty` rides with the fault counters because that is the only place it
    // can still be read once traffic starts: the line above it fires only while
    // *every* interrupt was empty, and the common case is one at bring-up
    // followed by a keyboard that works. It comes out of the same reading as
    // the total, so the two cannot disagree about how many interrupts there
    // were.
    log!(
        "i8042: {} interrupts, {} bytes, {} keys, {} motion, {} undecoded, {} discarded, {} overruns, {} dropped, {} lost edges, {} empty — last byte at {}ms",
        irqs,
        RX_BYTES.load(Ordering::Relaxed),
        KBD_EVENTS.load(Ordering::Relaxed),
        AUX_EVENTS.load(Ordering::Relaxed),
        UNEXPLAINED_N.load(Ordering::Relaxed),
        DISCARDS.load(Ordering::Relaxed),
        OVERRUNS.load(Ordering::Relaxed),
        DROPPED_TOTAL.load(Ordering::Relaxed),
        LOST_EDGES.load(Ordering::Relaxed),
        counts.empty,
        LAST_IRQ_NS.load(Ordering::Relaxed) / 1_000_000
    );
}

/// What the line looks like from outside, for a machine on which the pin has
/// gone quiet and nothing else can say why.
///
/// [`report_counters`] speaks only when the pin has asserted since its last
/// line, which is what makes its silence evidence — but evidence of one fact
/// with three causes, and it cannot separate them:
///
/// - **the controller is still holding a byte nobody took.** Delivery is edge
///   triggered, so it does not assert again until port 0x60 is read, and the
///   only reader is an ISR that will never run. `OBF` set on sample after
///   sample is that state and nothing else is.
/// - **the redirection entry changed.** Masked, re-pointed, or carrying a
///   vector that is no longer ours — all one word, printed raw.
/// - **neither, and the counters are flat**, which puts the fault at the EC or
///   the device and takes this driver out of it.
///
/// Every read is free of side effects: 0x64 is the status register, and an
/// entry is read through `IOREGSEL`/`IOWIN` under the topology's own lock. So
/// this does not have to run on the CPU the vector is pinned to, and it does
/// not disturb the ISR's sole ownership of 0x60.
///
/// Behind `heartbeat` because it can only be asked from the idle loop and only
/// a `diag-tick` build reaches that on a machine with nothing to run — which is
/// exactly the machine it is for.
#[cfg(feature = "boot-actuators")]
pub fn report_line() {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    log!(
        "i8042: line status={:#04x} irqs={} bytes={} kbd {} aux {}",
        inb(STATUS),
        TALLY.read().irqs(),
        RX_BYTES.load(Ordering::Relaxed),
        Rte(KEYBOARD_GSI.load(Ordering::Relaxed)),
        Rte(AUX_GSI.load(Ordering::Relaxed)),
    );
}

/// `gsi=1 rte=0x0000000000000024`, or why there is no entry to print.
#[cfg(feature = "boot-actuators")]
struct Rte(u32);

#[cfg(feature = "boot-actuators")]
impl core::fmt::Display for Rte {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0 == u32::MAX {
            return write!(f, "unrouted");
        }
        match ioapic::redirection(Gsi(self.0)) {
            Some(entry) => write!(f, "gsi={} rte={:#018x}", self.0, entry),
            None => write!(f, "gsi={} rte=busy", self.0),
        }
    }
}

/// Put the verdict on the panel as well as in the log ring, and only on a
/// machine that has nowhere else to put it.
///
/// The ring is the primary channel and the permanent one: it is what a serial
/// console carries today and what any later log sink drains. This is the
/// fallback for the machine the panic console was built for in the first place
/// — no UART, no virtio-console — and `panic_flush` declines on exactly the
/// same test. Repainting over a working console would cost a screenful and the
/// full framebuffer write for a line the owner can already read.
///
/// Best-effort by construction: `boot_checkpoint` returns without painting once
/// userland claims the framebuffer, so on an image that starts a compositor
/// this does nothing. That is what `diag/system.toml` exists to avoid and it is
/// not this driver's to solve.
fn to_screen() {
    if crate::drivers::serial::has_console() {
        return;
    }
    crate::drivers::panic_console::boot_checkpoint();
}

/// The decoder saw a device reset. Handled on `IRQ_CPU`, whichever CPU noticed.
static AUX_RESET_PENDING: AtomicBool = AtomicBool::new(false);
static AUX_REENABLE_FAILURES: AtomicU32 = AtomicU32::new(0);

/// A device that answers at all answers a one-byte command in well under a
/// millisecond; this is interrupts-off time on the one CPU that takes the
/// vector, so it is sized for "the controller is gone", not for slowness.
const AUX_REENABLE: Budget = Budget::of(
    Duration::from_millis(30),
    "the re-enable handshake is abandoned and counted against AUX_REENABLE_GIVE_UP",
);

/// A TrackPoint that resets in a loop would otherwise buy the same handshake
/// forever. After this many consecutive failures the aux line is masked and
/// the pointer is written off, which is one log line rather than a stall.
const AUX_REENABLE_GIVE_UP: u32 = 3;

// The byte ring.
//
// Producer is single by construction: delivery is pinned to one CPU and the
// gate is an interrupt gate, so the handler cannot nest. Consumers are
// serialized by `PS2`, which no ISR takes. `irq_ring`'s Relaxed-everywhere
// argument rests on every access being same-CPU and does not transfer here,
// because a drain runs on whichever CPU entered the scheduler — hence
// Release/Acquire on both indices. On x86 that is a compiler fence.

const RING_LEN: usize = 256;
const AUX_FLAG: u64 = 1 << 8;
/// The rest of the slot is the arrival time in microseconds. The mouse framer
/// resyncs on the gap between adjacent bytes and nothing else, so the time the
/// *drain* ran is useless to it — a batch would flatten every gap to zero. 55
/// bits of microseconds is longer than any machine stays up.
const TIME_SHIFT: u32 = 9;

static BYTES: [AtomicU64; RING_LEN] = [const { AtomicU64::new(0) }; RING_LEN];
static HEAD: AtomicU32 = AtomicU32::new(0);
static TAIL: AtomicU32 = AtomicU32::new(0);

fn push_isr(byte: u8, aux: bool, arrived_ns: u64) {
    let head = HEAD.load(Ordering::Relaxed);
    if head.wrapping_sub(TAIL.load(Ordering::Acquire)) as usize >= RING_LEN {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let slot = &BYTES[head as usize % RING_LEN];
    let value = ((arrived_ns / 1_000) << TIME_SHIFT)
        | if aux { AUX_FLAG } else { 0 }
        | byte as u64;
    slot.store(value, Ordering::Relaxed);
    HEAD.store(head.wrapping_add(1), Ordering::Release);
}

fn pop() -> Option<(u8, bool, u64)> {
    let tail = TAIL.load(Ordering::Relaxed);
    if tail == HEAD.load(Ordering::Acquire) {
        return None;
    }
    let value = BYTES[tail as usize % RING_LEN].load(Ordering::Relaxed);
    // Counted *before* the slot is released, so that a byte is in the ring or in
    // this number and never in neither: `report_health`'s `has_bytes` guard is
    // only a guard if a byte in mid-decode is on one side of it. See
    // [`RX_BYTES`].
    RX_BYTES.fetch_add(1, Ordering::Relaxed);
    TAIL.store(tail.wrapping_add(1), Ordering::Release);
    Some((value as u8, value & AUX_FLAG != 0, (value >> TIME_SHIFT) * 1_000))
}

fn has_bytes() -> bool {
    HEAD.load(Ordering::Acquire) != TAIL.load(Ordering::Relaxed)
}

/// Under `i8042-fault`, armed at the end of a successful init so the next
/// interrupt makes the output buffer look permanently full. The only way to
/// reach the ISR's bound without a controller that is genuinely broken.
static FAULT: AtomicBool = AtomicBool::new(false);

#[inline]
fn buffer_full(status: u8) -> bool {
    if crate::actuator::i8042_fault() && FAULT.load(Ordering::Relaxed) {
        return true;
    }
    status & OBF != 0
}

/// Rust half of the pin-interrupt handler. Read the module doc before adding
/// anything to it.
pub extern "sysv64" fn handler() {
    crate::irq_census::irq_took!(I8042);
    let timestamp = crate::clock::nanos_since_boot();
    // No compare-exchange: delivery is pinned to one CPU behind an interrupt
    // gate, so this handler cannot nest and there is no second writer.
    if FIRST_IRQ_NS.load(Ordering::Relaxed) == 0 {
        FIRST_IRQ_NS.store(timestamp, Ordering::Relaxed);
    }
    LAST_IRQ_NS.store(timestamp, Ordering::Relaxed);
    let mut n = 0;
    while n < ISR_BURST {
        let status = inb(STATUS);
        if !buffer_full(status) {
            break;
        }
        // Read where the byte is read, not once for the burst. The mouse framer
        // has no start marker and resyncs on the idle gap between *adjacent*
        // bytes, so one timestamp for a burst is the flattening `mouse.rs` says
        // must not happen — and a burst is what a delayed ISR takes, which is
        // the same delay under which bytes get lost and the gap is needed.
        push_isr(inb(DATA), status & AUXB != 0, crate::clock::nanos_since_boot());
        n += 1;
    }
    if n == ISR_BURST && buffer_full(inb(STATUS)) {
        // It cannot mask the line itself — that needs the I/O APIC lock.
        QUARANTINE.store(true, Ordering::Relaxed);
    }
    // One release-add, here and not on the way in: what this says is what the
    // burst *found*, so it is only sayable now — and the release publishes the
    // bytes above it, so no reader can see this interrupt counted and go looking
    // for a byte that has not landed. `tally.rs` carries the argument.
    TALLY.record(if n == 0 { Carried::Nothing } else { Carried::Bytes });
    if n > 0 {
        crate::irq_ring::isr_publish(IrqSource::I8042, timestamp);
        crate::preempt::set_need_resched();
    }
    crate::arch::apic::eoi();
}

/// A run of bytes a decoder has taken and not yet accounted for.
///
/// A packet is three bytes and a Pause is six, spread over as many scheduler
/// passes as the interrupts fall in, and only the byte that *ends* the run says
/// whether any of it produced anything. Holding the run is what lets the whole
/// of an undecodable sequence be named — `0xE1 0x1D 0x45 0xE1 0x9D 0xC5` rather
/// than its last byte — without naming the identical bytes of one that worked.
///
/// One per stream, because the keyboard's and the pointer's interleave in the
/// ring. Capped at what the report can print; past that the oldest bytes of an
/// over-long run are the ones worth keeping.
///
/// A partial the mouse framer abandons on the idle gap is not reported as
/// abandoned, so its bytes stay here until the next run ends. That over-names
/// at most two bytes, in a stream that has already lost one — never in the
/// healthy case this exists for.
struct Partial {
    bytes: [u8; UNEXPLAINED_LEN],
    len: usize,
}

impl Partial {
    const fn new() -> Self {
        Self { bytes: [0; UNEXPLAINED_LEN], len: 0 }
    }

    fn push(&mut self, byte: u8) {
        if self.len < UNEXPLAINED_LEN {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }

    /// The run produced an event, so none of it is a suspect.
    fn clear(&mut self) {
        self.len = 0;
    }

    /// The run ended on `last` and produced nothing. All of it is.
    fn blame(&mut self, last: u8, aux: bool) {
        for i in 0..self.len {
            record_unexplained(self.bytes[i], aux);
        }
        self.len = 0;
        record_unexplained(last, aux);
    }
}

struct Decoders {
    keys: KeyDecoder,
    pointer: MouseDecoder,
    kbd_partial: Partial,
    aux_partial: Partial,
}

static PS2: Lock<Decoders> = Lock::new(Decoders {
    keys: KeyDecoder::new(),
    pointer: MouseDecoder::new(),
    kbd_partial: Partial::new(),
    aux_partial: Partial::new(),
});

/// Turn whatever the ISR published into events and wakes. Runs at the top of
/// every scheduler pass on every CPU, so the idle cost is one atomic load.
pub fn service() {
    // Unconditionally, and before any other test: an undrained `irq_ring`
    // record keeps `any_pending_self` true, and the idle loop rechecks it
    // before halting — so a record nobody consumes spins a CPU forever. The
    // quarantine path found this the hard way.
    let recorded = crate::irq_ring::take(IrqSource::I8042).is_some();
    if QUARANTINE.load(Ordering::Relaxed) {
        quarantine();
        return;
    }
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    // Polled port I/O, so only the CPU the vector is pinned to may do it. Any
    // other CPU leaves the request standing; this one is in a pass at least
    // once a tick, and a lid-open is not a deadline.
    if AUX_RESET_PENDING.load(Ordering::Relaxed) && is_irq_cpu() {
        aux_reenable();
    }
    widen_edge_window();
    // Unconditional, not gated on `recorded`: this is what detects a lost
    // edge and what heals it in the same pass.
    if has_bytes() {
        // The ISR fills the ring before it publishes its record, so bytes this
        // pass finds may belong to an interrupt that arrived after the record
        // was read absent. Asking again with the bytes in hand is what tells
        // that apart from an edge nothing ever delivered.
        let recorded = recorded || crate::irq_ring::take(IrqSource::I8042).is_some();
        service_bytes(recorded);
    }
    // Last, so the line it may print counts the bytes this pass just decoded.
    // Reported from the top, the first interrupt's own pass said `2 interrupts,
    // 0 bytes` — true at the instant it was read and useless to read.
    let health = HEALTH.load(Ordering::Relaxed);
    if health != HEALTH_DONE {
        report_health(health);
    }
    // Not gated on the state: a machine whose bytes all decode to nothing stays
    // in `HEALTH_MUTE_SAID` forever, and that is the case the repeat is most
    // needed for.
    report_counters();
}

/// Under `i8042-edge-race`, hold the pass between reading the record and
/// reading the ring for long enough that an interrupt lands in between.
///
/// Unwidened that window is a handful of instructions on one CPU, which no
/// injection the harness can time and no load it can stage reaches.
fn widen_edge_window() {
    if !crate::actuator::i8042_edge_race() {
        return;
    }
    for _ in 0..200 {
        core::hint::spin_loop();
    }
}

/// Decode what the ISR left in the ring and wake whoever it belongs to.
/// `recorded` is whether this pass found an `irq_ring` record for the source.
fn service_bytes(recorded: bool) {
    // Only `IRQ_CPU` can hold a record for this source, so only `IRQ_CPU` can
    // read anything into its absence. On any other CPU `!recorded` is a fact
    // about `irq_ring`'s per-CPU shape, and counting it there reported a lost
    // edge on every healthy `--smp N>1` boot.
    if !recorded && is_irq_cpu() {
        // Loud the first time, silent after — a rate is what would matter and
        // nothing reads one.
        if LOST_EDGES.fetch_add(1, Ordering::Relaxed) == 0 {
            log!("i8042: bytes with no IRQ record — an edge was lost");
        }
    }

    let Drained { bytes, keys, motion, aux_reset } = drain();

    // Wake only when the decode queued something. Readiness that disagrees
    // with `has_data()` parks the next reader until the following real
    // event, which is the defect that froze the compositor on the USB path.
    let woke_kb = keys > 0;
    if woke_kb {
        crate::keyboard::wake_waiters();
        let watchers = crate::keyboard::inbox_watchers();
        if !watchers.is_empty() {
            crate::inbox::complete_pending_for_event(
                &watchers,
                crate::inbox::Source::Keyboard,
            );
        }
    }
    let woke_ms = motion > 0;
    if woke_ms {
        crate::mouse::wake_waiters();
        let watchers = crate::mouse::inbox_watchers();
        if !watchers.is_empty() {
            crate::inbox::complete_pending_for_event(
                &watchers,
                crate::inbox::Source::Mouse,
            );
        }
    }
    trace_drain(bytes, keys, motion, woke_kb, woke_ms);

    if aux_reset {
        AUX_RESET_PENDING.store(true, Ordering::Relaxed);
    }
}

struct Drained {
    bytes: usize,
    keys: usize,
    motion: usize,
    aux_reset: bool,
}

/// Consume the ring. Releases `PS2` before returning, so the caller's wakes —
/// which reach the scheduler, cross-CPU doorbells and possibly an IPI —
/// never run under a driver lock. Lock order is PS2 → KEY_BUF, never the
/// reverse.
fn drain() -> Drained {
    let mut state = PS2.lock();
    let mut out = Drained { bytes: 0, keys: 0, motion: 0, aux_reset: false };
    let mut lost = false;

    let dropped = DROPPED.swap(0, Ordering::Relaxed);
    DROPPED_TOTAL.fetch_add(dropped, Ordering::Relaxed);
    if dropped > 0 {
        // Never expected: 256 slots against ~300 B/s, drained at every
        // scheduler pass. It costs a gesture, not the framing, which is what
        // the decoder resets below are for.
        log!("i8042: ring overflow, {} bytes dropped — resyncing", dropped);
    }
    if dropped > 0 {
        // A hole in a framed stream: both decoders' partial state is
        // meaningless now, and the pointer would stay one byte off forever.
        state.keys.reset();
        state.pointer.reset();
        state.kbd_partial.clear();
        state.aux_partial.clear();
        lost = true;
    }

    while let Some((byte, aux, arrived)) = pop() {
        out.bytes += 1;
        // Whether the byte's *run* is over, and whether anything came of it.
        // The whole path, not the decoder's verdict: `handle_key` drops a break
        // for a usage nothing held, and `handle_motion` drops a packet that
        // moved the cursor nowhere, and both are bytes that produced no event
        // just as much as an unmapped code is.
        let explained = if aux {
            match state.pointer.feed(byte, arrived) {
                MouseOutcome::Pending => {
                    state.aux_partial.push(byte);
                    continue;
                }
                MouseOutcome::Packet { buttons, dx, dy } => {
                    let queued = crate::mouse::handle_motion(
                        crate::mouse::PointerSource::PS2,
                        buttons,
                        crate::mouse::Motion::Relative { dx, dy },
                        0,
                    );
                    if queued {
                        out.motion += 1;
                    }
                    queued
                }
                MouseOutcome::Reset => {
                    out.aux_reset = true;
                    true
                }
                MouseOutcome::Discarded => {
                    DISCARDS.fetch_add(1, Ordering::Relaxed);
                    false
                }
            }
        } else {
            match state.keys.feed(byte) {
                KeyOutcome::Pending => {
                    state.kbd_partial.push(byte);
                    continue;
                }
                KeyOutcome::Key { usage, pressed } => {
                    let queued = crate::keyboard::handle_key(usage, pressed);
                    if queued {
                        out.keys += 1;
                    }
                    queued
                }
                // The overrun codes explain themselves and are counted rather
                // than blamed: `0x00`/`0xFF` name the fault outright, which is
                // more than the byte list could add.
                KeyOutcome::Lost => {
                    OVERRUNS.fetch_add(1, Ordering::Relaxed);
                    lost = true;
                    true
                }
                KeyOutcome::None => false,
            }
        };
        let partial = if aux { &mut state.aux_partial } else { &mut state.kbd_partial };
        if explained {
            partial.clear();
        } else {
            partial.blame(byte, aux);
        }
    }

    if lost {
        // The break codes for whatever is down may be among what was lost —
        // and so may the packet that lifts a held pointer button, which no
        // later report from another pointer can clear.
        out.keys += crate::keyboard::release_all();
        if crate::mouse::release_buttons(crate::mouse::PointerSource::PS2) {
            out.motion += 1;
        }
    }

    KBD_EVENTS.fetch_add(out.keys as u32, Ordering::Relaxed);
    AUX_EVENTS.fetch_add(out.motion as u32, Ordering::Relaxed);
    // `RX_BYTES` is not here: `pop` counts each byte as it takes it, which is
    // what keeps a byte from being invisible for the length of its own decode.
    out
}

/// A controller producing bytes faster than the ISR's bound can drain them.
/// One masked line and a dead keyboard, never a spinning CPU.
fn quarantine() {
    QUARANTINE.store(false, Ordering::Relaxed);
    ACTIVE.store(false, Ordering::Relaxed);
    // The line below carries the same counters the health verdict would, and
    // the pin is about to be masked, so there is nothing left for it to say.
    HEALTH.store(HEALTH_DONE, Ordering::Relaxed);
    // Whatever was down stays down otherwise: no further report can arrive to
    // lift it, and the pointer merge republishes it on every other pointer's
    // motion for the rest of the boot.
    crate::keyboard::release_all();
    crate::mouse::release_buttons(crate::mouse::PointerSource::PS2);
    // The count, not the intent: "one masked line and a dead keyboard,
    // never a spinning CPU" is only true if the mask actually took.
    let mut masked = 0;
    for line in [KEYBOARD_GSI.load(Ordering::Relaxed), AUX_GSI.load(Ordering::Relaxed)] {
        if line != u32::MAX && ioapic::set_masked(Gsi(line), true).is_ok() {
            masked += 1;
        }
    }
    log!(
        "i8042: quarantined — output buffer never emptied, masked={} (kbd={} aux={} lost={})",
        masked,
        KBD_EVENTS.load(Ordering::Relaxed),
        AUX_EVENTS.load(Ordering::Relaxed),
        LOST_EDGES.load(Ordering::Relaxed)
    );
}

/// The `woke_*` fields are the gates the wakes actually ran under, not a
/// re-derivation of them — so a test can assert the gate agrees with the
/// event count.
fn trace_drain(bytes: usize, keys: usize, motion: usize, woke_kb: bool, woke_ms: bool) {
    if !crate::actuator::i8042_trace() {
        return;
    }
    log!(
        "i8042: drain bytes={} keys={} motion={} woke_kb={} woke_ms={}",
        bytes,
        keys,
        motion,
        u8::from(woke_kb),
        u8::from(woke_ms)
    );
}

// Polled init.
//
// Nothing below runs after `ACTIVE` is set, and all of it runs with the
// controller's interrupt bits clear, so the ISR cannot be racing it.

fn deadline(millis: u64) -> u64 {
    crate::clock::nanos_since_boot() + millis * 1_000_000
}

/// The millisecond figure this module's polled init is written in.
///
/// Every stage below is a [`Budget`] — expiry names the stage and degrades the
/// probe's answer, never panics — and the arithmetic that sums them stays in
/// milliseconds, where it was written and where its own doc argues about it.
const fn ms(budget: Budget) -> u64 {
    budget.duration().millis()
}

/// Everything that is not inside a named stage draws on this: the initial
/// disable and flush, the config read-modify-write and its read-back, both
/// interface tests, and the arming write at the end. Each is a controller
/// command with no PS/2 device behind it, so none of them waits on an EC's
/// firmware — but the time is still spent, and leaving it out of the total is
/// what made the total wrong.
///
/// An allowance, not a measurement. No real EC has ever been timed here, in
/// either direction.
const CONTROLLER: Budget = Budget::of(
    Duration::from_millis(250),
    "the stage that ran out is named and the probe reports DISABLED",
);
/// `0xAA`. A floating bus is already gone by here, so what this separates is
/// "a controller" from "something else decoding 0x60/0x64" — firmware trapping
/// the ports in SMM for USB legacy emulation is the case that exists.
const SELFTEST: Budget = Budget::of(
    Duration::from_millis(500),
    "the controller is reported absent and the machine boots with no PS/2 input",
);
/// `0xF5`, the `0xF0 0x00` read-back and `0xF4`, each acknowledged by the
/// keyboard itself rather than by the controller.
const KEYBOARD: Budget = Budget::of(
    Duration::from_millis(750),
    "the keyboard stage is named as the one that ran out",
);
/// The aux port's `0xFF` is a *device reset*: a real PS/2 device answers it
/// with a self-test that takes real time, which is why this stage is the one
/// that must not be shortened to make an arithmetic error go away.
const AUX_RESET: Budget = Budget::of(
    Duration::from_millis(600),
    "the pointer is written off and the keyboard half still comes up",
);

/// Derived, never written down independently.
///
/// It used to be a literal 1500 against stages summing to 1850, so a machine
/// slow enough to use a meaningful fraction of each stage ran out of budget
/// with the arming write still to come — and every wait after that returned
/// immediately, making a *timeout* present as `DISABLED — cfg … did not take`,
/// a controller fault. Deriving the total is what makes that disagreement
/// unrepresentable; naming the stage that ran out is what makes the remaining
/// case legible. The direction of the fix is forced: each stage number is what
/// that step is worth waiting *from here*, so shrinking one silently shortens a
/// real device's wait, and the aux reset's is the last one to touch.
/// `i8042-budget-expired` spends the whole of it before the probe starts, so
/// the expiry paths run on a controller that is answering perfectly. QEMU
/// answers every step in microseconds and no real EC timing has ever been
/// taken, so nothing else can reach them.
fn init_budget_ms() -> u64 {
    if crate::actuator::i8042_budget_expired() {
        0
    } else {
        ms(CONTROLLER) + ms(SELFTEST) + ms(KEYBOARD) + ms(AUX_RESET)
    }
}

/// A stage's own deadline, never past the whole probe's — and `None` when the
/// probe's is already spent.
///
/// The clamp alone cannot say *why* a step gave up. With the budget gone every
/// `wait_writable` and `read_data` below returns immediately, so a slow EC
/// produces the log line a broken controller produces. On a machine that cannot
/// be single-stepped, naming the stage that ran out is the whole difference
/// between "your firmware is slow" and "your controller is broken".
fn stage(millis: u64, budget: u64, name: &str) -> Option<u64> {
    if crate::clock::nanos_since_boot() >= budget {
        log!(
            "i8042: {}ms init budget spent before the {name} stage — no PS/2 input",
            init_budget_ms()
        );
        return None;
    }
    Some(deadline(millis).min(budget))
}

fn budget_spent(budget: u64) -> bool {
    crate::clock::nanos_since_boot() >= budget
}

/// The status register, or `None` when nothing decodes the port.
fn status() -> Option<u8> {
    match inb(STATUS) {
        FLOATING_BUS => None,
        other => Some(other),
    }
}

fn wait_writable(deadline: u64) -> bool {
    loop {
        let Some(status) = status() else { return false };
        if status & IBF == 0 {
            return true;
        }
        if crate::clock::nanos_since_boot() >= deadline {
            return false;
        }
    }
}

fn read_data(deadline: u64) -> Option<u8> {
    loop {
        if status()? & OBF != 0 {
            return Some(inb(DATA));
        }
        if crate::clock::nanos_since_boot() >= deadline {
            return None;
        }
    }
}

fn command(cmd: u8, deadline: u64) -> bool {
    wait_writable(deadline) && {
        // SAFETY: `outb` asks its caller to own the port and the byte.
        // `COMMAND` is 0x64, the 8042's fixed architectural command port — no
        // other device on any machine this kernel targets decodes it, and the
        // controller has no path to memory. Every `cmd` that reaches here is one
        // of this module's `CMD_*` constants, which are the controller's own
        // documented command bytes.
        unsafe { outb(COMMAND, cmd) };
        true
    }
}

fn write_data(byte: u8, deadline: u64) -> bool {
    wait_writable(deadline) && {
        // SAFETY: `command`'s argument for the port — `DATA` is 0x60, the other
        // half of the same controller's two-port block. The byte is either a
        // configuration word this module built or a device command destined for
        // the keyboard or the mouse behind the controller, neither of which can
        // reach memory.
        unsafe { outb(DATA, byte) };
        true
    }
}

fn read_config(deadline: u64) -> Option<u8> {
    command(CMD_READ_CONFIG, deadline).then(|| read_data(deadline)).flatten()
}

fn write_config(value: u8, deadline: u64) -> bool {
    command(CMD_WRITE_CONFIG, deadline) && write_data(value, deadline)
}

/// Iteration-bounded rather than clock-bounded, and takes no deadline for that
/// reason: draining a one-byte buffer 32 times is already past every legitimate
/// backlog, and a controller still asserting OBF after that is not going to
/// stop.
fn flush() -> bool {
    for _ in 0..32 {
        match status() {
            None => return false,
            Some(s) if s & OBF == 0 => return true,
            Some(_) => {
                inb(DATA);
            }
        }
    }
    status().is_some_and(|s| s & OBF == 0)
}

/// Send a device command byte by byte, each acknowledged with 0xFA.
///
/// No retry on 0xFE (resend): it is a wire-error recovery this driver has
/// never seen QEMU produce and cannot exercise, and a silent retry would
/// hide the one case worth knowing about. The byte that came back instead of
/// the ack is logged, which is what makes it diagnosable on metal.
fn device_command(bytes: &[u8], deadline: u64) -> bool {
    for &byte in bytes {
        if !write_data(byte, deadline) {
            log!("i8042: kbd cmd {:#04x} — input buffer never cleared", byte);
            return false;
        }
        match read_data(deadline) {
            Some(0xFA) => {}
            other => {
                log!("i8042: kbd cmd {:#04x} answered {:?}, not ack", byte, other);
                return false;
            }
        }
    }
    true
}

/// What `0xF0 0x00` established.
enum SetQuery {
    /// The read-back byte, translated by the controller like every other byte
    /// from port 1 — so it names the wire format, not just the set.
    Told(u8),
    /// A byte that is not an ack came back. The device does not implement the
    /// exchange, and the byte is the diagnosis.
    Refused(u8),
    /// Nothing came back at all, or the controller never took a write.
    Silent,
}

/// Ask the keyboard which scancode set it is in. Read, never write.
///
/// The matching write, `0xF0 0x02`, is not sent. On a translating controller
/// nothing else in the machine's life sends it either: Linux's
/// `atkbd_select_set` returns set 2 outright when `atkbd->translated` (which
/// `i8042.c` derives from the XLATE bit of the config byte the BIOS left), and
/// EDK2's `Ps2KeyboardDxe` selects a set only under `ExtendedVerification`,
/// which its own comment says is skipped when booting an OS. A write cannot
/// improve on a read that already answers, and an EC that mishandles it is left
/// in a state nothing can name.
fn query_scancode_set(deadline: u64) -> SetQuery {
    if !write_data(0xF0, deadline) {
        return SetQuery::Silent;
    }
    match read_data(deadline) {
        Some(0xFA) => {}
        Some(other) => return SetQuery::Refused(other),
        None => return SetQuery::Silent,
    }
    if !write_data(0x00, deadline) {
        return SetQuery::Silent;
    }
    // The T14 refused here, on the argument, having acked the command byte.
    match echo_the_argument(read_data(deadline)) {
        Some(0xFA) => {}
        Some(other) => return SetQuery::Refused(other),
        None => return SetQuery::Silent,
    }
    match read_data(deadline) {
        Some(set) => SetQuery::Told(set),
        None => SetQuery::Silent,
    }
}

/// Under `i8042-kbd-echo`, the argument byte is answered `0xEE` — ECHO's own
/// reply, and what the ThinkPad T14 Gen 2's EC answered on its own screen.
/// QEMU's PS/2 keyboard implements `0xF0` to the letter and no device or
/// machine property makes it stop, so nothing on the host side can hand the
/// driver a keyboard that will not report its set. Only the verdict is
/// replaced: the two bytes still go out and the reply the device queued behind
/// them stays in the output buffer, which is the residue a real EC in an
/// unnameable state would leave.
fn echo_the_argument(real: Option<u8>) -> Option<u8> {
    if crate::actuator::i8042_kbd_echo() {
        Some(0xEE)
    } else {
        real
    }
}

/// Same, for the aux port: every byte has to be prefixed with the controller
/// command that redirects the next write to port 2.
fn aux_command(bytes: &[u8], deadline: u64) -> bool {
    for &byte in bytes {
        if !command(CMD_WRITE_AUX, deadline) || !write_data(byte, deadline) {
            log!("i8042: aux cmd {:#04x} — input buffer never cleared", byte);
            return false;
        }
        match read_data(deadline) {
            Some(0xFA) => {}
            other => {
                log!("i8042: aux cmd {:#04x} answered {:?}, not ack", byte, other);
                return false;
            }
        }
    }
    true
}

/// Re-enable data reporting after the device reset itself. The EC does this
/// after suspend or a lid event, and without it the TrackPoint goes silent
/// for the rest of the boot. Caller has already established `is_irq_cpu`.
///
/// Interrupts off on this CPU, and the lines left alone. Masking them is what
/// the first version did, and it was wrong twice over: masking an RTE stops
/// neither an ISR already executing nor a vector already latched in that CPU's
/// LAPIC, so it never made this the sole reader of the one-byte output buffer;
/// and an edge asserted on a masked edge-triggered entry is *dropped*, so a
/// byte landing in that window leaves OBF full with no interrupt ever again —
/// both PS/2 devices dead for the rest of the boot, silently. Being the pinned
/// CPU with IF=0 is what "sole reader" actually requires, and it costs no edge:
/// one asserted here is latched in the LAPIC and delivered on the way out, to
/// an ISR that finds the buffer already empty.
fn aux_reenable() {
    AUX_RESET_PENDING.store(false, Ordering::Relaxed);
    let ok = {
        let _irq = crate::hw::IrqGuard::close();
        let budget = deadline(ms(AUX_REENABLE));
        // The keyboard is still scanning — masking the *line* does not stop
        // the *device*. `init` disables port 1 for exactly this reason: a
        // keystroke arriving mid-handshake is consumed as the aux ack, and
        // with reporting still off no further aux byte would ever ask again.
        command(CMD_DISABLE_PORT1, budget);
        let ok = aux_command(&[0xF4], budget);
        command(CMD_ENABLE_PORT1, budget);
        // With edge delivery a byte left in OBF means no further interrupt
        // ever, so the buffer must be empty before interrupts come back.
        handler_poll();
        ok
    };
    if ok {
        AUX_REENABLE_FAILURES.store(0, Ordering::Relaxed);
        log!("i8042: aux reset itself, reporting re-enabled");
        return;
    }
    let failures = AUX_REENABLE_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
    if failures < AUX_REENABLE_GIVE_UP {
        log!("i8042: aux reset itself, re-enable failed ({failures}/{AUX_REENABLE_GIVE_UP})");
        return;
    }
    let aux = AUX_GSI.swap(u32::MAX, Ordering::Relaxed);
    if aux != u32::MAX {
        let _ = ioapic::set_masked(Gsi(aux), true);
    }
    crate::mouse::release_buttons(crate::mouse::PointerSource::PS2);
    log!("i8042: aux re-enable failed {failures} times — pointer written off, line masked");
}

/// What firmware claims about the 8042, which is never what decides.
///
/// The substitute is the ThinkPad T14 Gen 2's own answer, printed on its own
/// screen: FADT revision 6, `iapc_boot_arch=0x0011` — `LEGACY_DEVICES` set,
/// **8042 clear**, `NO_ASPM` set — on a machine whose integrated keyboard is
/// PS/2. It exists because QEMU cannot stage the disagreement: `i8042=off`
/// clears the bit *by removing the device*, and `-device i8042` puts the device
/// back into the QOM tree the bit is derived from, so on QEMU the claim and the
/// hardware always agree. Handing the driver a denial on a machine that has a
/// controller is the only way to test that the denial does not stop it.
fn firmware_claim(rsdp_addr: u64) -> Result<(u8, u16), crate::drivers::acpi::TableError> {
    if crate::actuator::i8042_fadt_denial() {
        return Ok((6, 0x0011));
    }
    crate::drivers::acpi::iapc_boot_arch(rsdp_addr)
}

pub fn init(rsdp_addr: u64) {
    // Firmware's claim is logged and not obeyed, and the asymmetry is the
    // reason. `IAPC_BOOT_ARCH` bit 1 is one summary bit a vendor wrote once;
    // the handshake below is a config-byte read-back, a `0xAB` port interface
    // test and a `0xF0 0x00` scancode-set query checked against `0x41` — three
    // direct observations of the machine in front of us, each of which is
    // strictly better evidence than the claim. Gating the strong check on the
    // weak one is backwards, and it cost the T14 its keyboard: bit 1 clear on a
    // laptop whose integrated keyboard is PS/2, so the controller was never
    // given a chance to answer.
    //
    // The line stays, because the *disagreement* is the diagnosis. What
    // firmware said and what the controller answered are two separate facts,
    // and a machine that cannot be single-stepped needs both on the same
    // screen. An unreadable table is a third answer and is spelled differently
    // again: a refusal from the parser says nothing about the hardware.
    match firmware_claim(rsdp_addr) {
        Ok((revision, flags)) => log!(
            "i8042: FADT rev {} iapc_boot_arch={:#06x}, bit 1 (8042) {} — probing either way",
            revision,
            flags,
            if flags & FADT_8042 != 0 { "set" } else { "clear" }
        ),
        Err(e) => log!("i8042: no trustworthy FADT ({e:?}) — firmware claims nothing either way"),
    }

    // One `inb` settles every machine that has nothing there, before a single
    // byte is written to ports that might belong to something else.
    if status().is_none() {
        log!("i8042: absent — port {STATUS:#x} reads {FLOATING_BUS:#04x}, nothing decodes it");
        return;
    }

    // The whole probe, from the first port touch to the last. Every stage
    // clamps to it, and it is the sum of the stages plus what the controller
    // steps between them are allowed, so no machine can spend it before the
    // last stage has had its own.
    let budget = deadline(init_budget_ms());

    // Firmware may leave scanning on. A keystroke arriving mid-handshake
    // makes the config read return a scancode and everything after garbage.
    command(CMD_DISABLE_PORT1, budget);
    command(CMD_DISABLE_AUX, budget);

    if !flush() {
        log!("i8042: absent (output buffer never drains)");
        return;
    }

    let Some(before) = read_config(budget) else {
        log!("i8042: absent (no config byte)");
        return;
    };
    // Interrupts off until the device has answered; translation on, because
    // the keyboard is about to be put in set 2 and set 1 is what this kernel
    // decodes; port-1 clock on.
    let wanted = (before & !(CFG_PORT1_IRQ | CFG_PORT2_IRQ | CFG_PORT1_CLOCK_OFF)) | CFG_TRANSLATE;
    if !write_config(wanted, budget) {
        log!("i8042: absent (config write never accepted)");
        return;
    }
    match read_config(budget) {
        Some(v) if v == wanted => {}
        other => {
            log!("i8042: absent (cfg wrote {:#04x}, read back {:?})", wanted, other);
            return;
        }
    }

    let Some(selftest_deadline) = stage(ms(SELFTEST), budget, "self-test") else {
        return;
    };
    command(CMD_SELF_TEST, selftest_deadline);
    match read_data(selftest_deadline) {
        Some(0x55) => {}
        other => {
            log!(
                "i8042: absent (self-test {:?}, {}ms) — no PS/2 input",
                other,
                ms(SELFTEST)
            );
            return;
        }
    }
    // Some controllers reset the config byte across 0xAA.
    write_config(wanted, budget);

    command(CMD_TEST_PORT1, budget);
    let port1 = read_data(budget);
    if port1 != Some(0x00) {
        log!("i8042: port 1 interface test {:?} — no keyboard", port1);
        return;
    }
    // Enabling port 2 clears its clock-disable bit iff the port exists. The
    // interface test is then the cheap way to learn it does not, instead of
    // waiting out the whole aux-reset stage on every machine without one.
    command(CMD_ENABLE_AUX, budget);
    let dual = read_config(budget).is_some_and(|c| c & CFG_PORT2_CLOCK_OFF == 0);
    command(CMD_DISABLE_AUX, budget);
    let port2 = dual && {
        command(CMD_TEST_AUX, budget);
        read_data(budget) == Some(0x00)
    };

    command(CMD_ENABLE_PORT1, budget);
    log!(
        "i8042: ok selftest=0x55 cfg={:#04x}->{:#04x} port1=ok port2={}",
        before,
        wanted,
        if port2 { "ok" } else if dual { "failed" } else { "absent" }
    );

    // The slowest step on a real EC, hence its own budget.
    let Some(kbd) = stage(ms(KEYBOARD), budget, "keyboard") else {
        return;
    };
    if !device_command(&[0xF5], kbd) {
        log!("i8042: kbd would not stop scanning — disabled");
        return;
    }
    // The controller translates the reply too, so the read-back names the wire
    // format outright. Refusing to decode a format we did not ask for is the
    // point: one loud line naming the observed byte beats a keyboard that types
    // nonsense on a machine we cannot single-step.
    //
    // A device that will not answer at all — the ThinkPad T14 Gen 2's EC, which
    // returns ECHO — is not a device that answers wrongly. There the
    // wire format falls back to the *only* other evidence there is: the
    // translate bit firmware itself left in the config byte. That is not a
    // weaker version of the read-back, it is Linux's entire test — `i8042.c`
    // sets `i8042_direct` from XLATE in the BIOS-left CTR and `atkbd` decodes
    // set 1 on the strength of it, sending neither `0xF0` nor even `0xF2` on a
    // portable device. Enabling a set2->set1 translator is coherent only for a
    // device emitting set 2, so firmware having enabled it *is* a statement
    // about the wire, made by the one party that had a working keyboard on it.
    // Firmware having left translation off says nothing at all, and there the
    // driver still refuses.
    let (wire, how) = match query_scancode_set(kbd) {
        SetQuery::Told(0x41) => ("set2+xlat", "readback 0x41"),
        SetQuery::Told(0x01) => ("set1 raw", "readback 0x01, translation not applied"),
        SetQuery::Told(0x43) => {
            log!("i8042: kbd DISABLED — readback 0x43 means set 1 through the set2 table");
            return;
        }
        SetQuery::Told(0x02) => {
            log!("i8042: kbd DISABLED — readback 0x02 means set 2 raw on the wire");
            return;
        }
        SetQuery::Told(other) => {
            log!("i8042: kbd DISABLED — readback {:#04x} names no known wire format", other);
            return;
        }
        SetQuery::Refused(byte) if before & CFG_TRANSLATE != 0 => {
            log!(
                "i8042: kbd will not report its scancode set (0xF0 0x00 answered {:#04x}); firmware's own cfg {:#04x} has translate on, so the wire is set 1",
                byte,
                before
            );
            ("set2+xlat", "assumed, the set query was refused")
        }
        SetQuery::Refused(byte) => {
            log!(
                "i8042: kbd DISABLED - the set query answered {:#04x} and firmware's cfg {:#04x} has translate off, so nothing says what the wire carries",
                byte,
                before
            );
            return;
        }
        SetQuery::Silent => {
            log!("i8042: kbd DISABLED - the 0xF0 0x00 set query did not complete");
            return;
        }
    };
    if !device_command(&[0xF4], kbd) {
        log!("i8042: kbd would not resume scanning — disabled");
        return;
    }

    // The TrackPoint. Failure here costs the pointer and nothing else, so
    // every step logs and falls through rather than returning.
    let aux = port2 && {
        command(CMD_ENABLE_AUX, budget);
        stage(ms(AUX_RESET), budget, "aux reset").is_some_and(|reset| {
            // 0xFF answers 0xFA, then 0xAA (BAT ok), then the device id.
            aux_command(&[0xFF], reset)
                && read_data(reset) == Some(0xAA)
                && read_data(reset).is_some()
                && aux_command(&[0xF2], reset)
                && {
                    let id = read_data(reset);
                    if id != Some(0x00) {
                        log!("i8042: aux id {:?}, not a plain 3-byte mouse — framing anyway", id);
                    }
                    true
                }
                // 100 samples/s, 8 counts/mm. No IntelliMouse knock: the
                // TrackPoint has no wheel, and a fixed 3-byte frame is what
                // makes resync trivially self-healing.
                && aux_command(&[0xF3, 0x64], reset)
                && aux_command(&[0xE8, 0x03], reset)
                && aux_command(&[0xF4], reset)
        })
    };
    if port2 && !aux {
        log!("i8042: aux init failed — no pointer");
        command(CMD_DISABLE_AUX, budget);
    }

    // Steps above leave residue in the output buffer.
    flush();

    let apic_id = crate::arch::apic::id();
    let Some(kbd_line) = ioapic::gsi_for_isa_irq(ISA_IRQ_KEYBOARD) else {
        log!("i8042: no I/O APIC covers IRQ 1 — keyboard cannot be routed");
        return;
    };
    // The physical destination field is 8 bits without interrupt remapping,
    // and `route` refuses rather than mis-route. A keyboard-less boot is
    // diagnosable; an interrupt delivered to the wrong CPU is not.
    if let Err(e) = ioapic::route(kbd_line.gsi, I8042_VECTOR, apic_id, kbd_line.trigger, kbd_line.polarity)
    {
        log!("i8042: GSI {} not routable to apic {}: {:?}", kbd_line.gsi.0, apic_id, e);
        return;
    }
    KEYBOARD_GSI.store(kbd_line.gsi.0, Ordering::Relaxed);
    // `apic_id` is this CPU's, so this is the CPU the vector was just pinned
    // to. Everything downstream that says "the pinned CPU" reads it from here.
    IRQ_CPU.store(crate::arch::percpu::cpu_id(), Ordering::Relaxed);

    let aux_line = aux.then(|| ioapic::gsi_for_isa_irq(ISA_IRQ_AUX)).flatten().filter(|l| {
        match ioapic::route(l.gsi, I8042_VECTOR, apic_id, l.trigger, l.polarity) {
            Ok(()) => true,
            Err(e) => {
                log!("i8042: GSI {} not routable to apic {}: {:?}", l.gsi.0, apic_id, e);
                false
            }
        }
    });
    if let Some(l) = aux_line {
        AUX_GSI.store(l.gsi.0, Ordering::Relaxed);
    }

    // Arm the lines with interrupts off on this CPU. The vector is pinned
    // here, so this stays the sole reader of 0x60 across the switch: a byte
    // that landed between the last flush and the unmask would otherwise sit
    // in OBF forever, because with edge delivery the controller does not
    // re-assert until it is read.
    crate::arch::cpu::disable_interrupts();
    let mut config = wanted | CFG_PORT1_IRQ;
    if aux_line.is_some() {
        // Clearing the clock-disable bit as well as setting the IRQ bit:
        // `wanted` was derived from what firmware left behind, which has
        // port 2 disabled, and writing it back would undo the 0xA8 above.
        config = (config | CFG_PORT2_IRQ) & !CFG_PORT2_CLOCK_OFF;
    }
    // The one write that arms the pin, and the only one here that had no
    // read-back. A controller that drops it still fills the output buffer and
    // still never asserts, so nothing downstream can tell: no byte reaches the
    // ring, no edge is recorded as lost, and every line below prints green.
    let wrote = write_config(config, budget);
    let readback = read_config(budget);
    if !wrote || readback != Some(config) {
        crate::arch::cpu::enable_interrupts();
        // The last step of the probe, so it is also where a budget that ran out
        // anywhere upstream surfaces: with the budget gone this write and its
        // read-back both give up instantly and look exactly like a controller
        // that dropped them. Saying which it was is the difference between
        // "your EC is slow" and "your controller is broken", on the one machine
        // that cannot be single-stepped.
        if budget_spent(budget) {
            log!(
                "i8042: DISABLED — the {}ms init budget was spent before the pin could be armed; this is a timeout, not a controller fault",
                init_budget_ms()
            );
        } else {
            match readback {
                Some(v) => log!(
                    "i8042: DISABLED — cfg {:#04x} did not take (read back {:#04x}); the pin would never assert",
                    config,
                    v
                ),
                None => log!(
                    "i8042: DISABLED — cfg {:#04x} did not take (no config byte came back); the pin would never assert",
                    config
                ),
            }
        }
        return;
    }
    let unmasked = ioapic::set_masked(kbd_line.gsi, false).is_ok();
    if let Some(l) = aux_line {
        let _ = ioapic::set_masked(l.gsi, false);
    }
    ACTIVE.store(true, Ordering::Relaxed);
    ARMED_NS.store(crate::clock::nanos_since_boot(), Ordering::Relaxed);
    HEALTH.store(HEALTH_ARMED, Ordering::Relaxed);
    handler_poll();
    crate::arch::cpu::enable_interrupts();

    log!(
        "i8042: kbd {} ({}) scanning on, GSI {} -> vec {:#04x} apic {} {}",
        wire,
        how,
        kbd_line.gsi.0,
        I8042_VECTOR,
        apic_id,
        if unmasked { "on" } else { "MASKED" }
    );
    match aux_line {
        Some(l) => log!("i8042: aux rate=100 res=8/mm, GSI {} -> vec {:#04x} apic {}", l.gsi.0, I8042_VECTOR, apic_id),
        None => log!("i8042: no pointer on the aux port"),
    }

    if crate::actuator::i8042_fault() {
        FAULT.store(true, Ordering::Relaxed);
        log!("i8042: fault injection armed");
    }
}

/// One byte from the controller if it has one, and whether the aux port sent
/// it. Never waits: a machine whose keyboard is dead, disabled or absent costs
/// the caller one `inb` and answers `None` forever.
///
/// **Only legal once every CPU is halted.** It reads port 0x60, which this
/// module's whole design makes the ISR the sole reader of, and the halt is what
/// stands in for that: there is no ISR left to race. It exists for the panic
/// console's pager, which may take no lock and so cannot reach [`PS2`]'s
/// decoders — it feeds a [`KeyDecoder`] of its own instead.
pub fn poll_byte() -> Option<(u8, bool)> {
    let status = inb(STATUS);
    if status & OBF == 0 {
        return None;
    }
    Some((inb(DATA), status & AUXB != 0))
}

/// The handler's drain loop, without the EOI. Runs with interrupts off on the
/// CPU the vector is pinned to, which is what keeps `push_isr`'s single
/// producer single.
///
/// It publishes the same record the ISR does. Bytes in the ring with no record
/// is precisely what `service` reports as a lost edge, so a silent push here
/// manufactured one on every boot that found a byte in the buffer.
fn handler_poll() {
    let timestamp = crate::clock::nanos_since_boot();
    let mut n = 0;
    while n < ISR_BURST {
        let status = inb(STATUS);
        if status & OBF == 0 {
            break;
        }
        push_isr(inb(DATA), status & AUXB != 0, crate::clock::nanos_since_boot());
        n += 1;
    }
    if n > 0 {
        crate::irq_ring::isr_publish(IrqSource::I8042, timestamp);
        crate::preempt::set_need_resched();
    }
}

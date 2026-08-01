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

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use toyos_ps2::{KeyDecoder, KeyOutcome, MouseDecoder, MouseOutcome};

use crate::arch::cpu::{inb, outb};
use crate::arch::idt::I8042_VECTOR;
use crate::irq_ring::IrqSource;
use crate::log;
use crate::sync::Lock;
use super::ioapic::{self, Gsi};

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
const COMMAND: u16 = 0x64;

const OBF: u8 = 1 << 0;
const IBF: u8 = 1 << 1;
const AUXB: u8 = 1 << 5;

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
static KEYBOARD_GSI: AtomicU32 = AtomicU32::new(u32::MAX);
static AUX_GSI: AtomicU32 = AtomicU32::new(u32::MAX);

/// The CPU the vector is pinned to. Two things are only true there: the ISR is
/// the sole reader of port 0x60, and an `irq_ring` record for this source can
/// exist at all (records are strictly per-CPU). Both are load-bearing below.
static IRQ_CPU: AtomicU32 = AtomicU32::new(u32::MAX);

fn is_irq_cpu() -> bool {
    IRQ_CPU.load(Ordering::Relaxed) == crate::arch::percpu::cpu_id()
}

/// The decoder saw a device reset. Handled on `IRQ_CPU`, whichever CPU noticed.
static AUX_RESET_PENDING: AtomicBool = AtomicBool::new(false);
static AUX_REENABLE_FAILURES: AtomicU32 = AtomicU32::new(0);

/// A device that answers at all answers a one-byte command in well under a
/// millisecond; this is interrupts-off time on the one CPU that takes the
/// vector, so it is sized for "the controller is gone", not for slowness.
const AUX_REENABLE_MS: u64 = 30;

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
    TAIL.store(tail.wrapping_add(1), Ordering::Release);
    Some((value as u8, value & AUX_FLAG != 0, (value >> TIME_SHIFT) * 1_000))
}

fn has_bytes() -> bool {
    HEAD.load(Ordering::Acquire) != TAIL.load(Ordering::Relaxed)
}

/// Under `i8042-fault`, armed at the end of a successful init so the next
/// interrupt makes the output buffer look permanently full. The only way to
/// reach the ISR's bound without a controller that is genuinely broken.
#[cfg(feature = "i8042-fault")]
static FAULT: AtomicBool = AtomicBool::new(false);

#[inline]
fn buffer_full(status: u8) -> bool {
    #[cfg(feature = "i8042-fault")]
    if FAULT.load(Ordering::Relaxed) {
        return true;
    }
    status & OBF != 0
}

/// Rust half of the pin-interrupt handler. Read the module doc before adding
/// anything to it.
pub extern "sysv64" fn handler() {
    let timestamp = crate::clock::nanos_since_boot();
    let mut n = 0;
    while n < ISR_BURST {
        let status = inb(STATUS);
        if !buffer_full(status) {
            break;
        }
        push_isr(inb(DATA), status & AUXB != 0, timestamp);
        n += 1;
    }
    if n == ISR_BURST && buffer_full(inb(STATUS)) {
        // It cannot mask the line itself — that needs the I/O APIC lock.
        QUARANTINE.store(true, Ordering::Relaxed);
    }
    if n > 0 {
        crate::irq_ring::isr_publish(IrqSource::I8042, timestamp);
        crate::preempt::set_need_resched();
    }
    crate::arch::apic::eoi();
}

struct Decoders {
    keys: KeyDecoder,
    pointer: MouseDecoder,
}

static PS2: Lock<Decoders> =
    Lock::new(Decoders { keys: KeyDecoder::new(), pointer: MouseDecoder::new() });

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
    // Unconditional, not gated on `recorded`: this is what detects a lost
    // edge and what heals it in the same pass.
    if !has_bytes() {
        return;
    }
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
        let watchers = crate::keyboard::io_uring_watchers();
        if !watchers.is_empty() {
            crate::io_uring::complete_pending_for_event(
                &watchers,
                crate::io_uring::Source::Keyboard,
            );
        }
    }
    let woke_ms = motion > 0;
    if woke_ms {
        crate::mouse::wake_waiters();
        let watchers = crate::mouse::io_uring_watchers();
        if !watchers.is_empty() {
            crate::io_uring::complete_pending_for_event(
                &watchers,
                crate::io_uring::Source::Mouse,
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
        lost = true;
    }

    while let Some((byte, aux, arrived)) = pop() {
        out.bytes += 1;
        if aux {
            match state.pointer.feed(byte, arrived) {
                MouseOutcome::Packet { buttons, dx, dy } => {
                    if crate::mouse::handle_motion(
                        crate::mouse::PointerSource::PS2,
                        buttons,
                        crate::mouse::Motion::Relative { dx, dy },
                        0,
                    ) {
                        out.motion += 1;
                    }
                }
                MouseOutcome::Reset => out.aux_reset = true,
                MouseOutcome::None => {}
            }
            continue;
        }
        match state.keys.feed(byte) {
            KeyOutcome::Key { usage, pressed } => {
                if crate::keyboard::handle_key(usage, pressed) {
                    out.keys += 1;
                }
            }
            KeyOutcome::Lost => lost = true,
            KeyOutcome::None => {}
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
    out
}

/// A controller producing bytes faster than the ISR's bound can drain them.
/// One masked line and a dead keyboard, never a spinning CPU.
fn quarantine() {
    QUARANTINE.store(false, Ordering::Relaxed);
    ACTIVE.store(false, Ordering::Relaxed);
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
#[cfg(feature = "i8042-trace")]
fn trace_drain(bytes: usize, keys: usize, motion: usize, woke_kb: bool, woke_ms: bool) {
    log!(
        "i8042: drain bytes={} keys={} motion={} woke_kb={} woke_ms={}",
        bytes,
        keys,
        motion,
        u8::from(woke_kb),
        u8::from(woke_ms)
    );
}

#[cfg(not(feature = "i8042-trace"))]
fn trace_drain(_b: usize, _k: usize, _m: usize, _wkb: bool, _wms: bool) {}

// Polled init.
//
// Nothing below runs after `ACTIVE` is set, and all of it runs with the
// controller's interrupt bits clear, so the ISR cannot be racing it.

fn deadline(millis: u64) -> u64 {
    crate::clock::nanos_since_boot() + millis * 1_000_000
}

/// A stage's own deadline, never past the whole probe's. Each stage's number is
/// what that step is worth waiting *from here*; the budget is what the boot is
/// worth spending in total, and without the clamp the stages add up instead of
/// fitting inside it.
fn stage(millis: u64, budget: u64) -> u64 {
    deadline(millis).min(budget)
}

fn wait_writable(deadline: u64) -> bool {
    while inb(STATUS) & IBF != 0 {
        if crate::clock::nanos_since_boot() >= deadline {
            return false;
        }
    }
    true
}

fn read_data(deadline: u64) -> Option<u8> {
    while inb(STATUS) & OBF == 0 {
        if crate::clock::nanos_since_boot() >= deadline {
            return None;
        }
    }
    Some(inb(DATA))
}

fn command(cmd: u8, deadline: u64) -> bool {
    wait_writable(deadline) && {
        outb(COMMAND, cmd);
        true
    }
}

fn write_data(byte: u8, deadline: u64) -> bool {
    wait_writable(deadline) && {
        outb(DATA, byte);
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
        if inb(STATUS) & OBF == 0 {
            return true;
        }
        inb(DATA);
    }
    inb(STATUS) & OBF == 0
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
        let budget = deadline(AUX_REENABLE_MS);
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

pub fn init(rsdp_addr: u64) {
    // Three answers, and only one of them is firmware's. A refusal from the
    // parser says nothing about the hardware — it says the table could not be
    // believed — so it must never be spelled the way "absent" is, and it must
    // not stop the probe. On a laptop with a dead keyboard this line is the
    // whole question: did firmware tell us not to touch the controller, or did
    // we decide that for ourselves out of a table we could not read?
    match crate::drivers::acpi::iapc_boot_arch(rsdp_addr) {
        // Bit 1 is "8042 present", and it is only meaningful from revision 2.
        // On a machine that clears it, 0x60/0x64 may be decoded by something
        // else, so they are never touched at all.
        Ok((revision, flags)) if revision >= 2 && flags & 0x0002 == 0 => {
            log!("i8042: absent (FADT rev {} iapc_boot_arch={:#06x})", revision, flags);
            return;
        }
        Ok((revision, flags)) if revision < 2 => {
            log!("i8042: FADT rev {} says nothing (flags {:#06x}), probing", revision, flags);
        }
        Ok(_) => {}
        Err(e) => log!("i8042: no trustworthy FADT ({e:?}) — firmware said nothing either way, probing"),
    }

    // The whole probe, from the first port touch to the last. Every stage
    // clamps to it, so a machine whose EC answers everything slowly costs the
    // boot this and not the sum of the stages.
    let budget = deadline(1500);

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

    // On a machine with no controller `inb(0x64)` returns 0xFF from a
    // floating bus and every status bit reads set. The self-test is what
    // separates "controller" from "nothing".
    let selftest_deadline = stage(500, budget);
    command(CMD_SELF_TEST, selftest_deadline);
    match read_data(selftest_deadline) {
        Some(0x55) => {}
        other => {
            log!("i8042: absent (self-test {:?}, 500ms) — no PS/2 input", other);
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
    // waiting out a 600 ms device-reset timeout on every machine without one.
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
    let kbd = stage(750, budget);
    if !device_command(&[0xF5], kbd) {
        log!("i8042: kbd would not stop scanning — disabled");
        return;
    }
    if !device_command(&[0xF0, 0x02], kbd) {
        log!("i8042: kbd refused scancode set 2 — disabled");
        return;
    }
    if !device_command(&[0xF0, 0x00], kbd) {
        log!("i8042: kbd refused the set read-back — disabled");
        return;
    }
    let Some(mode) = read_data(kbd) else {
        log!("i8042: kbd read-back never answered — disabled");
        return;
    };
    // The controller translates the reply too, so the four answers fully
    // determine the wire format. Refusing to decode a format we did not ask
    // for is the point: one loud line naming the observed byte beats a
    // keyboard that types nonsense on a machine we cannot single-step.
    match mode {
        0x41 => {}
        0x01 => log!("i8042: kbd already set 1, translation off — decodable, unexpected"),
        0x43 => {
            log!("i8042: kbd DISABLED — readback 0x43 means set 1 through the set2 table");
            return;
        }
        0x02 => {
            log!("i8042: kbd DISABLED — readback 0x02 means set 2 raw on the wire");
            return;
        }
        other => {
            log!("i8042: kbd DISABLED — readback {:#04x} names no known wire format", other);
            return;
        }
    }
    if !device_command(&[0xF4], kbd) {
        log!("i8042: kbd would not resume scanning — disabled");
        return;
    }

    // The TrackPoint. Failure here costs the pointer and nothing else, so
    // every step logs and falls through rather than returning.
    let aux = port2 && {
        command(CMD_ENABLE_AUX, budget);
        let reset = stage(600, budget);
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
        return;
    }
    let unmasked = ioapic::set_masked(kbd_line.gsi, false).is_ok();
    if let Some(l) = aux_line {
        let _ = ioapic::set_masked(l.gsi, false);
    }
    ACTIVE.store(true, Ordering::Relaxed);
    handler_poll();
    crate::arch::cpu::enable_interrupts();

    log!(
        "i8042: kbd set2+xlat (readback {:#04x}) scanning on, GSI {} -> vec {:#04x} apic {} {}",
        mode,
        kbd_line.gsi.0,
        I8042_VECTOR,
        apic_id,
        if unmasked { "on" } else { "MASKED" }
    );
    match aux_line {
        Some(l) => log!("i8042: aux rate=100 res=8/mm, GSI {} -> vec {:#04x} apic {}", l.gsi.0, I8042_VECTOR, apic_id),
        None => log!("i8042: no pointer on the aux port"),
    }

    #[cfg(feature = "i8042-fault")]
    {
        FAULT.store(true, Ordering::Relaxed);
        log!("i8042: fault injection armed");
    }
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
        push_isr(inb(DATA), status & AUXB != 0, timestamp);
        n += 1;
    }
    if n > 0 {
        crate::irq_ring::isr_publish(IrqSource::I8042, timestamp);
        crate::preempt::set_need_resched();
    }
}

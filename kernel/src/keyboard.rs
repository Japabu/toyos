//! The machine's one keyboard, whichever keyboards it has.
//!
//! Every driver feeds transitions in here and nothing else comes out: a HID
//! usage, a direction, and the modifier mask every keyboard in the machine
//! adds up to. What a key *types* is a layout, a dead-key state and a
//! terminal's escape vocabulary, none of which the kernel has any business
//! knowing — `toyos-keymap` is that, in userland, one instance per surface.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::io_uring::RingId;
use crate::sync::Lock;
pub use toyos_abi::input::{RawKeyEvent, MOD_SHIFT, MOD_CTRL, MOD_ALT, MOD_GUI, MOD_RELEASED};

static KEY_BUF: Lock<VecDeque<RawKeyEvent>> = Lock::new(VecDeque::new());
static IO_URING_WATCHERS: Lock<Vec<RingId>> = Lock::new(Vec::new());

/// How many transitions the kernel holds for a reader that is not reading.
///
/// Policy, not physics: nothing about a keyboard says 512, and the queue used
/// to have no bound at all, so a machine whose only reader had exited grew it
/// for as long as anyone leaned on a key. **The overflow policy is
/// drop-oldest** — what is worth keeping when nobody is reading is what was
/// typed most recently, and a queue that refuses new events instead would
/// answer the eventual reader with the first 512 transitions after it stopped
/// and none of the ones since.
///
/// 1 KiB at two bytes an event, and about ten seconds of a stuck typematic
/// repeat.
pub const MAX_QUEUED_EVENTS: usize = 512;

/// Ctrl+Alt+D is recorded here, not acted on. `handle_key` runs under whichever
/// driver's guard produced the transition — `PS2` on the i8042 path, `XHCI` on
/// the USB one — and `sched::dump` asks every CPU for its parked tasks and
/// waits for them. The scheduler pass consumes this after every device
/// service, with no driver lock held.
static DUMP_REQUESTED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Consume a pending Ctrl+Alt+D. Called from `drain_irqs` and nowhere else.
pub fn take_dump_request() -> bool {
    DUMP_REQUESTED.swap(false, core::sync::atomic::Ordering::Relaxed)
}

/// Which HID usages are currently down, one bit each, across every keyboard
/// the machine has. Central because the modifier state is derived from it:
/// Shift held on one keyboard and a letter typed on another must produce a
/// capital, which per-driver modifier state cannot express.
///
/// Keyed by usage, not by source. Holding a modifier on two keyboards and
/// releasing one therefore drops it — accepted, because refcounting per
/// source reintroduces exactly the per-driver state this removes.
static HELD: Lock<[u64; 4]> = Lock::new([0; 4]);

pub fn add_io_uring_watcher(id: RingId) {
    let mut w = IO_URING_WATCHERS.lock();
    if !w.contains(&id) { w.push(id); }
}

pub fn remove_io_uring_watcher(id: RingId) {
    IO_URING_WATCHERS.lock().retain(|&x| x != id);
}

/// Wake every thread blocked on keyboard input.
pub fn wake_waiters() {
    crate::sched::waitqs::wake_device(
        &crate::sched::waitqs::KEYBOARD,
        &crate::sched::waitqs::KEYBOARD_WATCH,
    );
}

pub fn io_uring_watchers() -> Vec<RingId> {
    IO_URING_WATCHERS.lock().clone()
}

fn is_held(held: &[u64; 4], usage: u8) -> bool {
    held[usage as usize / 64] & (1 << (usage % 64)) != 0
}

fn modifiers_of(held: &[u64; 4]) -> u8 {
    let m = |a: u8, b: u8| is_held(held, a) || is_held(held, b);
    (if m(0xE1, 0xE5) { MOD_SHIFT } else { 0 })
        | (if m(0xE0, 0xE4) { MOD_CTRL } else { 0 })
        | (if m(0xE2, 0xE6) { MOD_ALT } else { 0 })
        | (if m(0xE3, 0xE7) { MOD_GUI } else { 0 })
}

/// The modifier bitmask every keyboard in the machine adds up to.
/// The held-modifier bitmask, for `test-input-merge`'s assertion that
/// `release_all` really released them. No shipping caller.
#[cfg(feature = "boot-actuators")]
pub fn modifiers() -> u8 {
    modifiers_of(&HELD.lock())
}

/// Queue one key transition. Returns true iff an event was queued.
///
/// The single production path from a keyboard of any kind into `KEY_BUF`. It
/// owns the held-set (and therefore the modifier mask) and the Ctrl+Alt+D
/// hook — both of which must be central rather than per-driver, and the
/// second of which a naive split silently leaves on whichever path it started
/// on.
///
/// A transition to the state a usage is already in queues nothing: that is
/// what makes a PS/2 typematic repeat, which is a make with no intervening
/// break, behave exactly like a USB keyboard's unchanged report.
pub fn handle_key(usage: u8, pressed: bool) -> bool {
    if usage == 0 {
        return false;
    }
    let modifiers = {
        let mut held = HELD.lock();
        if is_held(&held, usage) == pressed {
            return false;
        }
        let word = &mut held[usage as usize / 64];
        let bit = 1u64 << (usage % 64);
        if pressed { *word |= bit } else { *word &= !bit }
        modifiers_of(&held)
    };

    // Ctrl+Alt+D (HID 0x07) → dump blocked threads. On the HID usage and not
    // on a character, so it is the same three physical keys under every
    // layout. Recorded, not run: every caller of this function holds its
    // driver's guard.
    if pressed && modifiers & MOD_CTRL != 0 && modifiers & MOD_ALT != 0 && usage == 0x07 {
        DUMP_REQUESTED.store(true, core::sync::atomic::Ordering::Relaxed);
        return false;
    }

    let mut buf = KEY_BUF.lock();
    if buf.len() >= MAX_QUEUED_EVENTS {
        buf.pop_front();
    }
    buf.push_back(RawKeyEvent {
        keycode: usage,
        modifiers: if pressed { modifiers } else { modifiers | MOD_RELEASED },
    });
    true
}

/// Throw away everything queued. Called when the device changes hands: what
/// was typed while nobody was reading belongs to nobody, and handing it to the
/// next claimant gives one program another's keystrokes.
pub fn discard_queued() {
    KEY_BUF.lock().clear();
}

/// Synthesise a release for every held usage. The self-heal for a keyboard
/// that reset behind our back: without it a modifier that was down at the
/// reset stays down for the rest of the boot.
pub fn release_all() -> usize {
    let held = *HELD.lock();
    let mut n = 0;
    for usage in 0..=u8::MAX {
        if is_held(&held, usage) && handle_key(usage, false) {
            n += 1;
        }
    }
    n
}

/// Process a HID boot protocol keyboard report (8 bytes). Returns the number
/// of events queued — the caller wakes only on a non-zero count, so a report
/// identical to the last one costs nothing.
///
/// `prev` belongs to the device, and must: a report is a *snapshot* of one
/// keyboard, so diffing it against another one's says every key the first holds
/// was just released. A dongle that exposes a HID keyboard interface for media
/// keys — very common — would otherwise flap a real keyboard's held key at the
/// combined polling rate. `HELD` stays central because it is the union across
/// keyboards; this is per-device by the same argument.
pub fn handle_report(state: &mut [u8; 8], report: &[u8]) -> usize {
    let prev = *state;
    state.copy_from_slice(&report[..8]);
    let mut queued = 0;

    // The boot protocol puts modifiers in report[0] as a bitmask, not as
    // usages in report[2..8]. Discrete events are synthesized so apps (DOOM)
    // that want individual modifier transitions get them.
    const MOD_BITS: [(u8, u8); 8] = [
        (0x01, 0xE0),
        (0x02, 0xE1),
        (0x04, 0xE2),
        (0x08, 0xE3),
        (0x10, 0xE4),
        (0x20, 0xE5),
        (0x40, 0xE6),
        (0x80, 0xE7),
    ];
    for &(bit, usage) in &MOD_BITS {
        let now = report[0] & bit != 0;
        if (prev[0] & bit != 0) != now && handle_key(usage, now) {
            queued += 1;
        }
    }

    for i in 2..8 {
        let usage = prev[i];
        if usage >= 4 && !report[2..8].contains(&usage) && handle_key(usage, false) {
            queued += 1;
        }
    }

    for i in 2..8 {
        let usage = report[i];
        if usage >= 4 && !prev[2..8].contains(&usage) && handle_key(usage, true) {
            queued += 1;
        }
    }

    queued
}

pub fn has_data() -> bool {
    !KEY_BUF.lock().is_empty()
}

pub fn try_read_event() -> Option<RawKeyEvent> {
    KEY_BUF.lock().pop_front()
}

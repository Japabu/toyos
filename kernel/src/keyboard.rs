use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::io_uring::RingId;
use crate::sync::Lock;
pub use toyos_abi::input::{RawKeyEvent, MOD_SHIFT, MOD_CTRL, MOD_ALT, MOD_GUI, MOD_RELEASED};

static KEY_BUF: Lock<VecDeque<RawKeyEvent>> = Lock::new(VecDeque::new());
static IO_URING_WATCHERS: Lock<Vec<RingId>> = Lock::new(Vec::new());

/// Ctrl+Alt+D is recorded here, not acted on. `handle_key` runs under whichever
/// driver's guard produced the transition — `PS2` on the i8042 path, `XHCI` on
/// the USB one — and `dump_blocked` walks the scheduler and logs a line per
/// parked thread. The scheduler pass consumes this after every device service,
/// with no driver lock held.
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
    crate::sched::waitqs::wake_all(&crate::sched::waitqs::KEYBOARD);
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
pub fn modifiers() -> u8 {
    modifiers_of(&HELD.lock())
}

/// Queue one key transition. Returns true iff an event was queued.
///
/// The single production path from a keyboard of any kind into `KEY_BUF`. It
/// owns the held-set (and therefore the modifier mask), the translation to
/// bytes, and the Ctrl+Alt+D hook — all three of which must be central
/// rather than per-driver, and the last of which a naive split silently
/// leaves on whichever path it started on.
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

    let shift = modifiers & MOD_SHIFT != 0;
    let ctrl = modifiers & MOD_CTRL != 0;
    let alt = modifiers & MOD_ALT != 0;

    // Ctrl+Alt+D (HID 0x07) → dump blocked threads. Recorded, not run: every
    // caller of this function holds its driver's guard.
    if pressed && ctrl && alt && usage == 0x07 {
        DUMP_REQUESTED.store(true, core::sync::atomic::Ordering::Relaxed);
        return false;
    }

    let mut event = RawKeyEvent {
        keycode: usage,
        modifiers: if pressed { modifiers } else { modifiers | MOD_RELEASED },
        len: 0,
        translated: [0; 5],
    };
    if pressed {
        translate(usage, shift, ctrl, alt, &mut event);
    }
    KEY_BUF.lock().push_back(event);
    true
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

fn translate(keycode: u8, shift: bool, ctrl: bool, alt: bool, event: &mut RawKeyEvent) {
    let escape_seq: Option<&[u8]> = match keycode {
        0x4F => Some(b"\x1B[C"),  // Right
        0x50 => Some(b"\x1B[D"),  // Left
        0x51 => Some(b"\x1B[B"),  // Down
        0x52 => Some(b"\x1B[A"),  // Up
        0x4A => Some(b"\x1B[H"),  // Home
        0x4D => Some(b"\x1B[F"),  // End
        0x4C => Some(b"\x1B[3~"), // Delete
        0x4B => Some(b"\x1B[5~"), // Page Up
        0x4E => Some(b"\x1B[6~"), // Page Down
        _ => None,
    };

    if let Some(seq) = escape_seq {
        let n = seq.len().min(5);
        event.translated[..n].copy_from_slice(&seq[..n]);
        event.len = n as u8;
        return;
    }

    if ctrl && (0x04..=0x1D).contains(&keycode) {
        event.translated[0] = keycode - 0x04 + 1;
        event.len = 1;
        return;
    }

    if let Some(bytes) = layout_lookup(keycode, shift, alt) {
        let n = bytes.len().min(5);
        event.translated[..n].copy_from_slice(&bytes[..n]);
        event.len = n as u8;
    }
}

pub fn has_data() -> bool {
    !KEY_BUF.lock().is_empty()
}

pub fn try_read_event() -> Option<RawKeyEvent> {
    KEY_BUF.lock().pop_front()
}

pub struct KeyEntry {
    pub normal: &'static [u8],
    pub shift: &'static [u8],
    pub option: &'static [u8],
    pub shift_option: &'static [u8],
}

/// HID usage codes 0x04..=0x38 mapped to characters (index = usage - 0x04).
pub struct Layout {
    pub name: &'static str,
    pub keys: [KeyEntry; 53],
    /// HID 0x64: the ISO key between left Shift and Y/Z on ISO keyboards.
    pub iso_key: KeyEntry,
}

pub fn layout_lookup(usage: u8, shift: bool, alt: bool) -> Option<&'static [u8]> {
    let entry = if (0x04..=0x38).contains(&usage) {
        &active_layout().keys[(usage - 0x04) as usize]
    } else if usage == 0x64 {
        &active_layout().iso_key
    } else {
        return None;
    };
    let bytes = match (shift, alt) {
        (false, false) => entry.normal,
        (true, false) => entry.shift,
        (false, true) => entry.option,
        (true, true) => entry.shift_option,
    };
    if bytes.is_empty() { None } else { Some(bytes) }
}

static ACTIVE_LAYOUT: Lock<usize> = Lock::new(0);

const LAYOUTS: &[&Layout] = &[&US_QWERTY, &GERMAN, &SWISS_GERMAN_MAC];

fn active_layout() -> &'static Layout {
    LAYOUTS[*ACTIVE_LAYOUT.lock()]
}

pub fn set_layout(name: &str) -> bool {
    for (i, layout) in LAYOUTS.iter().enumerate() {
        if layout.name == name {
            *ACTIVE_LAYOUT.lock() = i;
            return true;
        }
    }
    false
}

pub fn layout_name() -> &'static str {
    active_layout().name
}

const K: KeyEntry = KeyEntry { normal: &[], shift: &[], option: &[], shift_option: &[] };

const fn key(normal: &'static [u8], shift: &'static [u8]) -> KeyEntry {
    KeyEntry { normal, shift, option: &[], shift_option: &[] }
}

const fn key_opt(
    normal: &'static [u8],
    shift: &'static [u8],
    option: &'static [u8],
) -> KeyEntry {
    KeyEntry { normal, shift, option, shift_option: &[] }
}

const fn key_full(
    normal: &'static [u8],
    shift: &'static [u8],
    option: &'static [u8],
    shift_option: &'static [u8],
) -> KeyEntry {
    KeyEntry { normal, shift, option, shift_option }
}

const US_QWERTY: Layout = Layout {
    name: "us",
    iso_key: K,
    keys: [
        key(b"a", b"A"),
        key(b"b", b"B"),
        key(b"c", b"C"),
        key(b"d", b"D"),
        key(b"e", b"E"),
        key(b"f", b"F"),
        key(b"g", b"G"),
        key(b"h", b"H"),
        key(b"i", b"I"),
        key(b"j", b"J"),
        key(b"k", b"K"),
        key(b"l", b"L"),
        key(b"m", b"M"),
        key(b"n", b"N"),
        key(b"o", b"O"),
        key(b"p", b"P"),
        key(b"q", b"Q"),
        key(b"r", b"R"),
        key(b"s", b"S"),
        key(b"t", b"T"),
        key(b"u", b"U"),
        key(b"v", b"V"),
        key(b"w", b"W"),
        key(b"x", b"X"),
        key(b"y", b"Y"),
        key(b"z", b"Z"),
        key(b"1", b"!"),
        key(b"2", b"@"),
        key(b"3", b"#"),
        key(b"4", b"$"),
        key(b"5", b"%"),
        key(b"6", b"^"),
        key(b"7", b"&"),
        key(b"8", b"*"),
        key(b"9", b"("),
        key(b"0", b")"),
        key(b"\r", b"\r"),
        key(&[0x1B], &[0x1B]),
        key(&[0x08], &[0x08]),
        key(b"\t", b"\t"),
        key(b" ", b" "),
        key(b"-", b"_"),
        key(b"=", b"+"),
        key(b"[", b"{"),
        key(b"]", b"}"),
        key(b"\\", b"|"),
        K,
        key(b";", b":"),
        key(b"'", b"\""),
        key(b"`", b"~"),
        key(b",", b"<"),
        key(b".", b">"),
        key(b"/", b"?"),
    ],
};

const UUML_L: &[u8] = "ü".as_bytes();
const UUML_U: &[u8] = "Ü".as_bytes();
const OUML_L: &[u8] = "ö".as_bytes();
const OUML_U: &[u8] = "Ö".as_bytes();
const AUML_L: &[u8] = "ä".as_bytes();
const AUML_U: &[u8] = "Ä".as_bytes();
const EACU_L: &[u8] = "é".as_bytes();
const EGRV_L: &[u8] = "è".as_bytes();
const AGRV_L: &[u8] = "à".as_bytes();
const CCEDIL: &[u8] = "ç".as_bytes();
const SECT:   &[u8] = "§".as_bytes();
const DEGREE: &[u8] = "°".as_bytes();
const POUND:  &[u8] = "£".as_bytes();
const DIAER:  &[u8] = "¨".as_bytes();
const SZLIG:  &[u8] = "ß".as_bytes();
const EURO:   &[u8] = "€".as_bytes();
const MICRO:  &[u8] = "µ".as_bytes();
const ACUTE:  &[u8] = "´".as_bytes();

const GERMAN: Layout = Layout {
    name: "de",
    iso_key: key_opt(b"<", b">", b"|"),          // 0x64 (ISO key between left Shift and Y)
    keys: [
        key(b"a", b"A"),                            // 0x04
        key(b"b", b"B"),                            // 0x05
        key(b"c", b"C"),                            // 0x06
        key(b"d", b"D"),                            // 0x07
        key_opt(b"e", b"E", EURO),                  // 0x08
        key(b"f", b"F"),                            // 0x09
        key(b"g", b"G"),                            // 0x0A
        key(b"h", b"H"),                            // 0x0B
        key(b"i", b"I"),                            // 0x0C
        key(b"j", b"J"),                            // 0x0D
        key(b"k", b"K"),                            // 0x0E
        key(b"l", b"L"),                            // 0x0F
        key_opt(b"m", b"M", MICRO),                 // 0x10
        key(b"n", b"N"),                            // 0x11
        key(b"o", b"O"),                            // 0x12
        key(b"p", b"P"),                            // 0x13
        key_opt(b"q", b"Q", b"@"),                  // 0x14
        key(b"r", b"R"),                            // 0x15
        key(b"s", b"S"),                            // 0x16
        key(b"t", b"T"),                            // 0x17
        key(b"u", b"U"),                            // 0x18
        key(b"v", b"V"),                            // 0x19
        key(b"w", b"W"),                            // 0x1A
        key(b"x", b"X"),                            // 0x1B
        key(b"z", b"Z"),                            // 0x1C (QWERTZ: Y key types Z)
        key(b"y", b"Y"),                            // 0x1D (QWERTZ: Z key types Y)
        key(b"1", b"!"),                            // 0x1E
        key(b"2", b"\""),                           // 0x1F
        key_opt(b"3", SECT, b"#"),                  // 0x20 (note: shifted on german has no #)
        key(b"4", b"$"),                            // 0x21
        key(b"5", b"%"),                            // 0x22
        key(b"6", b"&"),                            // 0x23
        key_opt(b"7", b"/", b"{"),                  // 0x24
        key_opt(b"8", b"(", b"["),                  // 0x25
        key_opt(b"9", b")", b"]"),                  // 0x26
        key_opt(b"0", b"=", b"}"),                  // 0x27
        key(b"\r", b"\r"),                          // 0x28
        key(&[0x1B], &[0x1B]),                      // 0x29
        key(&[0x08], &[0x08]),                      // 0x2A
        key(b"\t", b"\t"),                          // 0x2B
        key(b" ", b" "),                            // 0x2C
        key_opt(SZLIG, b"?", b"\\"),                // 0x2D
        key(ACUTE, b"`"),                           // 0x2E
        key(UUML_L, UUML_U),                       // 0x2F
        key_opt(b"+", b"*", b"~"),                  // 0x30
        key(b"#", b"'"),                            // 0x31
        K,                                          // 0x32
        key(OUML_L, OUML_U),                        // 0x33
        key(AUML_L, AUML_U),                        // 0x34
        key(b"^", DEGREE),                          // 0x35
        key(b",", b";"),                            // 0x36
        key(b".", b":"),                            // 0x37
        key(b"-", b"_"),                            // 0x38
    ],
};

const SWISS_GERMAN_MAC: Layout = Layout {
    name: "swiss-german-mac",
    iso_key: key(SECT, DEGREE),              // 0x64 (top-left key on Mac ISO)
    keys: [
        key(b"a", b"A"),                        // 0x04
        key(b"b", b"B"),                        // 0x05
        key(b"c", b"C"),                        // 0x06
        key(b"d", b"D"),                        // 0x07
        key(b"e", b"E"),                        // 0x08
        key(b"f", b"F"),                        // 0x09
        key_opt(b"g", b"G", b"@"),              // 0x0A
        key(b"h", b"H"),                        // 0x0B
        key(b"i", b"I"),                        // 0x0C
        key(b"j", b"J"),                        // 0x0D
        key(b"k", b"K"),                        // 0x0E
        key(b"l", b"L"),                        // 0x0F
        key(b"m", b"M"),                        // 0x10
        key_opt(b"n", b"N", b"~"),              // 0x11
        key(b"o", b"O"),                        // 0x12
        key(b"p", b"P"),                        // 0x13
        key(b"q", b"Q"),                        // 0x14
        key(b"r", b"R"),                        // 0x15
        key(b"s", b"S"),                        // 0x16
        key(b"t", b"T"),                        // 0x17
        key(b"u", b"U"),                        // 0x18
        key(b"v", b"V"),                        // 0x19
        key(b"w", b"W"),                        // 0x1A
        key(b"x", b"X"),                        // 0x1B
        key(b"z", b"Z"),                        // 0x1C (QWERTZ)
        key(b"y", b"Y"),                        // 0x1D (QWERTZ)
        key(b"1", b"+"),                        // 0x1E
        key(b"2", b"\""),                       // 0x1F
        key_opt(b"3", b"*", b"#"),              // 0x20
        key(b"4", CCEDIL),                      // 0x21
        key_opt(b"5", b"%", b"["),              // 0x22
        key_opt(b"6", b"&", b"]"),              // 0x23
        key_full(b"7", b"/", b"|", b"\\"),      // 0x24
        key_opt(b"8", b"(", b"{"),              // 0x25
        key_opt(b"9", b")", b"}"),              // 0x26
        key(b"0", b"="),                        // 0x27
        key(b"\r", b"\r"),                      // 0x28
        key(&[0x1B], &[0x1B]),                  // 0x29
        key(&[0x08], &[0x08]),                  // 0x2A
        key(b"\t", b"\t"),                      // 0x2B
        key(b" ", b" "),                        // 0x2C
        key(b"'", b"?"),                        // 0x2D
        key(b"^", b"`"),                        // 0x2E
        key(UUML_L, EGRV_L),                   // 0x2F
        key(DIAER, b"!"),                       // 0x30
        key(b"$", POUND),                       // 0x31
        K,                                      // 0x32 (not used on Mac ISO)
        key(OUML_L, EACU_L),                    // 0x33
        key(AUML_L, AGRV_L),                    // 0x34
        key(b"<", b">"),                        // 0x35 (between left Shift and Y on Mac ISO)
        key(b",", b";"),                        // 0x36
        key(b".", b":"),                        // 0x37
        key(b"-", b"_"),                        // 0x38
    ],
};

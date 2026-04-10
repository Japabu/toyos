use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::io_uring::RingId;
use crate::sync::Lock;
pub use toyos_abi::input::{RawKeyEvent, MOD_SHIFT, MOD_CTRL, MOD_ALT, MOD_GUI, MOD_RELEASED};

static KEY_BUF: Lock<VecDeque<RawKeyEvent>> = Lock::new(VecDeque::new());
static PREV_REPORT: Lock<[u8; 8]> = Lock::new([0; 8]);
static IO_URING_WATCHERS: Lock<Vec<RingId>> = Lock::new(Vec::new());

pub fn add_io_uring_watcher(id: RingId) {
    let mut w = IO_URING_WATCHERS.lock();
    if !w.contains(&id) { w.push(id); }
}

pub fn remove_io_uring_watcher(id: RingId) {
    IO_URING_WATCHERS.lock().retain(|&x| x != id);
}

pub fn io_uring_watchers() -> Vec<RingId> {
    IO_URING_WATCHERS.lock().clone()
}

/// Process a HID boot protocol keyboard report (8 bytes).
pub fn handle_report(report: &[u8]) {
    let hid_modifiers = report[0];
    let shift = (hid_modifiers & 0x22) != 0;
    let ctrl = (hid_modifiers & 0x11) != 0;
    let alt = (hid_modifiers & 0x44) != 0;
    let gui = (hid_modifiers & 0x88) != 0;
    let prev = *PREV_REPORT.lock();

    let modifiers = if shift { MOD_SHIFT } else { 0 }
        | if ctrl { MOD_CTRL } else { 0 }
        | if alt { MOD_ALT } else { 0 }
        | if gui { MOD_GUI } else { 0 };

    let mut buf = KEY_BUF.lock();

    // Modifier key press/release events.
    // HID boot protocol puts modifiers in report[0] as a bitmask, not as
    // keycodes in report[2..8]. We synthesize discrete key events so apps
    // (e.g. DOOM) that need individual modifier press/release work correctly.
    let prev_mods = prev[0];
    const MOD_BITS: [(u8, u8, u8); 8] = [
        (0x01, 0xE0, MOD_CTRL),   // Left Ctrl
        (0x02, 0xE1, MOD_SHIFT),  // Left Shift
        (0x04, 0xE2, MOD_ALT),    // Left Alt
        (0x08, 0xE3, MOD_GUI),    // Left GUI
        (0x10, 0xE4, MOD_CTRL),   // Right Ctrl
        (0x20, 0xE5, MOD_SHIFT),  // Right Shift
        (0x40, 0xE6, MOD_ALT),    // Right Alt
        (0x80, 0xE7, MOD_GUI),    // Right GUI
    ];
    for &(bit, keycode, _) in &MOD_BITS {
        let was = prev_mods & bit != 0;
        let now = hid_modifiers & bit != 0;
        if was && !now {
            buf.push_back(RawKeyEvent {
                keycode,
                modifiers: modifiers | MOD_RELEASED,
                len: 0,
                translated: [0; 5],
            });
        } else if !was && now {
            buf.push_back(RawKeyEvent {
                keycode,
                modifiers,
                len: 0,
                translated: [0; 5],
            });
        }
    }

    // Key releases: keys in prev report but not in current report.
    for i in 2..8 {
        let keycode = prev[i];
        if keycode < 4 { continue; }
        if !report[2..8].contains(&keycode) {
            buf.push_back(RawKeyEvent {
                keycode,
                modifiers: modifiers | MOD_RELEASED,
                len: 0,
                translated: [0; 5],
            });
        }
    }

    // Key presses: keys in current report but not in prev report.
    for i in 2..8 {
        let keycode = report[i];
        if keycode < 4 { continue; }
        if !prev[2..8].contains(&keycode) {
            // Ctrl+Alt+D (HID 0x07) → dump blocked threads
            if ctrl && alt && keycode == 0x07 {
                crate::scheduler::dump_blocked();
                continue;
            }
            let mut event = RawKeyEvent {
                keycode,
                modifiers,
                len: 0,
                translated: [0; 5],
            };
            translate(keycode, shift, ctrl, alt, &mut event);
            buf.push_back(event);
        }
    }

    drop(buf);
    PREV_REPORT.lock().copy_from_slice(&report[..8]);
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

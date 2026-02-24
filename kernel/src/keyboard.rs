use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use crate::sync::Lock;

static KEY_BUF: Lock<VecDeque<u8>> = Lock::new(VecDeque::new());
static PREV_REPORT: Lock<[u8; 8]> = Lock::new([0; 8]);

/// Process a HID boot protocol keyboard report (8 bytes).
pub fn handle_report(report: &[u8]) {
    let modifiers = report[0];
    let shift = (modifiers & 0x22) != 0;
    let ctrl = (modifiers & 0x11) != 0;
    let alt = (modifiers & 0x44) != 0;
    let prev = *PREV_REPORT.lock();

    for i in 2..8 {
        let keycode = report[i];
        if keycode < 4 { continue; }
        if !prev[2..8].contains(&keycode) {
            match keycode {
                0x4F => { handle_key(0x1B); handle_key(b'['); handle_key(b'C'); }
                0x50 => { handle_key(0x1B); handle_key(b'['); handle_key(b'D'); }
                0x51 => { handle_key(0x1B); handle_key(b'['); handle_key(b'B'); }
                0x52 => { handle_key(0x1B); handle_key(b'['); handle_key(b'A'); }
                0x4A => { handle_key(0x1B); handle_key(b'['); handle_key(b'H'); }
                0x4D => { handle_key(0x1B); handle_key(b'['); handle_key(b'F'); }
                0x4C => { handle_key(0x1B); handle_key(b'['); handle_key(b'3'); handle_key(b'~'); }
                0x4B => { handle_key(0x1B); handle_key(b'['); handle_key(b'5'); handle_key(b'~'); }
                0x4E => { handle_key(0x1B); handle_key(b'['); handle_key(b'6'); handle_key(b'~'); }
                _ => {
                    if ctrl && (0x04..=0x1D).contains(&keycode) {
                        handle_key(keycode - 0x04 + 1);
                    } else if let Some(bytes) = layout_lookup(keycode, shift, alt) {
                        for &b in bytes {
                            handle_key(b);
                        }
                    }
                }
            }
        }
    }

    PREV_REPORT.lock().copy_from_slice(&report[..8]);
}

pub fn handle_key(byte: u8) {
    if byte != 0 {
        KEY_BUF.lock().push_back(byte);
    }
}

pub fn has_data() -> bool {
    !KEY_BUF.lock().is_empty()
}

pub fn try_read_char() -> Option<u8> {
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

const LAYOUTS: &[&Layout] = &[&US_QWERTY, &SWISS_GERMAN_MAC];

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

pub fn available_layouts() -> String {
    let names: Vec<&str> = LAYOUTS.iter().map(|l| l.name).collect();
    names.join(", ")
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
const OUML_L: &[u8] = "ö".as_bytes();
const AUML_L: &[u8] = "ä".as_bytes();
const EACU_L: &[u8] = "é".as_bytes();
const EGRV_L: &[u8] = "è".as_bytes();
const AGRV_L: &[u8] = "à".as_bytes();
const CCEDIL: &[u8] = "ç".as_bytes();
const SECT:   &[u8] = "§".as_bytes();
const DEGREE: &[u8] = "°".as_bytes();
const POUND:  &[u8] = "£".as_bytes();
const DIAER:  &[u8] = "¨".as_bytes();

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

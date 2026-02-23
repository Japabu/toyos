use alloc::string::String;
use alloc::vec::Vec;

use crate::sync::SyncCell;

static KEY_BUF: SyncCell<Vec<u8>> = SyncCell::new(Vec::new());

pub fn handle_key(byte: u8) {
    if byte != 0 {
        KEY_BUF.get_mut().push(byte);
    }
}

pub fn has_data() -> bool {
    !KEY_BUF.get().is_empty()
}

pub fn try_read_char() -> Option<u8> {
    let buf = KEY_BUF.get_mut();
    if buf.is_empty() { None } else { Some(buf.remove(0)) }
}

pub struct KeyEntry {
    pub normal: &'static [u8],
    pub shift: &'static [u8],
}

/// HID usage codes 0x04..=0x38 mapped to characters (index = usage - 0x04).
pub struct Layout {
    pub name: &'static str,
    pub keys: [KeyEntry; 53],
}

pub fn layout_lookup(usage: u8, shift: bool) -> Option<&'static [u8]> {
    if !(0x04..=0x38).contains(&usage) {
        return None;
    }
    let entry = &active_layout().keys[(usage - 0x04) as usize];
    let bytes = if shift { entry.shift } else { entry.normal };
    if bytes.is_empty() { None } else { Some(bytes) }
}

static ACTIVE_LAYOUT: SyncCell<usize> = SyncCell::new(0);

const LAYOUTS: &[&Layout] = &[&US_QWERTY, &SWISS_GERMAN_MAC];

fn active_layout() -> &'static Layout {
    LAYOUTS[*ACTIVE_LAYOUT.get()]
}

pub fn set_layout(name: &str) -> bool {
    for (i, layout) in LAYOUTS.iter().enumerate() {
        if layout.name == name {
            *ACTIVE_LAYOUT.get_mut() = i;
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

const K: KeyEntry = KeyEntry { normal: &[], shift: &[] };

const US_QWERTY: Layout = Layout {
    name: "us",
    keys: [
        KeyEntry { normal: b"a", shift: b"A" },
        KeyEntry { normal: b"b", shift: b"B" },
        KeyEntry { normal: b"c", shift: b"C" },
        KeyEntry { normal: b"d", shift: b"D" },
        KeyEntry { normal: b"e", shift: b"E" },
        KeyEntry { normal: b"f", shift: b"F" },
        KeyEntry { normal: b"g", shift: b"G" },
        KeyEntry { normal: b"h", shift: b"H" },
        KeyEntry { normal: b"i", shift: b"I" },
        KeyEntry { normal: b"j", shift: b"J" },
        KeyEntry { normal: b"k", shift: b"K" },
        KeyEntry { normal: b"l", shift: b"L" },
        KeyEntry { normal: b"m", shift: b"M" },
        KeyEntry { normal: b"n", shift: b"N" },
        KeyEntry { normal: b"o", shift: b"O" },
        KeyEntry { normal: b"p", shift: b"P" },
        KeyEntry { normal: b"q", shift: b"Q" },
        KeyEntry { normal: b"r", shift: b"R" },
        KeyEntry { normal: b"s", shift: b"S" },
        KeyEntry { normal: b"t", shift: b"T" },
        KeyEntry { normal: b"u", shift: b"U" },
        KeyEntry { normal: b"v", shift: b"V" },
        KeyEntry { normal: b"w", shift: b"W" },
        KeyEntry { normal: b"x", shift: b"X" },
        KeyEntry { normal: b"y", shift: b"Y" },
        KeyEntry { normal: b"z", shift: b"Z" },
        KeyEntry { normal: b"1", shift: b"!" },
        KeyEntry { normal: b"2", shift: b"@" },
        KeyEntry { normal: b"3", shift: b"#" },
        KeyEntry { normal: b"4", shift: b"$" },
        KeyEntry { normal: b"5", shift: b"%" },
        KeyEntry { normal: b"6", shift: b"^" },
        KeyEntry { normal: b"7", shift: b"&" },
        KeyEntry { normal: b"8", shift: b"*" },
        KeyEntry { normal: b"9", shift: b"(" },
        KeyEntry { normal: b"0", shift: b")" },
        KeyEntry { normal: b"\r", shift: b"\r" },
        KeyEntry { normal: &[0x1B], shift: &[0x1B] },
        KeyEntry { normal: &[0x08], shift: &[0x08] },
        KeyEntry { normal: b"\t", shift: b"\t" },
        KeyEntry { normal: b" ", shift: b" " },
        KeyEntry { normal: b"-", shift: b"_" },
        KeyEntry { normal: b"=", shift: b"+" },
        KeyEntry { normal: b"[", shift: b"{" },
        KeyEntry { normal: b"]", shift: b"}" },
        KeyEntry { normal: b"\\", shift: b"|" },
        K,
        KeyEntry { normal: b";", shift: b":" },
        KeyEntry { normal: b"'", shift: b"\"" },
        KeyEntry { normal: b"`", shift: b"~" },
        KeyEntry { normal: b",", shift: b"<" },
        KeyEntry { normal: b".", shift: b">" },
        KeyEntry { normal: b"/", shift: b"?" },
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
    keys: [
        KeyEntry { normal: b"a", shift: b"A" },
        KeyEntry { normal: b"b", shift: b"B" },
        KeyEntry { normal: b"c", shift: b"C" },
        KeyEntry { normal: b"d", shift: b"D" },
        KeyEntry { normal: b"e", shift: b"E" },
        KeyEntry { normal: b"f", shift: b"F" },
        KeyEntry { normal: b"g", shift: b"G" },
        KeyEntry { normal: b"h", shift: b"H" },
        KeyEntry { normal: b"i", shift: b"I" },
        KeyEntry { normal: b"j", shift: b"J" },
        KeyEntry { normal: b"k", shift: b"K" },
        KeyEntry { normal: b"l", shift: b"L" },
        KeyEntry { normal: b"m", shift: b"M" },
        KeyEntry { normal: b"n", shift: b"N" },
        KeyEntry { normal: b"o", shift: b"O" },
        KeyEntry { normal: b"p", shift: b"P" },
        KeyEntry { normal: b"q", shift: b"Q" },
        KeyEntry { normal: b"r", shift: b"R" },
        KeyEntry { normal: b"s", shift: b"S" },
        KeyEntry { normal: b"t", shift: b"T" },
        KeyEntry { normal: b"u", shift: b"U" },
        KeyEntry { normal: b"v", shift: b"V" },
        KeyEntry { normal: b"w", shift: b"W" },
        KeyEntry { normal: b"x", shift: b"X" },
        KeyEntry { normal: b"z", shift: b"Z" }, // QWERTZ: Y position -> z
        KeyEntry { normal: b"y", shift: b"Y" }, // QWERTZ: Z position -> y
        KeyEntry { normal: b"1", shift: b"+" },
        KeyEntry { normal: b"2", shift: b"\"" },
        KeyEntry { normal: b"3", shift: b"*" },
        KeyEntry { normal: b"4", shift: CCEDIL },
        KeyEntry { normal: b"5", shift: b"%" },
        KeyEntry { normal: b"6", shift: b"&" },
        KeyEntry { normal: b"7", shift: b"/" },
        KeyEntry { normal: b"8", shift: b"(" },
        KeyEntry { normal: b"9", shift: b")" },
        KeyEntry { normal: b"0", shift: b"=" },
        KeyEntry { normal: b"\r", shift: b"\r" },
        KeyEntry { normal: &[0x1B], shift: &[0x1B] },
        KeyEntry { normal: &[0x08], shift: &[0x08] },
        KeyEntry { normal: b"\t", shift: b"\t" },
        KeyEntry { normal: b" ", shift: b" " },
        KeyEntry { normal: b"'", shift: b"?" },
        KeyEntry { normal: b"^", shift: b"`" },
        KeyEntry { normal: UUML_L, shift: EGRV_L },
        KeyEntry { normal: DIAER, shift: b"!" },
        KeyEntry { normal: b"$", shift: POUND },
        KeyEntry { normal: b"<", shift: b">" },
        KeyEntry { normal: OUML_L, shift: EACU_L },
        KeyEntry { normal: AUML_L, shift: AGRV_L },
        KeyEntry { normal: SECT, shift: DEGREE },
        KeyEntry { normal: b",", shift: b";" },
        KeyEntry { normal: b".", shift: b":" },
        KeyEntry { normal: b"-", shift: b"_" },
    ],
};

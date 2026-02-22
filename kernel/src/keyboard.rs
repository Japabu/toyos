use alloc::string::String;

use crate::sync::SyncCell;

// Ring buffer for translated key characters
const BUF_SIZE: usize = 256;

struct KeyBuffer {
    buf: [u8; BUF_SIZE],
    head: usize,
    tail: usize,
}

static KEY_BUF: SyncCell<KeyBuffer> = SyncCell::new(KeyBuffer {
    buf: [0; BUF_SIZE],
    head: 0,
    tail: 0,
});

/// Called from the USB HID driver with a pre-translated byte.
pub fn handle_key(byte: u8) {
    if byte == 0 {
        return;
    }
    let kb = KEY_BUF.get_mut();
    let next = (kb.head + 1) % BUF_SIZE;
    if next != kb.tail {
        kb.buf[kb.head] = byte;
        kb.head = next;
    }
}

/// Check if there is data in the keyboard buffer.
pub fn has_data() -> bool {
    let kb = KEY_BUF.get();
    kb.tail != kb.head
}

/// Non-blocking read of the next character from the keyboard buffer.
pub fn try_read_char() -> Option<u8> {
    let kb = KEY_BUF.get_mut();
    if kb.tail == kb.head {
        return None;
    }
    let ch = kb.buf[kb.tail];
    kb.tail = (kb.tail + 1) % BUF_SIZE;
    Some(ch)
}

// ---------------------------------------------------------------------------
// Keyboard layout support
// ---------------------------------------------------------------------------

/// A single key mapping: (normal_bytes, shifted_bytes).
/// Empty slice means no character output.
pub struct KeyEntry {
    pub normal: &'static [u8],
    pub shift: &'static [u8],
}

/// A keyboard layout: name + mapping table for HID usage codes 0x04..=0x38.
pub struct Layout {
    pub name: &'static str,
    /// 53 entries for HID codes 0x04 through 0x38 (index = usage - 0x04).
    pub keys: [KeyEntry; 53],
}

/// Look up a HID usage code in the active layout.
/// Returns the UTF-8 byte sequence for the character, or None.
pub fn layout_lookup(usage: u8, shift: bool) -> Option<&'static [u8]> {
    if usage < 0x04 || usage > 0x38 {
        return None;
    }
    let layout = active_layout();
    let entry = &layout.keys[(usage - 0x04) as usize];
    let bytes = if shift { entry.shift } else { entry.normal };
    if bytes.is_empty() { None } else { Some(bytes) }
}

// Active layout global
static ACTIVE_LAYOUT: SyncCell<usize> = SyncCell::new(0); // index into LAYOUTS

const LAYOUTS: &[&Layout] = &[&US_QWERTY, &SWISS_GERMAN_MAC];

fn active_layout() -> &'static Layout {
    LAYOUTS[*ACTIVE_LAYOUT.get_mut()]
}

/// Set the active keyboard layout by name. Returns true on success.
pub fn set_layout(name: &str) -> bool {
    for (i, layout) in LAYOUTS.iter().enumerate() {
        if layout.name == name {
            *ACTIVE_LAYOUT.get_mut() = i;
            return true;
        }
    }
    false
}

/// Get the name of the active layout.
pub fn layout_name() -> &'static str {
    active_layout().name
}

/// Get a comma-separated string of available layout names.
pub fn available_layouts() -> String {
    let names: alloc::vec::Vec<&str> = LAYOUTS.iter().map(|l| l.name).collect();
    names.join(", ")
}

// ---------------------------------------------------------------------------
// US QWERTY layout
// ---------------------------------------------------------------------------

// Helper constants for readability
const K: KeyEntry = KeyEntry { normal: &[], shift: &[] }; // no output

const US_QWERTY: Layout = Layout {
    name: "us",
    keys: [
        // 0x04..=0x1D: Letters a-z
        KeyEntry { normal: b"a", shift: b"A" }, // 0x04
        KeyEntry { normal: b"b", shift: b"B" }, // 0x05
        KeyEntry { normal: b"c", shift: b"C" }, // 0x06
        KeyEntry { normal: b"d", shift: b"D" }, // 0x07
        KeyEntry { normal: b"e", shift: b"E" }, // 0x08
        KeyEntry { normal: b"f", shift: b"F" }, // 0x09
        KeyEntry { normal: b"g", shift: b"G" }, // 0x0A
        KeyEntry { normal: b"h", shift: b"H" }, // 0x0B
        KeyEntry { normal: b"i", shift: b"I" }, // 0x0C
        KeyEntry { normal: b"j", shift: b"J" }, // 0x0D
        KeyEntry { normal: b"k", shift: b"K" }, // 0x0E
        KeyEntry { normal: b"l", shift: b"L" }, // 0x0F
        KeyEntry { normal: b"m", shift: b"M" }, // 0x10
        KeyEntry { normal: b"n", shift: b"N" }, // 0x11
        KeyEntry { normal: b"o", shift: b"O" }, // 0x12
        KeyEntry { normal: b"p", shift: b"P" }, // 0x13
        KeyEntry { normal: b"q", shift: b"Q" }, // 0x14
        KeyEntry { normal: b"r", shift: b"R" }, // 0x15
        KeyEntry { normal: b"s", shift: b"S" }, // 0x16
        KeyEntry { normal: b"t", shift: b"T" }, // 0x17
        KeyEntry { normal: b"u", shift: b"U" }, // 0x18
        KeyEntry { normal: b"v", shift: b"V" }, // 0x19
        KeyEntry { normal: b"w", shift: b"W" }, // 0x1A
        KeyEntry { normal: b"x", shift: b"X" }, // 0x1B
        KeyEntry { normal: b"y", shift: b"Y" }, // 0x1C
        KeyEntry { normal: b"z", shift: b"Z" }, // 0x1D
        // 0x1E..=0x27: Numbers 1-9, 0
        KeyEntry { normal: b"1", shift: b"!" }, // 0x1E
        KeyEntry { normal: b"2", shift: b"@" }, // 0x1F
        KeyEntry { normal: b"3", shift: b"#" }, // 0x20
        KeyEntry { normal: b"4", shift: b"$" }, // 0x21
        KeyEntry { normal: b"5", shift: b"%" }, // 0x22
        KeyEntry { normal: b"6", shift: b"^" }, // 0x23
        KeyEntry { normal: b"7", shift: b"&" }, // 0x24
        KeyEntry { normal: b"8", shift: b"*" }, // 0x25
        KeyEntry { normal: b"9", shift: b"(" }, // 0x26
        KeyEntry { normal: b"0", shift: b")" }, // 0x27
        // 0x28..=0x2C: Special keys (Enter, Esc, Backspace, Tab, Space)
        // These are handled separately in xhci.rs, but included for completeness
        KeyEntry { normal: b"\r", shift: b"\r" },   // 0x28 Enter
        KeyEntry { normal: &[0x1B], shift: &[0x1B] }, // 0x29 Escape
        KeyEntry { normal: &[0x08], shift: &[0x08] }, // 0x2A Backspace
        KeyEntry { normal: b"\t", shift: b"\t" },   // 0x2B Tab
        KeyEntry { normal: b" ", shift: b" " },     // 0x2C Space
        // 0x2D..=0x38: Punctuation and symbols
        KeyEntry { normal: b"-", shift: b"_" }, // 0x2D
        KeyEntry { normal: b"=", shift: b"+" }, // 0x2E
        KeyEntry { normal: b"[", shift: b"{" }, // 0x2F
        KeyEntry { normal: b"]", shift: b"}" }, // 0x30
        KeyEntry { normal: b"\\", shift: b"|" }, // 0x31
        K,                                       // 0x32 Non-US # (ISO)
        KeyEntry { normal: b";", shift: b":" }, // 0x33
        KeyEntry { normal: b"'", shift: b"\"" }, // 0x34
        KeyEntry { normal: b"`", shift: b"~" }, // 0x35
        KeyEntry { normal: b",", shift: b"<" }, // 0x36
        KeyEntry { normal: b".", shift: b">" }, // 0x37
        KeyEntry { normal: b"/", shift: b"?" }, // 0x38
    ],
};

// ---------------------------------------------------------------------------
// Swiss German macOS layout
// ---------------------------------------------------------------------------

// UTF-8 byte sequences for non-ASCII characters
const UUML_L: &[u8] = "ü".as_bytes(); // U+00FC
const OUML_L: &[u8] = "ö".as_bytes(); // U+00F6
const AUML_L: &[u8] = "ä".as_bytes(); // U+00E4
const EACU_L: &[u8] = "é".as_bytes(); // U+00E9
const EGRV_L: &[u8] = "è".as_bytes(); // U+00E8
const AGRV_L: &[u8] = "à".as_bytes(); // U+00E0
const CCEDIL: &[u8] = "ç".as_bytes(); // U+00E7
const SECT:   &[u8] = "§".as_bytes(); // U+00A7
const DEGREE: &[u8] = "°".as_bytes(); // U+00B0
const POUND:  &[u8] = "£".as_bytes(); // U+00A3
const DIAER:  &[u8] = "¨".as_bytes(); // U+00A8

const SWISS_GERMAN_MAC: Layout = Layout {
    name: "swiss-german-mac",
    keys: [
        // 0x04..=0x1D: Letters (QWERTZ: Z and Y swapped)
        KeyEntry { normal: b"a", shift: b"A" }, // 0x04
        KeyEntry { normal: b"b", shift: b"B" }, // 0x05
        KeyEntry { normal: b"c", shift: b"C" }, // 0x06
        KeyEntry { normal: b"d", shift: b"D" }, // 0x07
        KeyEntry { normal: b"e", shift: b"E" }, // 0x08
        KeyEntry { normal: b"f", shift: b"F" }, // 0x09
        KeyEntry { normal: b"g", shift: b"G" }, // 0x0A
        KeyEntry { normal: b"h", shift: b"H" }, // 0x0B
        KeyEntry { normal: b"i", shift: b"I" }, // 0x0C
        KeyEntry { normal: b"j", shift: b"J" }, // 0x0D
        KeyEntry { normal: b"k", shift: b"K" }, // 0x0E
        KeyEntry { normal: b"l", shift: b"L" }, // 0x0F
        KeyEntry { normal: b"m", shift: b"M" }, // 0x10
        KeyEntry { normal: b"n", shift: b"N" }, // 0x11
        KeyEntry { normal: b"o", shift: b"O" }, // 0x12
        KeyEntry { normal: b"p", shift: b"P" }, // 0x13
        KeyEntry { normal: b"q", shift: b"Q" }, // 0x14
        KeyEntry { normal: b"r", shift: b"R" }, // 0x15
        KeyEntry { normal: b"s", shift: b"S" }, // 0x16
        KeyEntry { normal: b"t", shift: b"T" }, // 0x17
        KeyEntry { normal: b"u", shift: b"U" }, // 0x18
        KeyEntry { normal: b"v", shift: b"V" }, // 0x19
        KeyEntry { normal: b"w", shift: b"W" }, // 0x1A
        KeyEntry { normal: b"x", shift: b"X" }, // 0x1B
        KeyEntry { normal: b"z", shift: b"Z" }, // 0x1C (Y key position → z)
        KeyEntry { normal: b"y", shift: b"Y" }, // 0x1D (Z key position → y)
        // 0x1E..=0x27: Number row
        KeyEntry { normal: b"1", shift: b"+" },     // 0x1E
        KeyEntry { normal: b"2", shift: b"\"" },    // 0x1F
        KeyEntry { normal: b"3", shift: b"*" },     // 0x20
        KeyEntry { normal: b"4", shift: CCEDIL },   // 0x21 → ç
        KeyEntry { normal: b"5", shift: b"%" },     // 0x22
        KeyEntry { normal: b"6", shift: b"&" },     // 0x23
        KeyEntry { normal: b"7", shift: b"/" },     // 0x24
        KeyEntry { normal: b"8", shift: b"(" },     // 0x25
        KeyEntry { normal: b"9", shift: b")" },     // 0x26
        KeyEntry { normal: b"0", shift: b"=" },     // 0x27
        // 0x28..=0x2C: Special keys (layout-independent)
        KeyEntry { normal: b"\r", shift: b"\r" },   // 0x28 Enter
        KeyEntry { normal: &[0x1B], shift: &[0x1B] }, // 0x29 Escape
        KeyEntry { normal: &[0x08], shift: &[0x08] }, // 0x2A Backspace
        KeyEntry { normal: b"\t", shift: b"\t" },   // 0x2B Tab
        KeyEntry { normal: b" ", shift: b" " },     // 0x2C Space
        // 0x2D..=0x38: Punctuation and symbols
        KeyEntry { normal: b"'", shift: b"?" },     // 0x2D
        KeyEntry { normal: b"^", shift: b"`" },     // 0x2E (dead keys, output char directly)
        KeyEntry { normal: UUML_L, shift: EGRV_L }, // 0x2F → ü / è
        KeyEntry { normal: DIAER, shift: b"!" },    // 0x30 → ¨ / !
        KeyEntry { normal: b"$", shift: POUND },    // 0x31 → $ / £
        KeyEntry { normal: b"<", shift: b">" },     // 0x32 Non-US key → < / >
        KeyEntry { normal: OUML_L, shift: EACU_L }, // 0x33 → ö / é
        KeyEntry { normal: AUML_L, shift: AGRV_L }, // 0x34 → ä / à
        KeyEntry { normal: SECT, shift: DEGREE },   // 0x35 → § / °
        KeyEntry { normal: b",", shift: b";" },     // 0x36
        KeyEntry { normal: b".", shift: b":" },     // 0x37
        KeyEntry { normal: b"-", shift: b"_" },     // 0x38
    ],
};

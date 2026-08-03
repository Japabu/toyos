//! The whole of one key press, from HID usage to the bytes a surface delivers.
//!
//! `compose.rs` drives the layout tables and the dead-key machine directly;
//! this drives them through the type a surface owner actually holds, which is
//! also the only place the control codes and the escape sequences exist.

use toyos_keymap::{Mods, Translator};

const NONE: Mods = Mods { shift: false, ctrl: false, alt: false };
const SHIFT: Mods = Mods { shift: true, ctrl: false, alt: false };
const CTRL: Mods = Mods { shift: false, ctrl: true, alt: false };
const ALT: Mods = Mods { shift: false, ctrl: false, alt: true };

// HID usages this file names by hand.
const A: u8 = 0x04;
const C: u8 = 0x06;
const E: u8 = 0x08;
const Q: u8 = 0x14;
const U: u8 = 0x18;
const Y: u8 = 0x1C;
const Z: u8 = 0x1D;
const TWO: u8 = 0x1F;
const SPACE: u8 = 0x2C;
const MINUS: u8 = 0x2D;
const EQUAL: u8 = 0x2E;
const BRACKET_LEFT: u8 = 0x2F;
const BRACKET_RIGHT: u8 = 0x30;
const SEMICOLON: u8 = 0x33;
const APOSTROPHE: u8 = 0x34;
const GRAVE: u8 = 0x35;
const LEFT_SHIFT: u8 = 0xE1;
const UP_ARROW: u8 = 0x52;
const LEFT_ARROW: u8 = 0x50;
const PAGE_UP: u8 = 0x4B;
const F1: u8 = 0x3A;
const ISO: u8 = 0x64;

fn typed(layout: &str, keys: &[(u8, Mods)]) -> String {
    let mut t = Translator::new();
    assert!(t.set_layout(layout), "no layout {layout}");
    let mut out = String::new();
    for &(usage, mods) in keys {
        out.push_str(t.press(usage, mods).as_str());
    }
    out
}

#[test]
fn the_default_layout_is_us() {
    assert_eq!(Translator::new().layout(), "us");
}

#[test]
fn an_unknown_name_leaves_the_layout_alone() {
    let mut t = Translator::new();
    assert!(t.set_layout("swiss-german"));
    assert!(!t.set_layout("klingon"));
    assert_eq!(t.layout(), "swiss-german");
}

#[test]
fn arrows_and_page_keys_send_escape_sequences() {
    assert_eq!(typed("us", &[(UP_ARROW, NONE)]), "\x1b[A");
    assert_eq!(typed("us", &[(LEFT_ARROW, NONE)]), "\x1b[D");
    assert_eq!(typed("us", &[(PAGE_UP, NONE)]), "\x1b[5~");
}

/// The escape sequences are layout-independent, which is the reason they live
/// here and not in a table: `swiss-german` moves letters, not arrows.
#[test]
fn an_escape_sequence_does_not_depend_on_the_layout() {
    for layout in toyos_keymap::LAYOUTS {
        assert_eq!(typed(layout.name, &[(LEFT_ARROW, NONE)]), "\x1b[D", "{}", layout.name);
    }
}

#[test]
fn ctrl_makes_control_codes_of_the_letter_row() {
    assert_eq!(typed("us", &[(C, CTRL)]), "\u{3}");
    assert_eq!(typed("us", &[(A, CTRL)]), "\u{1}");
    assert_eq!(typed("us", &[(Z, CTRL)]), "\u{1a}");
}

/// Ctrl selects by *position*, so Ctrl+C is the same physical key on a QWERTZ
/// board — where that key prints `C` too — and Ctrl+Y and Ctrl+Z do not swap
/// with the letters above them.
#[test]
fn a_control_code_is_the_key_position_not_the_letter() {
    assert_eq!(typed("swiss-german", &[(Y, CTRL)]), "\u{19}");
    assert_eq!(typed("swiss-german", &[(Y, NONE)]), "z");
}

/// A key the layout does not define leaves a pending diacritic pending — the
/// property that makes `^`, Shift, `e` produce `Ê` — and F1 is one that is
/// neither a modifier nor an escape sequence this table has.
#[test]
fn an_undefined_key_does_not_consume_a_pending_diacritic() {
    assert_eq!(
        typed("swiss-german", &[(EQUAL, NONE), (F1, NONE), (LEFT_SHIFT, NONE), (E, SHIFT)]),
        "Ê"
    );
}

/// The pending diacritic is per-instance. Two surfaces typed at in turn must
/// not compose each other's keys — the defect a machine-wide composer had by
/// construction.
#[test]
fn two_translators_do_not_share_a_pending_diacritic() {
    let mut one = Translator::new();
    let mut two = Translator::new();
    assert!(one.set_layout("swiss-german"));
    assert!(two.set_layout("swiss-german"));

    assert_eq!(one.press(EQUAL, NONE).as_str(), "", "the dead key is pending on `one`");
    // `two` never saw the `^`, so its `e` is a bare `e`.
    assert_eq!(two.press(E, NONE).as_str(), "e");
    // And `one`'s is still waiting for a base character.
    assert_eq!(one.press(E, NONE).as_str(), "ê");
}

#[test]
fn changing_layout_drops_a_pending_diacritic() {
    let mut t = Translator::new();
    assert!(t.set_layout("swiss-german"));
    assert_eq!(t.press(EQUAL, NONE).as_str(), "");
    assert!(t.set_layout("us"));
    assert_eq!(t.press(E, NONE).as_str(), "e");
}

/// The Swiss German gate's own key sequence, by position, through the type the
/// guest gate now holds.
///
/// The same presses `swiss_german_layout` injects through QMP and the same
/// string it asserts on. Here it costs a millisecond and proves the tables and
/// the machine; there it costs a boot and proves that the i8042, the kernel's
/// merge, the surface channel and the config all carry it.
#[test]
fn the_swiss_german_gates_own_sequence() {
    let out = typed(
        "swiss-german",
        &[
            (Y, NONE),
            (Z, NONE),
            (BRACKET_LEFT, NONE),
            (SEMICOLON, NONE),
            (APOSTROPHE, NONE),
            (APOSTROPHE, SHIFT),
            (TWO, ALT),
            (E, ALT),
            (BRACKET_LEFT, ALT),
            (ISO, NONE),
            (ISO, SHIFT),
            (ISO, ALT),
            (EQUAL, NONE),
            (E, NONE),
            (EQUAL, NONE),
            (E, SHIFT),
            (BRACKET_RIGHT, NONE),
            (U, SHIFT),
            (EQUAL, NONE),
            (SPACE, NONE),
            (MINUS, ALT),
            (E, NONE),
            (EQUAL, NONE),
            (Q, NONE),
            (GRAVE, NONE),
        ],
    );
    assert_eq!(out, "zyüöäà@€[<>\\êÊÜ^é^q§");
}

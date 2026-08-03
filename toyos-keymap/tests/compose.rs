//! The dead-key machine, driven the way the kernel drives it.
//!
//! Every case here is a sequence of HID usages and modifier states resolved
//! through a real layout, so a table entry and the machine over it are gated
//! together: `^` composing with `e` is only interesting if `AE12` really is a
//! dead key on `swiss-german`.

use toyos_keymap::{by_name, Composer, Dead, Emit, Key, Layout, LAYOUTS, MAX_EMIT};

const SHIFT: u8 = 1;
const ALT: u8 = 2;

fn layout(name: &str) -> &'static Layout {
    LAYOUTS[by_name(name).unwrap_or_else(|| panic!("no layout {name}"))]
}

/// Type `(usage, modifiers)` in order and return everything that came out.
fn type_keys(name: &str, keys: &[(u8, u8)]) -> String {
    let layout = layout(name);
    let mut composer = Composer::new();
    let mut out = Vec::new();
    for &(usage, mods) in keys {
        let key = layout.lookup(usage, mods & SHIFT != 0, mods & ALT != 0);
        out.extend_from_slice(composer.press(key).as_bytes());
    }
    String::from_utf8(out).expect("layouts emit UTF-8")
}

fn one(name: &str, usage: u8, mods: u8) -> String {
    type_keys(name, &[(usage, mods)])
}

// HID usages this file names by hand.
const AE11: u8 = 0x2D; // ' ? ´
const AE12: u8 = 0x2E; // ^ ` ~
const AD11: u8 = 0x2F; // ü è [
const AD12: u8 = 0x30; // ¨ ! ]
const AC11: u8 = 0x34; // ä à {
const BKSL: u8 = 0x31; // $ £ }
const TLDE: u8 = 0x35; // § °
const LSGT: u8 = 0x64; // < > \ ¦
const SPACE: u8 = 0x2C;
const KEY_A: u8 = 0x04;
const KEY_E: u8 = 0x08;
const KEY_N: u8 = 0x11;
const KEY_Q: u8 = 0x14;
const KEY_U: u8 = 0x18;
const KEY_2: u8 = 0x1F;
const KEY_7: u8 = 0x24;
const POS_Y: u8 = 0x1C; // the key a US board calls y
const POS_Z: u8 = 0x1D; // the key a US board calls z
const LSHIFT: u8 = 0xE1;

#[test]
fn qwertz() {
    assert_eq!(one("swiss-german", POS_Y, 0), "z");
    assert_eq!(one("swiss-german", POS_Z, 0), "y");
    assert_eq!(one("swiss-german", POS_Y, SHIFT), "Z");
    assert_eq!(one("swiss-german", POS_Z, SHIFT), "Y");
    // The layout it is not: same physical keys, QWERTY.
    assert_eq!(one("us", POS_Y, 0), "y");
    assert_eq!(one("us", POS_Z, 0), "z");
}

#[test]
fn dedicated_umlauts() {
    assert_eq!(one("swiss-german", AD11, 0), "ü");
    assert_eq!(one("swiss-german", 0x33, 0), "ö");
    assert_eq!(one("swiss-german", AC11, 0), "ä");
    // Shift on those keys is the accented vowels, not the capitals — which is
    // the whole reason this layout carries a diaeresis dead key.
    assert_eq!(one("swiss-german", AD11, SHIFT), "è");
    assert_eq!(one("swiss-german", 0x33, SHIFT), "é");
    assert_eq!(one("swiss-german", AC11, SHIFT), "à");
}

#[test]
fn iso_key() {
    assert_eq!(one("swiss-german", LSGT, 0), "<");
    assert_eq!(one("swiss-german", LSGT, SHIFT), ">");
    assert_eq!(one("swiss-german", LSGT, ALT), "\\");
    assert_eq!(one("swiss-german", LSGT, SHIFT | ALT), "¦");
}

#[test]
fn altgr_layer() {
    for (usage, mods, want) in [
        (KEY_2, ALT, "@"),
        (KEY_E, ALT, "€"),
        (KEY_7, ALT, "|"),
        (AD11, ALT, "["),
        (AD12, ALT, "]"),
        (AC11, ALT, "{"),
        (BKSL, ALT, "}"),
        (0x20, ALT, "#"),
        (0x23, ALT, "¬"),
        (KEY_A, ALT, "æ"),
        (KEY_A, SHIFT | ALT, "Æ"),
    ] {
        assert_eq!(one("swiss-german", usage, mods), want, "usage {usage:#04x} mods {mods}");
    }
    assert_eq!(one("swiss-german", TLDE, 0), "§");
    assert_eq!(one("swiss-german", TLDE, SHIFT), "°");
}

/// The five positions the reference makes dead, and the fact that pressing one
/// delivers nothing at all.
#[test]
fn dead_keys_emit_nothing_when_pressed() {
    let layout = layout("swiss-german");
    for (usage, mods, dead) in [
        (AE12, 0, Dead::Circumflex),
        (AE12, SHIFT, Dead::Grave),
        (AE12, ALT, Dead::Tilde),
        (AE11, ALT, Dead::Acute),
        (AD12, 0, Dead::Diaeresis),
    ] {
        let key = layout.lookup(usage, mods & SHIFT != 0, mods & ALT != 0);
        assert_eq!(key, Key::Dead(dead), "usage {usage:#04x} mods {mods}");

        let mut composer = Composer::new();
        assert_eq!(composer.press(key), Emit::EMPTY, "{dead:?} emitted on its own press");
        assert_eq!(composer.pending(), Some(dead));
    }
}

#[test]
fn composition() {
    for (seq, want) in [
        (&[(AE12, 0), (KEY_E, 0)][..], "ê"),
        (&[(AE12, 0), (KEY_A, 0)][..], "â"),
        (&[(AE12, 0), (KEY_E, SHIFT)][..], "Ê"),
        (&[(AE12, SHIFT), (KEY_A, 0)][..], "à"),
        (&[(AE12, SHIFT), (KEY_U, 0)][..], "ù"),
        (&[(AE12, ALT), (KEY_N, 0)][..], "ñ"),
        (&[(AE12, ALT), (KEY_A, SHIFT)][..], "Ã"),
        (&[(AE11, ALT), (KEY_E, 0)][..], "é"),
        (&[(AE11, ALT), (KEY_E, SHIFT)][..], "É"),
        (&[(AD12, 0), (KEY_U, 0)][..], "ü"),
        (&[(AD12, 0), (KEY_A, 0)][..], "ä"),
        // The capital umlauts the layout has no dedicated key for.
        (&[(AD12, 0), (KEY_U, SHIFT)][..], "Ü"),
        (&[(AD12, 0), (KEY_A, SHIFT)][..], "Ä"),
        (&[(AD12, 0), (0x12, SHIFT)][..], "Ö"),
        // Composition does not stop the next key being ordinary.
        (&[(AE12, 0), (KEY_E, 0), (KEY_E, 0)][..], "êe"),
    ] {
        assert_eq!(type_keys("swiss-german", seq), want, "{seq:?}");
    }
}

/// Shift is pressed *after* the dead key to make a capital, so a key the
/// layout defines nothing for must not disturb what is pending.
#[test]
fn modifiers_do_not_break_composition() {
    assert_eq!(
        type_keys("swiss-german", &[(AE12, 0), (LSHIFT, 0), (KEY_E, SHIFT)]),
        "Ê"
    );
}

/// The reference gives a dead key before a space the ASCII character and a
/// doubled dead key the spacing diacritic, and for `´` and `¨` those differ.
#[test]
fn dead_then_space_and_doubled() {
    for (dead, mods, ascii, spacing) in [
        (AE12, 0, "^", "^"),
        (AE12, SHIFT, "`", "`"),
        (AE12, ALT, "~", "~"),
        (AE11, ALT, "'", "´"),
        (AD12, 0, "\"", "¨"),
    ] {
        assert_eq!(type_keys("swiss-german", &[(dead, mods), (SPACE, 0)]), ascii);
        assert_eq!(type_keys("swiss-german", &[(dead, mods), (dead, mods)]), spacing);
    }
}

/// Two different dead keys in a row: the first gives up its spacing form and
/// the second takes its place.
#[test]
fn dead_then_other_dead() {
    assert_eq!(type_keys("swiss-german", &[(AE12, 0), (AD12, 0), (KEY_U, 0)]), "^ü");
    assert_eq!(type_keys("swiss-german", &[(AD12, 0), (AE12, 0), (KEY_E, 0)]), "¨ê");
}

/// Nothing the reference composes with: the diacritic, then the character.
/// Neither key press is lost.
#[test]
fn fallthrough() {
    assert_eq!(type_keys("swiss-german", &[(AE12, 0), (KEY_Q, 0)]), "^q");
    assert_eq!(type_keys("swiss-german", &[(AD12, 0), (0x07, 0)]), "¨d");
    assert_eq!(type_keys("swiss-german", &[(AE11, ALT), (BKSL, 0)]), "´$");
    // Including the pair the bound in `MAX_EMIT` is set by.
    assert_eq!(type_keys("swiss-german", &[(AD12, 0), (KEY_E, ALT)]), "¨€");
}

#[test]
fn worst_case_fallthrough_fits_max_emit() {
    let layout = layout("swiss-german");
    let mut worst = 0;
    for dead in [AE12, AE11, AD12] {
        for mods in [0, SHIFT, ALT] {
            let d = layout.lookup(dead, mods & SHIFT != 0, mods & ALT != 0);
            if !matches!(d, Key::Dead(_)) {
                continue;
            }
            for usage in 0x04..=0x38u8 {
                for next in [0, SHIFT, ALT, SHIFT | ALT] {
                    let mut composer = Composer::new();
                    composer.press(d);
                    let out =
                        composer.press(layout.lookup(usage, next & SHIFT != 0, next & ALT != 0));
                    worst = worst.max(out.as_bytes().len());
                }
            }
        }
    }
    assert_eq!(worst, MAX_EMIT, "the bound should be tight, not merely respected");
}

/// A layout change drops what the old layout left pending.
#[test]
fn reset_drops_pending() {
    let sg = layout("swiss-german");
    let mut composer = Composer::new();
    composer.press(sg.lookup(AE12, false, false));
    assert!(composer.pending().is_some());
    composer.reset();
    assert_eq!(composer.press(layout("us").lookup(KEY_E, false, false)).as_bytes(), b"e");
}

/// The three layouts that were here before have no dead key anywhere, so
/// nothing about them can have changed shape.
#[test]
fn only_swiss_german_has_dead_keys() {
    for layout in LAYOUTS {
        let dead = (0x04..=0x38u8)
            .chain(core::iter::once(0x64))
            .flat_map(|u| (0..4).map(move |i| (u, i)))
            .filter(|&(u, i)| matches!(layout.entry(u).unwrap().level(i), Key::Dead(_)))
            .count();
        let want = if layout.name == "swiss-german" { 5 } else { 0 };
        assert_eq!(dead, want, "{} has {dead} dead keys", layout.name);
    }
}

#[test]
fn us_is_the_default_and_still_types_ascii() {
    assert_eq!(LAYOUTS[toyos_keymap::DEFAULT_LAYOUT].name, "us");
    assert_eq!(type_keys("us", &[(0x0B, 0), (KEY_E, 0), (0x0F, 0), (0x0F, 0), (0x12, 0)]), "hello");
    assert_eq!(one("us", KEY_2, SHIFT), "@");
    assert_eq!(one("us", 0x35, 0), "`");
}

#[test]
fn german_is_unchanged_by_the_dead_key_work() {
    // `de` types the diacritics as characters and always has; it is not a
    // de_CH in disguise.
    assert_eq!(one("de", AE12, 0), "´");
    assert_eq!(one("de", AE12, SHIFT), "`");
    assert_eq!(type_keys("de", &[(AE12, 0), (KEY_E, 0)]), "´e");
    assert_eq!(one("de", AD11, SHIFT), "Ü");
    assert_eq!(one("de", KEY_Q, ALT), "@");
}

#[test]
fn swiss_german_mac_is_untouched() {
    assert_eq!(one("swiss-german-mac", AE12, 0), "^");
    assert_eq!(type_keys("swiss-german-mac", &[(AE12, 0), (KEY_E, 0)]), "^e");
    assert_eq!(one("swiss-german-mac", 0x0A, ALT), "@");
    assert_eq!(one("swiss-german-mac", 0x35, 0), "<");
    assert_eq!(one("swiss-german-mac", LSGT, 0), "§");
}

#[test]
fn unknown_layout_is_named_as_unknown() {
    assert_eq!(by_name("swiss_german"), None);
    assert_eq!(by_name("de_CH"), None);
    assert!(by_name("swiss-german").is_some());
}

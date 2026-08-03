//! The layout tables.
//!
//! Indexed by HID usage minus [`FIRST_USAGE`], so the comment on each row is
//! the xkb key name that row is written against — `AD01` is the first key of
//! the top letter row wherever the reference puts it, which is what makes a
//! QWERTZ table checkable against a QWERTY-indexed reference.

use crate::{Dead, Key, KeyEntry, Layout};

const fn key(normal: &'static str, shift: &'static str) -> KeyEntry {
    KeyEntry {
        normal: Key::Chars(normal),
        shift: Key::Chars(shift),
        option: Key::None,
        shift_option: Key::None,
    }
}

const fn key_opt(normal: &'static str, shift: &'static str, option: &'static str) -> KeyEntry {
    KeyEntry {
        normal: Key::Chars(normal),
        shift: Key::Chars(shift),
        option: Key::Chars(option),
        shift_option: Key::None,
    }
}

const fn key_full(
    normal: &'static str,
    shift: &'static str,
    option: &'static str,
    shift_option: &'static str,
) -> KeyEntry {
    KeyEntry {
        normal: Key::Chars(normal),
        shift: Key::Chars(shift),
        option: Key::Chars(option),
        shift_option: Key::Chars(shift_option),
    }
}

/// A key the layout does not define at any level.
const K: KeyEntry =
    KeyEntry { normal: Key::None, shift: Key::None, option: Key::None, shift_option: Key::None };

/// US remains the layout every machine boots with.
pub const DEFAULT_LAYOUT: usize = 0;

pub static LAYOUTS: &[&Layout] = &[&US_QWERTY, &GERMAN, &SWISS_GERMAN, &SWISS_GERMAN_MAC];

const US_QWERTY: Layout = Layout {
    name: "us",
    iso_key: K,
    keys: [
        key("a", "A"),
        key("b", "B"),
        key("c", "C"),
        key("d", "D"),
        key("e", "E"),
        key("f", "F"),
        key("g", "G"),
        key("h", "H"),
        key("i", "I"),
        key("j", "J"),
        key("k", "K"),
        key("l", "L"),
        key("m", "M"),
        key("n", "N"),
        key("o", "O"),
        key("p", "P"),
        key("q", "Q"),
        key("r", "R"),
        key("s", "S"),
        key("t", "T"),
        key("u", "U"),
        key("v", "V"),
        key("w", "W"),
        key("x", "X"),
        key("y", "Y"),
        key("z", "Z"),
        key("1", "!"),
        key("2", "@"),
        key("3", "#"),
        key("4", "$"),
        key("5", "%"),
        key("6", "^"),
        key("7", "&"),
        key("8", "*"),
        key("9", "("),
        key("0", ")"),
        key("\r", "\r"),
        key("\x1b", "\x1b"),
        key("\x08", "\x08"),
        key("\t", "\t"),
        key(" ", " "),
        key("-", "_"),
        key("=", "+"),
        key("[", "{"),
        key("]", "}"),
        key("\\", "|"),
        K,
        key(";", ":"),
        key("'", "\""),
        key("`", "~"),
        key(",", "<"),
        key(".", ">"),
        key("/", "?"),
    ],
};

const GERMAN: Layout = Layout {
    name: "de",
    iso_key: key_opt("<", ">", "|"),
    keys: [
        key("a", "A"),
        key("b", "B"),
        key("c", "C"),
        key("d", "D"),
        key_opt("e", "E", "€"),
        key("f", "F"),
        key("g", "G"),
        key("h", "H"),
        key("i", "I"),
        key("j", "J"),
        key("k", "K"),
        key("l", "L"),
        key_opt("m", "M", "µ"),
        key("n", "N"),
        key("o", "O"),
        key("p", "P"),
        key_opt("q", "Q", "@"),
        key("r", "R"),
        key("s", "S"),
        key("t", "T"),
        key("u", "U"),
        key("v", "V"),
        key("w", "W"),
        key("x", "X"),
        key("z", "Z"),
        key("y", "Y"),
        key("1", "!"),
        key("2", "\""),
        key_opt("3", "§", "#"),
        key("4", "$"),
        key("5", "%"),
        key("6", "&"),
        key_opt("7", "/", "{"),
        key_opt("8", "(", "["),
        key_opt("9", ")", "]"),
        key_opt("0", "=", "}"),
        key("\r", "\r"),
        key("\x1b", "\x1b"),
        key("\x08", "\x08"),
        key("\t", "\t"),
        key(" ", " "),
        key_opt("ß", "?", "\\"),
        key("´", "`"),
        key("ü", "Ü"),
        key_opt("+", "*", "~"),
        key("#", "'"),
        K,
        key("ö", "Ö"),
        key("ä", "Ä"),
        key("^", "°"),
        key(",", ";"),
        key(".", ":"),
        key("-", "_"),
    ],
};

/// `ch(de)` from xkeyboard-config's `symbols/ch`, which is `ch(basic)`: the
/// keys it names, over the `latin` base it includes for the rest.
///
/// An xkb key statement in an including file replaces the included one whole,
/// so a row `ch(basic)` gives three levels for has nothing at the fourth even
/// where `latin` had something — `AD11` is `ü è [` and not `ü è [ ˚`.
///
/// Two divergences from the reference, both deliberate:
///
/// - `AC07` (`j`) keeps `latin`'s `dead_hook` and `dead_horn` at its AltGr
///   levels. Neither is here: no key on this layout composes with either, and
///   a dead key whose every sequence falls through is a key that eats the next
///   character for nothing.
/// - `AE01` is `|` at AltGr where a Swiss keycap prints `¦`. That is the
///   reference as it stands — `ch(legacy)` exists precisely because the two
///   have been swapped over the years — and following the keycap here would
///   mean a table that no longer says where it came from.
const SWISS_GERMAN: Layout = Layout {
    name: "swiss-german",
    iso_key: key_full("<", ">", "\\", "¦"), // LSGT
    keys: [
        key_full("a", "A", "æ", "Æ"),       // AC01
        key_full("b", "B", "“", "‘"),       // AB05
        key_full("c", "C", "¢", "©"),       // AB03
        key_full("d", "D", "ð", "Ð"),       // AC03
        key_opt("e", "E", "€"),             // AD03
        key_full("f", "F", "đ", "ª"),       // AC04
        key_full("g", "G", "ŋ", "Ŋ"),       // AC05
        key_full("h", "H", "ħ", "Ħ"),       // AC06
        key_full("i", "I", "→", "ı"),       // AD08
        key("j", "J"),                      // AC07
        key_full("k", "K", "ĸ", "&"),       // AC08
        key_full("l", "L", "ł", "Ł"),       // AC09
        key_full("m", "M", "µ", "º"),       // AB07
        key_full("n", "N", "”", "’"),       // AB06
        key_full("o", "O", "œ", "Œ"),       // AD09
        key_full("p", "P", "þ", "Þ"),       // AD10
        key_full("q", "Q", "@", "Ω"),       // AD01
        key_full("r", "R", "¶", "®"),       // AD04
        key_full("s", "S", "ß", "ẞ"),       // AC02
        key_full("t", "T", "ŧ", "Ŧ"),       // AD05
        key_full("u", "U", "↓", "↑"),       // AD07
        key_full("v", "V", "„", "‚"),       // AB04
        key_full("w", "W", "ſ", "§"),       // AD02
        key_full("x", "X", "»", ">"),       // AB02
        key("z", "Z"),                      // AD06
        key("y", "Y"),                      // AB01
        key_full("1", "+", "|", "¡"),       // AE01
        key_full("2", "\"", "@", "⅛"),      // AE02
        key_opt("3", "*", "#"),             // AE03
        key("4", "ç"),                      // AE04
        key("5", "%"),                      // AE05
        key_opt("6", "&", "¬"),             // AE06
        key_opt("7", "/", "|"),             // AE07
        key_opt("8", "(", "¢"),             // AE08
        key("9", ")"),                      // AE09
        key("0", "="),                      // AE10
        key("\r", "\r"),
        key("\x1b", "\x1b"),
        key("\x08", "\x08"),
        key("\t", "\t"),
        key(" ", " "),
        KeyEntry {
            // AE11
            normal: Key::Chars("'"),
            shift: Key::Chars("?"),
            option: Dead::Acute.key(),
            shift_option: Key::None,
        },
        KeyEntry {
            // AE12
            normal: Dead::Circumflex.key(),
            shift: Dead::Grave.key(),
            option: Dead::Tilde.key(),
            shift_option: Key::None,
        },
        key_opt("ü", "è", "["), // AD11
        KeyEntry {
            // AD12
            normal: Dead::Diaeresis.key(),
            shift: Key::Chars("!"),
            option: Key::Chars("]"),
            shift_option: Key::None,
        },
        key_opt("$", "£", "}"), // BKSL
        K,                      // no key here on an ISO board; BKSL is 0x31
        key("ö", "é"),          // AC10
        key_opt("ä", "à", "{"), // AC11
        key("§", "°"),          // TLDE
        key(",", ";"),          // AB08
        key(".", ":"),          // AB09
        key("-", "_"),          // AB10
    ],
};

const SWISS_GERMAN_MAC: Layout = Layout {
    name: "swiss-german-mac",
    iso_key: key("§", "°"),
    keys: [
        key("a", "A"),
        key("b", "B"),
        key("c", "C"),
        key("d", "D"),
        key("e", "E"),
        key("f", "F"),
        key_opt("g", "G", "@"),
        key("h", "H"),
        key("i", "I"),
        key("j", "J"),
        key("k", "K"),
        key("l", "L"),
        key("m", "M"),
        key_opt("n", "N", "~"),
        key("o", "O"),
        key("p", "P"),
        key("q", "Q"),
        key("r", "R"),
        key("s", "S"),
        key("t", "T"),
        key("u", "U"),
        key("v", "V"),
        key("w", "W"),
        key("x", "X"),
        key("z", "Z"),
        key("y", "Y"),
        key("1", "+"),
        key("2", "\""),
        key_opt("3", "*", "#"),
        key("4", "ç"),
        key_opt("5", "%", "["),
        key_opt("6", "&", "]"),
        key_full("7", "/", "|", "\\"),
        key_opt("8", "(", "{"),
        key_opt("9", ")", "}"),
        key("0", "="),
        key("\r", "\r"),
        key("\x1b", "\x1b"),
        key("\x08", "\x08"),
        key("\t", "\t"),
        key(" ", " "),
        key("'", "?"),
        key("^", "`"),
        key("ü", "è"),
        key("¨", "!"),
        key("$", "£"),
        K,
        key("ö", "é"),
        key("ä", "à"),
        key("<", ">"),
        key(",", ";"),
        key(".", ":"),
        key("-", "_"),
    ],
};

//! Dead keys and what they compose with.
//!
//! The table is derived mechanically from X11's `en_US.UTF-8/Compose`, which
//! is the composition half of the reference the layouts are written against
//! (xkeyboard-config's `symbols/ch`; the symbols file names dead keys and says
//! nothing about what they produce). Every rule for these five dead keys is
//! included whose second key is a single ASCII character and whose result is
//! one codepoint.
//!
//! Two mechanical exclusions, both of which are the reference saying no
//! precomposed character exists:
//!
//! - results of two codepoints — a base plus a combining mark (`` ` `` `m`,
//!   `´` `j`);
//! - results that are themselves a combining mark (`¨` `'`).
//!
//! `Ǘ`/`Ǜ` from `´`/`` ` `` before `v` look like typos and are not: the
//! reference uses `v` as its stand-in for `ü`, which has no ASCII key.

use crate::Key;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dead {
    Circumflex,
    Grave,
    Acute,
    Diaeresis,
    Tilde,
}

pub(crate) const DEAD: &[Dead] =
    &[Dead::Circumflex, Dead::Grave, Dead::Acute, Dead::Diaeresis, Dead::Tilde];

impl Dead {
    /// The spacing diacritic — what a doubled dead key produces, and what an
    /// uncomposed one emits before the character that followed it.
    pub const fn spacing(self) -> &'static str {
        match self {
            Dead::Circumflex => "^",
            Dead::Grave => "`",
            Dead::Acute => "\u{b4}",
            Dead::Diaeresis => "\u{a8}",
            Dead::Tilde => "~",
        }
    }

    /// What the dead key produces before a space. Not always the spacing
    /// diacritic: the reference gives `´` an apostrophe and `¨` a quotation
    /// mark, which is how those two ASCII characters stay reachable on a
    /// layout that spends their keys on diacritics.
    pub const fn ascii(self) -> &'static str {
        match self {
            Dead::Circumflex => "^",
            Dead::Grave => "`",
            Dead::Acute => "'",
            Dead::Diaeresis => "\"",
            Dead::Tilde => "~",
        }
    }

    pub const fn key(self) -> Key {
        Key::Dead(self)
    }
}

/// `(diacritic, base, result)`, sorted, one row per reference rule.
pub(crate) const TABLE: &[(Dead, u8, &str)] = &[
    (Dead::Acute, b'A', "Á"),
    (Dead::Acute, b'C', "Ć"),
    (Dead::Acute, b'E', "É"),
    (Dead::Acute, b'G', "Ǵ"),
    (Dead::Acute, b'I', "Í"),
    (Dead::Acute, b'K', "Ḱ"),
    (Dead::Acute, b'L', "Ĺ"),
    (Dead::Acute, b'M', "Ḿ"),
    (Dead::Acute, b'N', "Ń"),
    (Dead::Acute, b'O', "Ó"),
    (Dead::Acute, b'P', "Ṕ"),
    (Dead::Acute, b'R', "Ŕ"),
    (Dead::Acute, b'S', "Ś"),
    (Dead::Acute, b'U', "Ú"),
    (Dead::Acute, b'V', "Ǘ"),
    (Dead::Acute, b'W', "Ẃ"),
    (Dead::Acute, b'Y', "Ý"),
    (Dead::Acute, b'Z', "Ź"),
    (Dead::Acute, b'a', "á"),
    (Dead::Acute, b'c', "ć"),
    (Dead::Acute, b'e', "é"),
    (Dead::Acute, b'g', "ǵ"),
    (Dead::Acute, b'i', "í"),
    (Dead::Acute, b'k', "ḱ"),
    (Dead::Acute, b'l', "ĺ"),
    (Dead::Acute, b'm', "ḿ"),
    (Dead::Acute, b'n', "ń"),
    (Dead::Acute, b'o', "ó"),
    (Dead::Acute, b'p', "ṕ"),
    (Dead::Acute, b'r', "ŕ"),
    (Dead::Acute, b's', "ś"),
    (Dead::Acute, b'u', "ú"),
    (Dead::Acute, b'v', "ǘ"),
    (Dead::Acute, b'w', "ẃ"),
    (Dead::Acute, b'y', "ý"),
    (Dead::Acute, b'z', "ź"),
    (Dead::Circumflex, b'(', "⁽"),
    (Dead::Circumflex, b')', "⁾"),
    (Dead::Circumflex, b'+', "⁺"),
    (Dead::Circumflex, b'-', "⁻"),
    (Dead::Circumflex, b'.', "·"),
    (Dead::Circumflex, b'0', "⁰"),
    (Dead::Circumflex, b'1', "¹"),
    (Dead::Circumflex, b'2', "²"),
    (Dead::Circumflex, b'3', "³"),
    (Dead::Circumflex, b'4', "⁴"),
    (Dead::Circumflex, b'5', "⁵"),
    (Dead::Circumflex, b'6', "⁶"),
    (Dead::Circumflex, b'7', "⁷"),
    (Dead::Circumflex, b'8', "⁸"),
    (Dead::Circumflex, b'9', "⁹"),
    (Dead::Circumflex, b'=', "⁼"),
    (Dead::Circumflex, b'A', "Â"),
    (Dead::Circumflex, b'C', "Ĉ"),
    (Dead::Circumflex, b'E', "Ê"),
    (Dead::Circumflex, b'G', "Ĝ"),
    (Dead::Circumflex, b'H', "Ĥ"),
    (Dead::Circumflex, b'I', "Î"),
    (Dead::Circumflex, b'J', "Ĵ"),
    (Dead::Circumflex, b'O', "Ô"),
    (Dead::Circumflex, b'S', "Ŝ"),
    (Dead::Circumflex, b'U', "Û"),
    (Dead::Circumflex, b'W', "Ŵ"),
    (Dead::Circumflex, b'Y', "Ŷ"),
    (Dead::Circumflex, b'Z', "Ẑ"),
    (Dead::Circumflex, b'a', "â"),
    (Dead::Circumflex, b'c', "ĉ"),
    (Dead::Circumflex, b'e', "ê"),
    (Dead::Circumflex, b'g', "ĝ"),
    (Dead::Circumflex, b'h', "ĥ"),
    (Dead::Circumflex, b'i', "î"),
    (Dead::Circumflex, b'j', "ĵ"),
    (Dead::Circumflex, b'o', "ô"),
    (Dead::Circumflex, b's', "ŝ"),
    (Dead::Circumflex, b'u', "û"),
    (Dead::Circumflex, b'w', "ŵ"),
    (Dead::Circumflex, b'y', "ŷ"),
    (Dead::Circumflex, b'z', "ẑ"),
    (Dead::Diaeresis, b'A', "Ä"),
    (Dead::Diaeresis, b'E', "Ë"),
    (Dead::Diaeresis, b'H', "Ḧ"),
    (Dead::Diaeresis, b'I', "Ï"),
    (Dead::Diaeresis, b'O', "Ö"),
    (Dead::Diaeresis, b'U', "Ü"),
    (Dead::Diaeresis, b'W', "Ẅ"),
    (Dead::Diaeresis, b'X', "Ẍ"),
    (Dead::Diaeresis, b'Y', "Ÿ"),
    (Dead::Diaeresis, b'a', "ä"),
    (Dead::Diaeresis, b'e', "ë"),
    (Dead::Diaeresis, b'h', "ḧ"),
    (Dead::Diaeresis, b'i', "ï"),
    (Dead::Diaeresis, b'o', "ö"),
    (Dead::Diaeresis, b't', "ẗ"),
    (Dead::Diaeresis, b'u', "ü"),
    (Dead::Diaeresis, b'w', "ẅ"),
    (Dead::Diaeresis, b'x', "ẍ"),
    (Dead::Diaeresis, b'y', "ÿ"),
    (Dead::Grave, b'A', "À"),
    (Dead::Grave, b'E', "È"),
    (Dead::Grave, b'I', "Ì"),
    (Dead::Grave, b'N', "Ǹ"),
    (Dead::Grave, b'O', "Ò"),
    (Dead::Grave, b'U', "Ù"),
    (Dead::Grave, b'V', "Ǜ"),
    (Dead::Grave, b'W', "Ẁ"),
    (Dead::Grave, b'Y', "Ỳ"),
    (Dead::Grave, b'a', "à"),
    (Dead::Grave, b'e', "è"),
    (Dead::Grave, b'i', "ì"),
    (Dead::Grave, b'n', "ǹ"),
    (Dead::Grave, b'o', "ò"),
    (Dead::Grave, b'u', "ù"),
    (Dead::Grave, b'v', "ǜ"),
    (Dead::Grave, b'w', "ẁ"),
    (Dead::Grave, b'y', "ỳ"),
    (Dead::Tilde, b'<', "≲"),
    (Dead::Tilde, b'=', "≃"),
    (Dead::Tilde, b'>', "≳"),
    (Dead::Tilde, b'A', "Ã"),
    (Dead::Tilde, b'E', "Ẽ"),
    (Dead::Tilde, b'I', "Ĩ"),
    (Dead::Tilde, b'N', "Ñ"),
    (Dead::Tilde, b'O', "Õ"),
    (Dead::Tilde, b'U', "Ũ"),
    (Dead::Tilde, b'V', "Ṽ"),
    (Dead::Tilde, b'Y', "Ỹ"),
    (Dead::Tilde, b'a', "ã"),
    (Dead::Tilde, b'e', "ẽ"),
    (Dead::Tilde, b'i', "ĩ"),
    (Dead::Tilde, b'n', "ñ"),
    (Dead::Tilde, b'o', "õ"),
    (Dead::Tilde, b'u', "ũ"),
    (Dead::Tilde, b'v', "ṽ"),
    (Dead::Tilde, b'y', "ỹ"),
];

/// What `dead` and `base` compose to, if the reference says they do.
///
/// `base` is the whole output of the following key, not its first byte: a key
/// that types `ä` composes with nothing, and taking a prefix would have it
/// compose as whatever byte `ä` starts with.
pub(crate) fn compose(dead: Dead, base: &str) -> Option<&'static str> {
    let [b] = *base.as_bytes() else { return None };
    TABLE.iter().find(|&&(d, k, _)| d == dead && k == b).map(|&(_, _, out)| out)
}

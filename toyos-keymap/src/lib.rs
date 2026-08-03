//! Keyboard layouts and dead-key composition.
//!
//! A layout maps a HID usage and a modifier level to one of three things: a
//! string, a dead key, or nothing. [`Composer`] turns that stream into the
//! bytes a key press delivers, holding a pending diacritic between two
//! presses, and [`Translator`] is the whole of what a surface owner does with
//! a transition — layout, composition, control codes and escape sequences.
//!
//! Userland only. The kernel delivers the transition and nothing else, so
//! nothing here needs a kernel: the tables are data and the composition is a
//! two-state machine over them, which is what makes this testable on the host
//! at millisecond cost.

#![no_std]
#![forbid(unsafe_code)]

mod compose;
pub mod detect;
mod layouts;
mod translate;

pub use compose::Dead;
pub use layouts::{LAYOUTS, DEFAULT_LAYOUT};
pub use translate::{Mods, Translator};

/// What a layout says one (usage, level) produces.
///
/// A sum type rather than a string that is empty when there is nothing: a key
/// with no character at this level and a key that types a dead diacritic are
/// different answers, and an empty string can only express the first by
/// convention.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// The layout defines nothing here.
    None,
    Chars(&'static str),
    Dead(Dead),
}

pub struct KeyEntry {
    pub normal: Key,
    pub shift: Key,
    pub option: Key,
    pub shift_option: Key,
}

/// The four levels an entry has, indexed the way [`Layout::lookup`] selects
/// them, so a walk over every level of every key needs no second spelling of
/// the order.
pub const LEVELS: usize = 4;

impl KeyEntry {
    pub const fn level(&self, i: usize) -> Key {
        match i {
            0 => self.normal,
            1 => self.shift,
            2 => self.option,
            _ => self.shift_option,
        }
    }
}

/// The lowest and highest HID usage a layout table covers.
pub const FIRST_USAGE: u8 = 0x04;
pub const LAST_USAGE: u8 = 0x38;
/// HID 0x64: the ISO key between left Shift and the bottom letter row.
pub const ISO_USAGE: u8 = 0x64;

const TABLE_LEN: usize = (LAST_USAGE - FIRST_USAGE + 1) as usize;

pub struct Layout {
    pub name: &'static str,
    pub keys: [KeyEntry; TABLE_LEN],
    pub iso_key: KeyEntry,
}

impl Layout {
    pub const fn entry(&self, usage: u8) -> Option<&KeyEntry> {
        if usage >= FIRST_USAGE && usage <= LAST_USAGE {
            Some(&self.keys[(usage - FIRST_USAGE) as usize])
        } else if usage == ISO_USAGE {
            Some(&self.iso_key)
        } else {
            None
        }
    }

    /// What this layout produces for `usage` at the level `shift` and `alt`
    /// select. A usage the layout does not cover is [`Key::None`], the same
    /// answer as a covered usage with nothing at that level — the caller acts
    /// on both identically, and distinguishing them would mean an `Option`
    /// whose `None` arm no caller wants.
    pub const fn lookup(&self, usage: u8, shift: bool, alt: bool) -> Key {
        match self.entry(usage) {
            Some(e) => e.level((shift as usize) | ((alt as usize) << 1)),
            None => Key::None,
        }
    }
}

/// Index of the layout named `name`, for [`LAYOUTS`].
pub fn by_name(name: &str) -> Option<usize> {
    LAYOUTS.iter().position(|l| l.name == name)
}

/// The most bytes one key press can deliver.
///
/// The bound is not slack: a pending diacritic that does not compose emits
/// itself followed by the character typed, and `¨` before `€` is exactly five
/// bytes. The anonymous consts below and in `translate` are the compile-time
/// proof that no table entry, no composition and no escape sequence exceeds
/// it.
pub const MAX_EMIT: usize = 5;

/// The bytes one key press delivers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Emit {
    bytes: [u8; MAX_EMIT],
    len: u8,
}

impl Emit {
    pub const EMPTY: Self = Self { bytes: [0; MAX_EMIT], len: 0 };

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// Always succeeds: every producer appends whole `&str`s or one ASCII
    /// control byte, so the buffer is UTF-8 by construction. The signature
    /// says so rather than handing back a `Result` no caller can act on.
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).expect("Emit is assembled from UTF-8")
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Append `s`.
    ///
    /// The assert is for a table this crate ships, not for anything a keyboard
    /// can send: the compile-time walk covers every layout, every composition
    /// and every escape sequence, so an entry that would overflow is a build
    /// error rather than a key press.
    fn push(&mut self, s: &str) {
        let at = self.len as usize;
        assert!(at + s.len() <= MAX_EMIT, "key output exceeds MAX_EMIT");
        self.bytes[at..at + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len() as u8;
    }

    pub(crate) fn of(s: &str) -> Self {
        let mut e = Self::EMPTY;
        e.push(s);
        e
    }

    /// One ASCII control byte — what Ctrl makes of a letter. Not `of`, which
    /// takes a `&str` and would need one static per code.
    pub(crate) fn of_byte(b: u8) -> Self {
        assert!(b.is_ascii(), "Emit holds UTF-8");
        let mut e = Self::EMPTY;
        e.bytes[0] = b;
        e.len = 1;
        e
    }

    fn of2(a: &str, b: &str) -> Self {
        let mut e = Self::EMPTY;
        e.push(a);
        e.push(b);
        e
    }
}

impl core::fmt::Debug for Emit {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match core::str::from_utf8(self.as_bytes()) {
            Ok(s) => write!(f, "{s:?}"),
            Err(_) => write!(f, "{:?}", self.as_bytes()),
        }
    }
}

/// Holds the diacritic a dead key left pending.
///
/// One per surface — see [`Translator`]. Not one per keyboard: a dead key
/// pressed on one keyboard and the letter typed on another must compose, and
/// the Shift that makes it a capital may come from a third. The surface's host
/// has already merged those three into one stream.
#[derive(Clone, Copy, Default)]
pub struct Composer {
    pending: Option<Dead>,
}

impl Composer {
    pub const fn new() -> Self {
        Self { pending: None }
    }

    pub fn pending(&self) -> Option<Dead> {
        self.pending
    }

    /// Drop a pending diacritic without emitting it. The layout it came from
    /// is no longer the layout that would compose it.
    pub fn reset(&mut self) {
        self.pending = None;
    }

    /// Feed one key press.
    ///
    /// [`Key::None`] leaves a pending diacritic alone. That is what makes
    /// `^`, Shift, `e` produce `Ê`: every key the layout does not define —
    /// modifiers above all — is a key the composition does not see.
    pub fn press(&mut self, key: Key) -> Emit {
        let Some(pending) = self.pending else {
            return match key {
                Key::None => Emit::EMPTY,
                Key::Chars(s) => Emit::of(s),
                Key::Dead(d) => {
                    self.pending = Some(d);
                    Emit::EMPTY
                }
            };
        };

        match key {
            Key::None => Emit::EMPTY,
            // The reference gives a doubled dead key the spacing diacritic and
            // a dead key before a space the ASCII character, and for `´` and
            // `¨` those differ: `´´` is `´`, `´ ` is `'`.
            Key::Dead(d) if d == pending => {
                self.pending = None;
                Emit::of(pending.spacing())
            }
            Key::Dead(d) => {
                self.pending = Some(d);
                Emit::of(pending.spacing())
            }
            Key::Chars(s) => {
                self.pending = None;
                if s == " " {
                    return Emit::of(pending.ascii());
                }
                match compose::compose(pending, s) {
                    Some(c) => Emit::of(c),
                    // The reference's own engine discards an unmatched
                    // sequence, losing both key presses. Emitting the
                    // diacritic and then the character is what every desktop
                    // toolkit does and what this OS's rule against silent loss
                    // requires.
                    None => Emit::of2(pending.spacing(), s),
                }
            }
        }
    }
}

// Proof that nothing any table can produce overruns `MAX_EMIT`. Anonymous
// because rustc evaluates every non-generic const item whether or not anything
// reads it, so a table entry that broke the bound fails the build rather than
// tripping an assert on a key press.
const _: () = {
    let mut worst_chars = 0;
    let mut li = 0;
    while li < LAYOUTS.len() {
        let layout = LAYOUTS[li];
        let mut ui = FIRST_USAGE;
        while ui <= LAST_USAGE {
            worst_chars = worst_entry(&layout.keys[(ui - FIRST_USAGE) as usize], worst_chars);
            ui += 1;
        }
        worst_chars = worst_entry(&layout.iso_key, worst_chars);
        li += 1;
    }

    let mut worst_spacing = 0;
    let mut di = 0;
    while di < compose::DEAD.len() {
        let d = compose::DEAD[di];
        if d.spacing().len() > worst_spacing {
            worst_spacing = d.spacing().len();
        }
        assert!(d.ascii().len() <= MAX_EMIT);
        di += 1;
    }

    let mut ci = 0;
    while ci < compose::TABLE.len() {
        assert!(compose::TABLE[ci].2.len() <= MAX_EMIT);
        ci += 1;
    }

    assert!(worst_spacing + worst_chars <= MAX_EMIT);
};

const fn worst_entry(e: &KeyEntry, mut worst: usize) -> usize {
    let mut i = 0;
    while i < LEVELS {
        if let Key::Chars(s) = e.level(i) {
            if s.len() > worst {
                worst = s.len();
            }
        }
        i += 1;
    }
    worst
}

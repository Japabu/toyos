//! One key transition to the bytes it types.
//!
//! The whole of what a surface does with a key press: the layout table, the
//! dead-key machine over it, the control codes Ctrl makes of the letter row,
//! and the escape sequences the keys no layout defines send instead.
//!
//! One instance per surface. That is stricter than it sounds — a pending
//! diacritic belongs to the thing being typed into, so a `^` typed at one
//! terminal must not compose with the `e` typed at another. The kernel used to
//! hold one for the whole machine, which made those two the same key press.

use crate::{Composer, Emit, DEFAULT_LAYOUT, LAYOUTS, MAX_EMIT};

/// The modifier level a press is at, as the surface's host reports it.
///
/// Three fields rather than the ABI's bitmask: this crate is the layout
/// tables and knows nothing about a kernel event's encoding, and a caller that
/// hands over a mask has to agree with it about which bit is which.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

/// The letter row, which is the range Ctrl turns into control codes.
const USAGE_A: u8 = 0x04;
const USAGE_Z: u8 = 0x1D;

const HOME: u8 = 0x4A;
const PAGE_UP: u8 = 0x4B;
const DELETE: u8 = 0x4C;
const END: u8 = 0x4D;
const PAGE_DOWN: u8 = 0x4E;
const RIGHT: u8 = 0x4F;
const LEFT: u8 = 0x50;
const DOWN: u8 = 0x51;
const UP: u8 = 0x52;

/// The keys no layout has an entry for, and the sequence each one sends.
///
/// Here rather than in a terminal because every surface owner sends the same
/// bytes for them and there is nothing per-surface to decide.
const ESCAPES: &[(u8, &str)] = &[
    (HOME, "\x1b[H"),
    (PAGE_UP, "\x1b[5~"),
    (DELETE, "\x1b[3~"),
    (END, "\x1b[F"),
    (PAGE_DOWN, "\x1b[6~"),
    (RIGHT, "\x1b[C"),
    (LEFT, "\x1b[D"),
    (DOWN, "\x1b[B"),
    (UP, "\x1b[A"),
];

/// A layout and the dead-key state over it.
pub struct Translator {
    layout: usize,
    composer: Composer,
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}

impl Translator {
    pub const fn new() -> Self {
        Self { layout: DEFAULT_LAYOUT, composer: Composer::new() }
    }

    pub fn layout(&self) -> &'static str {
        LAYOUTS[self.layout].name
    }

    /// Select the layout named `name`. False iff no layout has that name, in
    /// which case the current one is unchanged.
    pub fn set_layout(&mut self, name: &str) -> bool {
        let Some(index) = crate::by_name(name) else {
            return false;
        };
        self.layout = index;
        // A diacritic the old layout left pending would compose against the
        // new one's table.
        self.composer.reset();
        true
    }

    /// The bytes one key press types.
    ///
    /// Presses only: a release types nothing, and feeding one here would let
    /// the key that ends a chord consume the diacritic the key that started it
    /// left pending.
    pub fn press(&mut self, usage: u8, mods: Mods) -> Emit {
        if let Some(&(_, seq)) = ESCAPES.iter().find(|&&(u, _)| u == usage) {
            return Emit::of(seq);
        }

        if mods.ctrl && (USAGE_A..=USAGE_Z).contains(&usage) {
            return Emit::of_byte(usage - USAGE_A + 1);
        }

        // Both returns above are keys no layout defines, and neither disturbs
        // a pending diacritic: they are the same class of key as Shift, which
        // has to be pressable between `^` and `e`.
        let key = LAYOUTS[self.layout].lookup(usage, mods.shift, mods.alt);
        self.composer.press(key)
    }
}

/// The escape sequences are the third producer [`MAX_EMIT`] has to hold, and
/// the only one whose length is not derived from a layout table.
const _: () = {
    let mut i = 0;
    while i < ESCAPES.len() {
        assert!(ESCAPES[i].1.len() <= MAX_EMIT);
        i += 1;
    }
};

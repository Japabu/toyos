//! Identifying a keyboard by asking its owner to press labelled keys.
//!
//! The sensor is the user's eyes. Every layout here agrees about what a key
//! *does* once you know which layout it is, and disagrees about *where* a
//! given legend sits — so a press of the key printed `Z` reports one HID usage
//! on a QWERTY board and another on a QWERTZ one, and that is the whole
//! measurement. It is the technique Debian's installer uses.
//!
//! The questions are data. Which one to ask next is derived from them against
//! the candidates still alive, so a fifth layout is rows in [`QUESTIONS`] and
//! not a new branch: there is no tree in the source to get out of step with
//! the table.

use crate::{by_name, LAYOUTS};

/// One key, named by what is printed on it.
pub struct Question {
    /// The legend, as the keycap carries it.
    ///
    /// Only characters the console font can draw. Its coverage is Latin-1 plus
    /// box-drawing, and a codepoint outside that renders as `?` — a wizard
    /// that asked for `€` would be asking for a key it just printed as a
    /// question mark. `legends_are_renderable` is the gate.
    pub legend: &'static str,
    /// The HID usage each layout puts that legend on. A layout absent here is
    /// one this question cannot speak for, and [`Detector::step`] will not ask
    /// it while such a layout is still a candidate.
    pub answers: &'static [(&'static str, u8)],
}

/// Two presses tell today's four layouts apart, and one tells US from the
/// rest.
///
/// `Z` is the QWERTY/QWERTZ split. `§` then separates the three QWERTZ
/// layouts: German prints it on the `3` key, a Swiss PC keyboard gives it the
/// key left of `1`, and an Apple ISO board reports that same physical key as
/// the ISO usage instead — which is the quirk `swiss-german-mac`'s table is
/// built around, so the wizard reads it rather than working around it.
pub const QUESTIONS: &[Question] = &[
    Question {
        legend: "Z",
        answers: &[
            ("us", 0x1D),
            ("de", 0x1C),
            ("swiss-german", 0x1C),
            ("swiss-german-mac", 0x1C),
        ],
    },
    Question {
        legend: "\u{a7}",
        answers: &[("de", 0x20), ("swiss-german", 0x35), ("swiss-german-mac", 0x64)],
    },
];

/// What the wizard should do next.
pub enum Step<'a> {
    Ask(Ask<'a>),
    /// Index into [`LAYOUTS`].
    Decided(usize),
    /// No layout matches what was pressed, or none of the remaining questions
    /// can separate the ones that do. The wizard offers the manual list; it
    /// never guesses.
    Unrecognized,
}

/// A question that has been put to the user, and the only way to answer one.
///
/// It borrows the detector, so answering a question nobody asked does not
/// compile.
pub struct Ask<'a> {
    detector: &'a mut Detector,
    index: usize,
}

impl Ask<'_> {
    pub fn legend(&self) -> &'static str {
        QUESTIONS[self.index].legend
    }

    /// Record the HID usage the user's press reported.
    pub fn observe(self, usage: u8) {
        let q = &QUESTIONS[self.index];
        self.detector.alive &= mask_of(q, |u| u == usage);
        self.detector.asked |= 1 << self.index;
    }
}

/// Which layouts are still possible.
pub struct Detector {
    alive: u32,
    asked: u32,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector {
    pub fn new() -> Self {
        assert!(LAYOUTS.len() <= u32::BITS as usize, "one bit per layout");
        assert!(QUESTIONS.len() <= u32::BITS as usize, "one bit per question");
        Self { alive: (1 << LAYOUTS.len()) - 1, asked: 0 }
    }

    pub fn candidates(&self) -> impl Iterator<Item = &'static str> + '_ {
        LAYOUTS
            .iter()
            .enumerate()
            .filter(move |(i, _)| self.alive & (1 << i) != 0)
            .map(|(_, l)| l.name)
    }

    pub fn step(&mut self) -> Step<'_> {
        match self.alive.count_ones() {
            0 => return Step::Unrecognized,
            1 => return Step::Decided(self.alive.trailing_zeros() as usize),
            _ => {}
        }
        let alive = self.alive;
        let next = QUESTIONS.iter().enumerate().position(|(i, q)| {
            self.asked & (1 << i) == 0 && covers(q, alive) && discriminates(q, alive)
        });
        match next {
            Some(index) => Step::Ask(Ask { detector: self, index }),
            None => Step::Unrecognized,
        }
    }
}

/// The layouts `q` names whose usage satisfies `f`.
fn mask_of(q: &Question, f: impl Fn(u8) -> bool) -> u32 {
    q.answers
        .iter()
        .filter(|&&(_, u)| f(u))
        .filter_map(|&(name, _)| by_name(name))
        .fold(0, |m, i| m | (1 << i))
}

/// Does `q` have an answer for every layout still alive? A question that does
/// not cannot be asked without eliminating a candidate it says nothing about.
fn covers(q: &Question, alive: u32) -> bool {
    alive & !mask_of(q, |_| true) == 0
}

/// Would `q` split the live candidates? A question every one of them answers
/// identically costs the user a keypress and learns nothing.
fn discriminates(q: &Question, alive: u32) -> bool {
    let usages = q
        .answers
        .iter()
        .filter(|&&(name, _)| by_name(name).is_some_and(|i| alive & (1 << i) != 0))
        .map(|&(_, u)| u);
    let mut first = None;
    for u in usages {
        match first {
            None => first = Some(u),
            Some(f) if f != u => return true,
            Some(_) => {}
        }
    }
    false
}

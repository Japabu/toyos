//! A value too wide for its line keeps both of its ends.
//!
//! **No bound on the record fixes an unbounded name.** A demangled Rust symbol
//! is bounded by nothing the kernel controls — `late_panic::Nest` is a generic
//! nested in itself and nothing stops it being nested again — so
//! `MAX_RECORD_MESSAGE` can only be raised until the *ordinary* line fits. What
//! is left is to decide which bytes of an over-wide one survive, and the answer
//! is both ends: the head is the crate and the module path, the tail is the
//! function, and a backtrace with only one of them names nothing.
//!
//! It is the **producer's** decision, so the record still holds a whole message
//! and `elided` still means what the ABI says it means.
//!
//! **That is also why this is not in `toyos-abi`**, where `elided` and the rest
//! of `LogRecord` live. The ABI is the contract between the kernel and
//! userland; this is one side deciding what to put in a field before it fills
//! it in, with one caller and nothing on the far side of the contract that
//! needs it — and `toyos-abi` is a dependency of `std`, so a formatter that
//! reaches no reader would ship in every program.
//!
//! Pure: `core::fmt` and nothing else — no allocation, no `unsafe`, and no
//! record. The one caller is `kernel/src/symbols.rs`, which spends the budget
//! `MAX_RECORD_MESSAGE` leaves a backtrace frame, and everything it decides is
//! checked here on the host, where a seam falling inside a four-byte character
//! costs milliseconds and no guest at all — the tree's own widest symbol is
//! under a third of what triggers the elision, so no boot reaches it.
//!
//! **Naming nothing outside `core` is the property this crate has to keep to
//! stay testable**: a dependency added here is the code leaving the host, and
//! the tests go with it.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

use core::fmt::{self, Display, Write};

/// The widest `...[N bytes elided]...` this can write.
///
/// **Counted rather than eyeballed, and it belongs to the budget rather than to
/// the caller's overhead.** An earlier version added the marker on top of a
/// head-plus-tail that already spent the whole budget, so a name one byte over
/// it produced a *line* over the record's bound — and what a record drops is
/// its tail, which is the half this exists to keep. `N` is a `usize`, so twenty
/// digits is every value one can have.
pub const MARKER_MAX: usize = "...[".len() + 20 + " bytes elided]...".len();

/// The widest thing `Elided<_, HEAD, TAIL>` can render.
///
/// Both branches are under it: a value that fits is at most `HEAD + TAIL` and
/// one that does not is exactly `HEAD` plus a marker plus at most `TAIL`.
pub const fn widest(head: usize, tail: usize) -> usize {
    head + MARKER_MAX + tail
}

/// `value`, rendered head-and-tail with the middle counted out when it is wider
/// than `HEAD + TAIL`.
///
/// Rendered through `{:#}`, because the one caller's value is a
/// `rustc_demangle::Demangle` and the alternate form is the one without the
/// hash suffix.
pub struct Elided<D, const HEAD: usize, const TAIL: usize>(pub D);

impl<D: Display, const HEAD: usize, const TAIL: usize> Display for Elided<D, HEAD, TAIL> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut count = Count(0);
        // `Count` cannot fail, so this measures rather than renders.
        let _ = write!(count, "{:#}", self.0);
        // Eliding something that already fits would make it *longer*, so the
        // test is against what is kept and not against the budget.
        if count.0 <= HEAD + TAIL {
            return write!(f, "{:#}", self.0);
        }
        let mut both = HeadTail::<HEAD, TAIL> { out: f, seen: 0, shown: 0, tail: [0; TAIL], filled: 0 };
        write!(both, "{:#}", self.0)?;
        both.finish()
    }
}

/// A `fmt::Write` that measures and writes nowhere.
struct Count(usize);

impl Write for Count {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0 = self.0.saturating_add(s.len());
        Ok(())
    }
}

/// Passes a value's head straight through and keeps its tail in a fixed buffer,
/// so what an over-wide one loses is its middle.
struct HeadTail<'a, 'b, const HEAD: usize, const TAIL: usize> {
    out: &'a mut fmt::Formatter<'b>,
    /// Bytes the value has produced.
    seen: usize,
    /// Bytes of head already written out. Below `HEAD` by up to three when the
    /// bound falls inside a character.
    shown: usize,
    tail: [u8; TAIL],
    filled: usize,
}

impl<const HEAD: usize, const TAIL: usize> HeadTail<'_, '_, HEAD, TAIL> {
    /// Keep the last `TAIL` bytes, contiguously.
    ///
    /// A ring would save the shifting and cost the seam: a character split
    /// across the wrap has no `&str` to be part of. The shifts are a few
    /// hundred bytes per chunk on a path that is already formatting.
    fn keep_tail(&mut self, bytes: &[u8]) {
        if let Some(last) = bytes.len().checked_sub(TAIL) {
            self.tail.copy_from_slice(&bytes[last..]);
            self.filled = TAIL;
            return;
        }
        let drop = (self.filled + bytes.len()).saturating_sub(TAIL);
        if drop > 0 {
            self.tail.copy_within(drop..self.filled, 0);
            self.filled -= drop;
        }
        let end = self.filled + bytes.len();
        if let Some(slot) = self.tail.get_mut(self.filled..end) {
            slot.copy_from_slice(bytes);
            self.filled = end;
        }
    }

    fn finish(self) -> fmt::Result {
        // The tail's first byte is wherever the shifting left it, which can be
        // inside a character; a character is at most four bytes, so at most
        // three of one can be.
        let mut tail: &[u8] = self.tail.get(..self.filled).unwrap_or(&[]);
        for _ in 0..3 {
            if core::str::from_utf8(tail).is_ok() {
                break;
            }
            tail = tail.get(1..).unwrap_or(&[]);
        }
        let text = core::str::from_utf8(tail).unwrap_or("");
        let elided = self.seen.saturating_sub(self.shown).saturating_sub(text.len());
        write!(self.out, "...[{elided} bytes elided]...{text}")
    }
}

impl<const HEAD: usize, const TAIL: usize> Write for HeadTail<'_, '_, HEAD, TAIL> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        // `shown == seen` is "the head is still open": the one chunk that
        // reaches the bound closes it, and a chunk cut short of it by a
        // character boundary closes it too rather than being retried.
        if self.shown == self.seen && self.seen < HEAD {
            let room = HEAD - self.seen;
            if bytes.len() <= room {
                self.out.write_str(s)?;
                self.shown += bytes.len();
            } else {
                let mut fit = room;
                while fit > 0 && !s.is_char_boundary(fit) {
                    fit -= 1;
                }
                self.out.write_str(s.get(..fit).unwrap_or(""))?;
                self.shown += fit;
            }
        }
        self.seen += bytes.len();
        self.keep_tail(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::format;
    use std::string::{String, ToString};

    const HEAD: usize = 16;
    const TAIL: usize = 8;

    fn render(value: &str) -> String {
        Elided::<_, HEAD, TAIL>(value).to_string()
    }

    /// A value written in one piece per character, which is how
    /// `rustc_demangle` writes a symbol: the seam logic has to hold across
    /// chunk boundaries it does not choose.
    struct PerChar<'a>(&'a str);

    impl Display for PerChar<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            for ch in self.0.chars() {
                write!(f, "{ch}")?;
            }
            Ok(())
        }
    }

    #[test]
    fn a_value_that_fits_is_untouched() {
        let fits = "a".repeat(HEAD + TAIL);
        assert_eq!(render(&fits), fits, "eliding something that fits would lengthen it");
        assert_eq!(render(""), "");
    }

    /// One byte past what is kept is where the marker starts costing more than
    /// it saves, and it is exactly where the branch has to be.
    #[test]
    fn one_byte_past_what_is_kept_elides() {
        let over = "a".repeat(HEAD + TAIL + 1);
        let got = render(&over);
        assert_eq!(got, "aaaaaaaaaaaaaaaa...[1 bytes elided]...aaaaaaaa");
        assert!(got.starts_with(&"a".repeat(HEAD)));
        assert!(got.ends_with(&"a".repeat(TAIL)));
    }

    /// **Both ends, which is the whole point.** The head names the module path
    /// and the tail names the function.
    #[test]
    fn both_ends_survive_and_the_count_is_exact() {
        let value = format!("{}{}{}", "H".repeat(HEAD), "M".repeat(500), "T".repeat(TAIL));
        let got = render(&value);
        assert_eq!(got, format!("{}...[500 bytes elided]...{}", "H".repeat(HEAD), "T".repeat(TAIL)));
        assert_eq!(
            HEAD + 500 + TAIL,
            value.len(),
            "the count has to be what was dropped, not what was kept"
        );
    }

    /// The bound is on the *rendered* width, and the marker is inside it.
    #[test]
    fn nothing_renders_wider_than_widest() {
        for len in 0..200 {
            let got = render(&"x".repeat(len));
            assert!(
                got.len() <= widest(HEAD, TAIL),
                "{len} bytes rendered {} > {}",
                got.len(),
                widest(HEAD, TAIL)
            );
        }
    }

    /// The head stops short of a character boundary rather than through it, and
    /// what that buys is **the head itself**. `write_str` slices with
    /// `s.get(..fit)`, and a `fit` inside a character makes that `None`, so the
    /// caller writes `""` — the whole head is lost, not a mangled byte of it.
    /// The failure is silent and total rather than visible and small, which is
    /// the harder kind to notice on a photograph of a panel.
    ///
    /// `€` is three bytes; `HEAD` is 16, so the sixth one straddles it.
    #[test]
    fn a_head_cut_inside_a_character_stops_before_it() {
        let value = format!("{}{}", "€".repeat(40), "T".repeat(TAIL));
        let got = render(&value);
        assert!(got.is_char_boundary(0));
        let head = got.split("...[").next().expect("a marker");
        assert_eq!(head, "€".repeat(5), "the head kept 15 of its 16 bytes and no half a character");
        assert!(got.ends_with(&"T".repeat(TAIL)));
    }

    /// The other seam: the tail's first byte is wherever the shifting left it,
    /// and a four-byte character straddling it costs three bytes rather than
    /// producing an invalid one.
    #[test]
    fn a_tail_cut_inside_a_character_drops_its_remainder() {
        // `TAIL` is 8 and `𝄞` is four bytes, so a run of them starting at an
        // odd offset leaves the tail beginning mid-character.
        let value = format!("{}x{}", "a".repeat(100), "𝄞".repeat(10));
        let got = render(&value);
        assert!(got.ends_with("𝄞𝄞"), "the tail is two whole characters, not eight bytes: {got}");
        assert_eq!(std::str::from_utf8(got.as_bytes()).is_ok(), true);
    }

    /// Every character straddles a seam sooner or later; sweeping the length
    /// walks the head cut through all four widths.
    #[test]
    fn no_length_and_no_width_produces_invalid_utf8() {
        for ch in ["a", "é", "€", "𝄞"] {
            for n in 0..64 {
                let value = ch.repeat(n);
                let got = render(&value);
                assert!(
                    std::str::from_utf8(got.as_bytes()).is_ok(),
                    "{ch} x{n} rendered invalid UTF-8"
                );
                assert!(got.len() <= widest(HEAD, TAIL));
            }
        }
    }

    /// The chunking is the writer's, not ours: a value that arrives one
    /// character at a time must render the same as one that arrives whole.
    #[test]
    fn the_chunking_a_value_arrives_in_does_not_change_it() {
        for ch in ["a", "é", "𝄞"] {
            for n in 0..64 {
                let value = ch.repeat(n);
                assert_eq!(
                    Elided::<_, HEAD, TAIL>(PerChar(&value)).to_string(),
                    render(&value),
                    "{ch} x{n} rendered differently one character at a time"
                );
            }
        }
    }

    /// A tail wider than the value cannot read past what it was given.
    #[test]
    fn a_tail_wider_than_the_value_keeps_all_of_it() {
        assert_eq!(Elided::<_, 2, 64>("abcdef").to_string(), "abcdef");
    }
}

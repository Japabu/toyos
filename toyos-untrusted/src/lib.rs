//! A number that crossed a trust boundary, and the four ways it gets out.
//!
//! CLAUDE.md's law is *"input that crossed a trust boundary is never trusted
//! and never panics the kernel — it is refused"*, and the recorded failures of
//! it are all one shape: a `u32` a device wrote, or a `u16` a peer wrote, was
//! carried in a plain integer and then used as an index, a length or an
//! address. A plain integer records nothing about where it came from, so the
//! comparison that should stand between it and the array is a thing an author
//! has to remember. Six filed issues are six authors who did not.
//!
//! [`Untrusted<T>`] is that provenance, in the type. It has **no** arithmetic,
//! no `Deref`, no `From`, no cast and no accessor: the only functions on it
//! that return a bare integer take the bound to check against and answer
//! [`Result`]. So a value that came off DMA cannot reach an index without the
//! comparison, and a reviewer does not have to notice that it should —
//! the compiler refuses the code.
//!
//! **This is not a second vocabulary.** `UserAddr` (`kernel/src/mm/mod.rs`) is
//! the same idea for the *address* half of the same boundary, and says so:
//! *"the type's name is a claim, and this is the only constructor that makes it
//! true of a number userland chose"*. What was missing was the value half.
//! `toyos-pci`'s module doc already states the principle for one format —
//! *"a function that names a reserved BAR indicator is not a function to
//! truncate into range"* — and this is the noun that sentence was reaching for.
//!
//! # What it costs
//!
//! Nothing at runtime. [`Untrusted<T>`] is `#[repr(transparent)]` over `T` and
//! every method is `#[inline]`, so it compiles to the integer it wraps; the
//! checks are the compare-and-branch a correct call site had to perform anyway,
//! and the incorrect ones were only cheaper because they were wrong.
//!
//! # What it does not do
//!
//! It does not decide *what* the bound is — that is the driver's knowledge and
//! it stays at the driver. It carries no source: a value a device wrote and a
//! value a peer wrote are the same kind of not-known-yet, and splitting them
//! would put the interesting question ("which of these is reachable from
//! userland?") into a type where it cannot be answered instead of into the
//! module header where it can.
//!
//! `no_std`, no allocation, no dependencies, `forbid(unsafe_code)`.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

/// The integer widths a boundary hands over. Sealed: the point of the type is
/// that the set of things it can be is closed.
mod sealed {
    pub trait Sealed {}
}

/// An integer that can be widened losslessly for comparison.
pub trait Raw: sealed::Sealed + Copy + fmt::Debug {
    /// This value as a `u64`, which every bound below is expressed in. Lossless
    /// for every implementor, which is what makes a comparison against a bound
    /// mean what it says.
    fn widen(self) -> u64;
}

macro_rules! raw {
    ($($t:ty),*) => { $(
        impl sealed::Sealed for $t {}
        impl Raw for $t {
            #[inline]
            fn widen(self) -> u64 { self as u64 }
        }
    )* };
}

raw!(u8, u16, u32, u64);

/// Why a value that crossed a boundary is not the thing it was asked to be.
///
/// Carries both numbers, so the caller's message can name the noun and this can
/// name the disagreement — `msix::Unusable`'s division of labour, which is what
/// keeps a refusal readable without every bound owning its own error enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// It was asked to be an index into a table of `len` entries and is not.
    PastTable { value: u64, len: u64 },
    /// It was asked to be at most `bound` and is more.
    PastBound { value: u64, bound: u64 },
    /// It was asked to be exactly `wanted`.
    NotExactly { value: u64, wanted: u64 },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PastTable { value, len } => write!(f, "{value} indexes no entry of {len}"),
            Self::PastBound { value, bound } => write!(f, "{value} is past the bound of {bound}"),
            Self::NotExactly { value, wanted } => write!(f, "{value} where {wanted} was required"),
        }
    }
}

/// A number this side of the machine did not choose.
///
/// Wrap one at the read that produces it — the volatile load out of a used
/// ring, the config-space dword, the header field — and it stays wrapped until
/// something compares it with a bound. There is deliberately no way to unwrap
/// it without one:
///
/// ```compile_fail
/// # use toyos_untrusted::Untrusted;
/// let len = Untrusted::new(4096u32);
/// let n: u32 = len.into();          // no `From`, no `Into`
/// ```
///
/// ```compile_fail
/// # use toyos_untrusted::Untrusted;
/// let id = Untrusted::new(3u32);
/// let next = id + 1;                // no arithmetic
/// ```
///
/// ```compile_fail
/// # use toyos_untrusted::Untrusted;
/// let id = Untrusted::new(3u32);
/// let table = [0u8; 16];
/// let entry = table[id as usize];   // no cast
/// ```
///
/// ```compile_fail
/// # use toyos_untrusted::Untrusted;
/// let id = Untrusted::new(3u32);
/// if id < 16 { }                    // no ordering against a bare integer
/// ```
///
/// What it is for, in one line each:
///
/// ```
/// # use toyos_untrusted::{Untrusted, Refused};
/// let table = [0u8; 16];
/// // The device named descriptor 3 of 16 — an index, once compared.
/// assert_eq!(Untrusted::new(3u32).index(table.len()), Ok(3));
/// // And descriptor 0x1_0003, which narrows to 3 and is not.
/// assert_eq!(
///     Untrusted::new(0x1_0003u32).index(table.len()),
///     Err(Refused::PastTable { value: 0x1_0003, len: 16 }),
/// );
/// ```
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct Untrusted<T>(T);

impl<T: Raw> Untrusted<T> {
    /// Wrap a value that came from outside.
    ///
    /// Unrestricted on purpose: *making* one of these is always sound, and a
    /// constructor nobody could call would only move the laundering to whoever
    /// could. What is restricted is getting a number back out.
    #[inline]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// As an index into a table of `len` entries.
    ///
    /// The one exit that produces a `usize`, and the reason: a `usize` is what
    /// indexes, so every path to one goes past this comparison. `len` is the
    /// table's own length — `slice.len()`, or the queue size the descriptor
    /// table was allocated from — never a constant retyped beside it.
    #[inline]
    pub fn index(self, len: usize) -> Result<usize, Refused> {
        let value = self.0.widen();
        if value >= len as u64 {
            return Err(Refused::PastTable { value, len: len as u64 });
        }
        Ok(value as usize)
    }

    /// As a count, size or length of at most `bound`.
    ///
    /// Inclusive, because the values this guards are byte counts and a buffer
    /// entirely filled is the ordinary case.
    #[inline]
    pub fn at_most(self, bound: u64) -> Result<u64, Refused> {
        let value = self.0.widen();
        if value > bound {
            return Err(Refused::PastBound { value, bound });
        }
        Ok(value)
    }

    /// As exactly `wanted` — for a field a protocol fixes, where anything else
    /// is not a value to clamp but a peer that is not speaking the protocol.
    #[inline]
    pub fn exactly(self, wanted: T) -> Result<u64, Refused> {
        let (value, wanted) = (self.0.widen(), wanted.widen());
        if value != wanted {
            return Err(Refused::NotExactly { value, wanted });
        }
        Ok(value)
    }

    /// Whether it is this value. Answering a `bool` decides nothing about
    /// memory, so this is not an exit — the caller learns only what it already
    /// wrote down.
    #[inline]
    pub fn is(self, value: T) -> bool {
        self.0.widen() == value.widen()
    }

    /// Decode it — mask a field out, shift, narrow — and keep the provenance.
    ///
    /// **The escape hatch that is not one.** Arithmetic on a value from outside
    /// is ordinary and necessary; what must not happen is the *result* becoming
    /// trusted because a computation happened to it. So this exists, and it
    /// returns another [`Untrusted`].
    #[inline]
    pub fn map<U: Raw>(self, f: impl FnOnce(T) -> U) -> Untrusted<U> {
        Untrusted(f(self.0))
    }
}

/// Prints the number, so a refusal can name what arrived.
///
/// Formatting is not an exit: it produces text, and no caller indexes with
/// text. Without it every refusal would have to launder the value to say what
/// it refused, which is the one thing that would make laundering necessary.
impl<T: fmt::Debug> fmt::Debug for Untrusted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Untrusted({:?})", self.0)
    }
}

impl<T: fmt::Display> fmt::Display for Untrusted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl<T: fmt::LowerHex> fmt::LowerHex for Untrusted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_is_below_the_table_it_indexes() {
        assert_eq!(Untrusted::new(0u32).index(16), Ok(0));
        assert_eq!(Untrusted::new(15u32).index(16), Ok(15));
        assert_eq!(
            Untrusted::new(16u32).index(16),
            Err(Refused::PastTable { value: 16, len: 16 })
        );
    }

    /// The defect this type is named for. `desc_to_rx` is `[u8; 16]` and the
    /// used ring's id is a `u32`; the code that read it wrote `id as u16`, so
    /// `0x1_0003` became descriptor 3 — a real entry — and every check written
    /// after the cast agreed. Widening rather than narrowing is the whole
    /// difference, and it is not a thing a call site can get wrong here.
    #[test]
    fn a_value_whose_low_bits_are_in_range_is_still_out_of_range() {
        assert_eq!(
            Untrusted::new(0x1_0003u32).index(16),
            Err(Refused::PastTable { value: 0x1_0003, len: 16 })
        );
        assert_eq!(
            Untrusted::new(0xFFFF_0000u32).index(16),
            Err(Refused::PastTable { value: 0xFFFF_0000, len: 16 })
        );
    }

    #[test]
    fn an_empty_table_indexes_nothing() {
        assert_eq!(
            Untrusted::new(0u32).index(0),
            Err(Refused::PastTable { value: 0, len: 0 })
        );
    }

    /// A length is inclusive of its bound and an index is not, and the two
    /// exits are separate so that a call site cannot pick the wrong one by
    /// writing `len - 1` and getting it wrong at the boundary.
    #[test]
    fn a_length_may_equal_its_bound_where_an_index_may_not() {
        assert_eq!(Untrusted::new(256u32).at_most(256), Ok(256));
        assert_eq!(
            Untrusted::new(257u32).at_most(256),
            Err(Refused::PastBound { value: 257, bound: 256 })
        );
        assert_eq!(
            Untrusted::new(256u32).index(256),
            Err(Refused::PastTable { value: 256, len: 256 })
        );
    }

    #[test]
    fn every_width_widens_rather_than_wrapping() {
        assert_eq!(Untrusted::new(u8::MAX).at_most(255), Ok(255));
        assert_eq!(Untrusted::new(u16::MAX).at_most(65_535), Ok(65_535));
        assert_eq!(Untrusted::new(u32::MAX).at_most(u32::MAX as u64), Ok(u32::MAX as u64));
        assert_eq!(Untrusted::new(u64::MAX).at_most(u64::MAX), Ok(u64::MAX));
        assert_eq!(
            Untrusted::new(u64::MAX).index(16),
            Err(Refused::PastTable { value: u64::MAX, len: 16 })
        );
    }

    #[test]
    fn exactly_refuses_by_naming_both_numbers() {
        assert_eq!(Untrusted::new(12u32).exactly(12), Ok(12));
        assert_eq!(
            Untrusted::new(13u32).exactly(12),
            Err(Refused::NotExactly { value: 13, wanted: 12 })
        );
    }

    /// Deciding a `bool` reaches no memory, so equality against a constant the
    /// caller wrote is not a way out — and it must keep working, because a
    /// driver comparing a status word against a known code is the ordinary
    /// thing to do with a value it does not trust.
    #[test]
    fn asking_whether_it_is_a_known_value_is_not_an_exit() {
        assert!(Untrusted::new(0u32).is(0));
        assert!(!Untrusted::new(1u32).is(0));
    }

    /// The property the compile-fail cases above state negatively: a decode
    /// produces another untrusted value, so the bound is still owed after it.
    #[test]
    fn decoding_keeps_the_provenance() {
        let bar = Untrusted::new(0xFEBD_4004u32);
        let low_bits = bar.map(|v| (v & 0xF) as u8);
        assert!(low_bits.is(4));
        assert_eq!(
            bar.map(|v| v >> 4).index(16),
            Err(Refused::PastTable { value: 0x0FEB_D400, len: 16 })
        );
    }

    /// Two `Untrusted`s compare, because neither side of that comparison is a
    /// claim about memory. This is what lets a driver notice that the device
    /// said the same thing twice.
    #[test]
    fn two_untrusted_values_compare_with_each_other() {
        assert_eq!(Untrusted::new(7u32), Untrusted::new(7u32));
        assert_ne!(Untrusted::new(7u32), Untrusted::new(8u32));
    }

    #[test]
    fn a_refusal_says_which_number_and_which_bound() {
        extern crate std;
        use std::string::ToString;
        assert_eq!(
            Refused::PastTable { value: 0x1_0003, len: 16 }.to_string(),
            "65539 indexes no entry of 16"
        );
        assert_eq!(
            Refused::PastBound { value: 4097, bound: 4096 }.to_string(),
            "4097 is past the bound of 4096"
        );
        assert_eq!(
            Refused::NotExactly { value: 13, wanted: 12 }.to_string(),
            "13 where 12 was required"
        );
    }

    /// `#[repr(transparent)]` and nothing else in the struct: the type is the
    /// integer, so adopting it at a site in a hot path costs that site nothing.
    #[test]
    fn it_is_the_integer_it_wraps() {
        use core::mem::{align_of, size_of};
        assert_eq!(size_of::<Untrusted<u32>>(), size_of::<u32>());
        assert_eq!(align_of::<Untrusted<u32>>(), align_of::<u32>());
        assert_eq!(size_of::<Untrusted<u64>>(), size_of::<u64>());
    }
}

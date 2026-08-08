//! The MSI capability structure (PCIe base spec §7.7.1).
//!
//! Not a lesser or older mechanism than MSI-X: the device performs the same
//! write to the same address, configured in config space instead of in a table
//! inside a BAR. What makes it worth its own decode is that its register
//! layout *moves* — a function that takes a 64-bit address has an extra dword
//! before the data register, which pushes the data and mask registers along
//! with it. Writing a vector at the wrong one of those offsets lands in
//! whatever capability comes next in the list.

/// This capability's id in a function's capability list.
pub const CAP_ID: u8 = 0x05;

/// Byte offset of Message Control, from the capability header. Everything
/// below it moves with [`Msi::wide`].
pub const MESSAGE_CONTROL: u64 = 0x02;

const ENABLE: u16 = 1 << 0;
const MULTI_MESSAGE_CAPABLE: u16 = 0x7 << 1;
const MULTI_MESSAGE_ENABLE: u16 = 0x7 << 4;
const ADDRESS_64: u16 = 1 << 7;
const PER_VECTOR_MASK: u16 = 1 << 8;

/// What a function's Message Control says about the shape of the rest of its
/// capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Msi {
    wide: bool,
    per_vector_mask: bool,
    vectors: u16,
}

impl Msi {
    pub fn decode(message_control: u16) -> Self {
        Self {
            wide: message_control & ADDRESS_64 != 0,
            per_vector_mask: message_control & PER_VECTOR_MASK != 0,
            // Multiple Message Capable is an exponent, not a count.
            vectors: 1 << ((message_control & MULTI_MESSAGE_CAPABLE) >> 1),
        }
    }

    /// Whether this function takes a 64-bit message address.
    pub fn wide(&self) -> bool {
        self.wide
    }

    /// How many consecutive vectors the function says it can raise. This
    /// kernel arms one; the count is what a diagnostic reports.
    pub fn vectors(&self) -> u16 {
        self.vectors
    }

    pub fn address_lo(&self) -> u64 {
        0x04
    }

    /// `None` on a function whose address register is 32 bits wide: there is
    /// no upper dword, and the register at this offset is the data one.
    pub fn address_hi(&self) -> Option<u64> {
        self.wide.then_some(0x08)
    }

    pub fn data(&self) -> u64 {
        if self.wide { 0x0C } else { 0x08 }
    }

    /// `None` on a function that does not implement per-vector masking. Its
    /// capability simply ends earlier, so this offset belongs to whatever
    /// comes next in the list.
    pub fn mask(&self) -> Option<u64> {
        self.per_vector_mask.then_some(if self.wide { 0x10 } else { 0x0C })
    }

    /// Message Control with the function enabled and Multiple Message Enable
    /// back to zero — one vector, the one just written. A function left at the
    /// count it advertises as *capable* raises consecutive vectors this kernel
    /// has no IDT gate for.
    pub fn enabled(message_control: u16) -> u16 {
        (message_control & !MULTI_MESSAGE_ENABLE) | ENABLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole reason this is a type: every register below Message Control
    /// sits four bytes later on a function that takes a 64-bit address, and
    /// no two of them may ever collide.
    #[test]
    fn the_layout_moves_with_the_address_width() {
        let narrow = Msi::decode(PER_VECTOR_MASK);
        assert_eq!((narrow.address_lo(), narrow.address_hi()), (0x04, None));
        assert_eq!((narrow.data(), narrow.mask()), (0x08, Some(0x0C)));

        let wide = Msi::decode(ADDRESS_64 | PER_VECTOR_MASK);
        assert_eq!((wide.address_lo(), wide.address_hi()), (0x04, Some(0x08)));
        assert_eq!((wide.data(), wide.mask()), (0x0C, Some(0x10)));
    }

    /// A narrow function's data register is where a wide one's upper address
    /// dword is, so the two must not both be offered.
    #[test]
    fn a_narrow_function_has_no_upper_address_register() {
        let narrow = Msi::decode(0);
        assert_eq!(narrow.address_hi(), None);
        assert_eq!(narrow.data(), 0x08);
    }

    #[test]
    fn a_function_without_per_vector_masking_offers_no_mask_register() {
        assert_eq!(Msi::decode(0).mask(), None);
        assert_eq!(Msi::decode(ADDRESS_64).mask(), None);
    }

    #[test]
    fn multiple_message_capable_is_an_exponent() {
        for (encoded, vectors) in [(0u16, 1u16), (1, 2), (2, 4), (3, 8), (4, 16), (5, 32)] {
            assert_eq!(Msi::decode(encoded << 1).vectors(), vectors);
        }
    }

    /// Multiple Message *Enable* sits three bits above Multiple Message
    /// *Capable*, and reading one for the other would leave a function armed
    /// for as many vectors as it can raise.
    #[test]
    fn the_capable_field_is_not_the_enable_field() {
        assert_eq!(Msi::decode(MULTI_MESSAGE_ENABLE).vectors(), 1);
        assert_eq!(Msi::enabled(MULTI_MESSAGE_ENABLE), ENABLE);
    }

    #[test]
    fn enabling_keeps_what_the_function_said_about_itself() {
        let ctrl = ADDRESS_64 | PER_VECTOR_MASK | (5 << 1);
        assert_eq!(Msi::enabled(ctrl), ctrl | ENABLE);
    }
}

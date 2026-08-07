//! The MSI-X capability structure (PCIe base spec §7.7.2).

use core::fmt;

/// This capability's id in a function's capability list.
pub const CAP_ID: u8 = 0x11;

/// Byte offsets of the capability's registers, from the capability header.
pub const MESSAGE_CONTROL: u64 = 0x02;
pub const TABLE: u64 = 0x04;

const TABLE_SIZE: u16 = 0x7FF;
const FUNCTION_MASK: u16 = 1 << 14;
const ENABLE: u16 = 1 << 15;

const BIR: u32 = 0x7;
/// The highest Base Address Register a Type 0 header has. 6 and 7 are the
/// reserved BIR encodings, and the reason this crate exists: a function
/// naming one sends a caller that indexes without checking into the CardBus
/// CIS pointer at config offset 0x28, which decodes to no BAR at all and
/// whose value would be mapped and written through.
const MAX_BIR: u8 = 5;

/// One table entry: four dwords (§7.7.2.3).
pub const ENTRY_BYTES: u64 = 16;
pub const ENTRY_ADDRESS_LO: u64 = 0x0;
pub const ENTRY_ADDRESS_HI: u64 = 0x4;
pub const ENTRY_DATA: u64 = 0x8;
pub const ENTRY_VECTOR_CONTROL: u64 = 0xC;
/// Vector Control's bit 0 is the per-entry mask, and it comes out of reset
/// set: an entry programmed and left alone delivers nothing.
pub const ENTRY_UNMASKED: u32 = 0;

/// Why this function's MSI-X cannot be armed. Not a failure of the kernel: a
/// device that publishes one of these is a device whose interrupts the driver
/// has to make its own decision about, and the decision differs — an xHC falls
/// back to MSI, a virtio device has nothing to fall back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unusable {
    ReservedBir(u8),
    /// Firmware assigned the named BAR no address. Its table would be at a
    /// physical address the kernel picked out of nothing.
    BarUnassigned(u8),
    /// The BAR's base plus the table's offset does not fit an address.
    OutOfRange { base: u64, offset: u32 },
}

impl fmt::Display for Unusable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedBir(bir) => {
                write!(f, "its table names BAR {bir}, a reserved indicator")
            }
            Self::BarUnassigned(bir) => {
                write!(f, "BAR {bir} carries its table and firmware assigned it no address")
            }
            Self::OutOfRange { base, offset } => {
                write!(f, "its table is {offset:#x} into a BAR at {base:#x}, past the address space")
            }
        }
    }
}

/// The MSI-X capability's two configuration registers, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Msix {
    entries: u16,
    bir: u8,
    offset: u32,
}

impl Msix {
    pub fn decode(message_control: u16, table: u32) -> Result<Self, Unusable> {
        let bir = (table & BIR) as u8;
        if bir > MAX_BIR {
            return Err(Unusable::ReservedBir(bir));
        }
        Ok(Self {
            // Table Size is encoded one less than it is, so a function that
            // has this capability at all has at least one entry and entry 0
            // needs no bound. Decoded for what it says about the device and
            // not as a check.
            entries: (message_control & TABLE_SIZE) + 1,
            bir,
            offset: table & !BIR,
        })
    }

    pub fn entries(&self) -> u16 {
        self.entries
    }

    /// Which Base Address Register carries the table.
    pub fn bir(&self) -> u8 {
        self.bir
    }

    /// Where entry 0 sits, given the base address the named BAR holds.
    pub fn table_address(&self, bar_base: u64) -> Result<u64, Unusable> {
        if bar_base == 0 {
            return Err(Unusable::BarUnassigned(self.bir));
        }
        bar_base
            .checked_add(self.offset as u64)
            .ok_or(Unusable::OutOfRange { base: bar_base, offset: self.offset })
    }

    /// Message Control with the function's messages enabled and the
    /// function-wide mask cleared.
    pub fn enabled(message_control: u16) -> u16 {
        (message_control | ENABLE) & !FUNCTION_MASK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_size_is_encoded_one_less_than_it_is() {
        assert_eq!(Msix::decode(0x0000, 0).unwrap().entries(), 1);
        assert_eq!(Msix::decode(0x0007, 0).unwrap().entries(), 8);
        assert_eq!(Msix::decode(0x07FF, 0).unwrap().entries(), 2048);
    }

    /// Enable and Function Mask sit inside Message Control alongside Table
    /// Size, so a decoder that read the whole register as a size would report
    /// 2048 entries for every enabled function.
    #[test]
    fn the_control_bits_are_not_part_of_the_size() {
        assert_eq!(Msix::decode(ENABLE | FUNCTION_MASK, 0).unwrap().entries(), 1);
    }

    #[test]
    fn bir_and_offset_share_one_register() {
        let msix = Msix::decode(0, 0x0000_2003).unwrap();
        assert_eq!(msix.bir(), 3);
        assert_eq!(msix.table_address(0xFEBD_0000), Ok(0xFEBD_2000));
    }

    /// The lower three bits are the indicator and never part of the offset,
    /// so a BIR of 5 must not put the table 5 bytes further into the BAR.
    #[test]
    fn the_indicator_is_not_added_to_the_offset() {
        let msix = Msix::decode(0, 0x0000_0005).unwrap();
        assert_eq!(msix.table_address(0x1_0000), Ok(0x1_0000));
    }

    #[test]
    fn a_reserved_indicator_is_refused_rather_than_indexed() {
        for bir in [6u32, 7] {
            assert_eq!(Msix::decode(0, bir), Err(Unusable::ReservedBir(bir as u8)));
        }
        for bir in 0u32..=5 {
            assert_eq!(Msix::decode(0, bir).unwrap().bir(), bir as u8);
        }
    }

    #[test]
    fn an_unassigned_bar_is_refused_rather_than_written_at_its_offset() {
        let msix = Msix::decode(0, 0x0000_3002).unwrap();
        assert_eq!(msix.table_address(0), Err(Unusable::BarUnassigned(2)));
    }

    #[test]
    fn a_table_past_the_address_space_is_refused_rather_than_wrapped() {
        let msix = Msix::decode(0, 0xFFFF_FFF8).unwrap();
        let base = u64::MAX - 0x100;
        assert_eq!(
            msix.table_address(base),
            Err(Unusable::OutOfRange { base, offset: 0xFFFF_FFF8 })
        );
    }

    #[test]
    fn enabling_clears_the_function_wide_mask_and_keeps_the_size() {
        let ctrl = FUNCTION_MASK | 0x0003;
        assert_eq!(Msix::enabled(ctrl), ENABLE | 0x0003);
        assert_eq!(Msix::enabled(0x0003), ENABLE | 0x0003);
    }

    #[test]
    fn an_entry_is_four_dwords_in_the_order_the_spec_gives_them() {
        assert_eq!(
            [ENTRY_ADDRESS_LO, ENTRY_ADDRESS_HI, ENTRY_DATA, ENTRY_VECTOR_CONTROL],
            [0x0, 0x4, 0x8, 0xC]
        );
        assert_eq!(ENTRY_VECTOR_CONTROL + 4, ENTRY_BYTES);
    }
}

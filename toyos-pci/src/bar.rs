//! A Base Address Register, decoded (PCIe base spec §7.5.1.2.1).
//!
//! A BAR's low dword says three things before it says an address: whether the
//! register describes memory at all, how wide the address is, and whether it is
//! prefetchable. The kernel's decoder read the second without the first — it
//! took bits 2:1 as the Memory Space BAR's Type field with no look at bit 0,
//! which is the bit that says the register is a Memory Space BAR.
//!
//! On an I/O BAR bit 0 is set, bit 1 is reserved, and **bits 31:2 are the port
//! number**, so bits 2:1 read as `(0, address bit 2)`. A port whose bit 2 is set
//! therefore decoded as `0b10` — the 64-bit encoding — and the decoder read the
//! *next* register as the upper half of a physical address, which for BAR 5 is
//! the CardBus CIS pointer and for the others is another BAR entirely. With
//! bit 2 clear the same function returned the port number with its low nibble
//! masked off, as a physical address to map. There was no encoding of an I/O
//! BAR it refused.
//!
//! Which is [`msix`](super::msix)'s reason restated one register lower down:
//! these bits are the device's, and a field that says "this is not memory" is
//! not a field to decode past.
//!
//! **Why this is a decode and not a `toyos_untrusted::Untrusted`.** That type
//! is for a number used as an *index* or a *length*, where the fix is a
//! comparison with a bound. Nothing here is indexed and nothing is sized: the
//! defect was reading the register as the wrong *kind* of thing, and the fix is
//! a type that has no address in it until the encoding said there was one.
//! [`Width`] is that type — a caller cannot reach an address without having
//! been handed a [`Memory`], and it cannot be handed one for an I/O BAR.
//!
//! **What this does not bound is the address.** A BAR that says memory names a
//! physical address the kernel then maps, and every non-zero value is a
//! possible one. Bounding what a device may be *pointed at* is the IOMMU's
//! question (`issues/kernel/the-iommu-stops-at-translation.md`), not this
//! decoder's.

use core::fmt;

/// Byte offset of BAR 0 in a Type 0 configuration header.
pub const BASE: u64 = 0x10;

/// The highest Base Address Register a Type 0 header has.
pub const MAX_INDEX: u8 = 5;

const IO_SPACE: u32 = 1 << 0;
const TYPE: u32 = 0b110;
const TYPE_32: u32 = 0b000;
const TYPE_64: u32 = 0b100;
const PREFETCHABLE: u32 = 1 << 3;
/// Bits 31:4 of a Memory Space BAR are the address; 3:0 are the flags above.
const MEMORY_ADDRESS: u32 = !0xF;
/// Bits 31:2 of an I/O Space BAR are the port number.
const IO_PORT: u32 = !0x3;

/// Why this register describes no memory the kernel can map.
///
/// Not a failure of the kernel and not always a broken device: a function may
/// legitimately publish an I/O BAR, and what to *do* about one is the driver's
/// decision — an xHC has none to fall back on, a virtio capability naming one
/// is a capability to skip. What is not a decision is decoding past it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unusable {
    /// Bit 0 is set: this register is an I/O Space BAR and bits 31:2 are a
    /// port number. Carries the port, so a log line can say what the device
    /// published rather than only that something was wrong.
    IoSpace { port: u32 },
    /// The Type field is `0b01` or `0b11`. `0b01` was "locate below 1 MiB" and
    /// is reserved since PCI 3.0; `0b11` has never meant anything. Either is a
    /// device describing an address space this kernel does not have.
    ReservedType(u8),
    /// Firmware assigned this BAR no address. Its registers would be mapped at
    /// physical zero.
    Unassigned,
}

impl fmt::Display for Unusable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IoSpace { port } => {
                write!(f, "it is an I/O BAR at port {port:#x}, not memory")
            }
            Self::ReservedType(ty) => {
                write!(f, "its Type field is {ty:#04b}, a reserved encoding")
            }
            Self::Unassigned => write!(f, "firmware assigned it no address"),
        }
    }
}

/// A Memory Space BAR whose upper half is in the register after it.
///
/// Separate from [`Memory`] so a caller reads that register only when the
/// device said there is one there — which is the whole of the original defect,
/// since BAR 5's neighbour is the CardBus CIS pointer and decodes to no BAR at
/// all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wide {
    low: u32,
    prefetchable: bool,
}

impl Wide {
    /// This BAR's address, given the dword after it.
    pub fn with_high(self, high: u32) -> Result<Memory, Unusable> {
        Memory::new(
            ((high as u64) << 32) | (self.low & MEMORY_ADDRESS) as u64,
            self.prefetchable,
        )
    }
}

/// A physical address a Memory Space BAR names, and firmware assigned.
///
/// There is no constructor that produces one out of an I/O BAR, a reserved
/// Type encoding or an unassigned register, which is the property: a driver
/// holding one of these is holding an address because the encoding said so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    address: u64,
    prefetchable: bool,
}

impl Memory {
    fn new(address: u64, prefetchable: bool) -> Result<Self, Unusable> {
        if address == 0 {
            return Err(Unusable::Unassigned);
        }
        Ok(Self { address, prefetchable })
    }

    pub fn address(self) -> u64 {
        self.address
    }

    pub fn prefetchable(self) -> bool {
        self.prefetchable
    }
}

/// How wide this BAR's address is, which decides whether the next register is
/// part of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    /// All 32 bits of the address are in this register.
    Narrow(Memory),
    /// The upper 32 bits are in the register after this one.
    Wide(Wide),
}

/// Decode a BAR's low dword.
///
/// Bit 0 first, which is the whole point: a register that does not describe
/// memory has no Type field to read, and the bits where one would be are part
/// of a port number.
pub fn decode(low: u32) -> Result<Width, Unusable> {
    if low & IO_SPACE != 0 {
        return Err(Unusable::IoSpace { port: low & IO_PORT });
    }
    let prefetchable = low & PREFETCHABLE != 0;
    match low & TYPE {
        TYPE_32 => Memory::new((low & MEMORY_ADDRESS) as u64, prefetchable).map(Width::Narrow),
        TYPE_64 => Ok(Width::Wide(Wide { low, prefetchable })),
        _ => Err(Unusable::ReservedType(((low & TYPE) >> 1) as u8)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn narrow(low: u32) -> u64 {
        match decode(low) {
            Ok(Width::Narrow(memory)) => memory.address(),
            other => panic!("wanted a 32-bit memory BAR, got {other:?}"),
        }
    }

    fn wide(low: u32, high: u32) -> u64 {
        match decode(low) {
            Ok(Width::Wide(w)) => w.with_high(high).unwrap().address(),
            other => panic!("wanted a 64-bit memory BAR, got {other:?}"),
        }
    }

    #[test]
    fn a_32_bit_memory_bar_is_its_own_whole_address() {
        assert_eq!(narrow(0xFEBD_0000), 0xFEBD_0000);
        // The low nibble is the four flags and never part of the address.
        assert_eq!(narrow(0xFEBD_0008), 0xFEBD_0000);
    }

    #[test]
    fn a_64_bit_memory_bar_takes_the_register_after_it() {
        assert_eq!(wide(0xFEBD_0004, 0x0000_0001), 0x1_FEBD_0000);
        assert_eq!(wide(0x0000_000C, 0xFFFF_FFFF), 0xFFFF_FFFF_0000_0000);
    }

    /// The filed defect. An I/O BAR sets bit 0, and bits 2:1 are then address
    /// bits — so a port with bit 2 set reads as Type `0b10`, the 64-bit
    /// encoding, and a decoder that skipped bit 0 read the *next* register as
    /// the upper half of a physical address.
    #[test]
    fn an_io_bar_whose_bit_2_is_set_is_not_a_64_bit_memory_bar() {
        assert_eq!(decode(0x0000_C005), Err(Unusable::IoSpace { port: 0xC004 }));
    }

    /// The quieter half of the same defect: with bit 2 clear it decoded as a
    /// 32-bit memory BAR and returned the port number, low nibble masked off,
    /// as a physical address to map.
    #[test]
    fn an_io_bar_whose_bit_2_is_clear_is_not_a_32_bit_memory_bar() {
        assert_eq!(decode(0x0000_C001), Err(Unusable::IoSpace { port: 0xC000 }));
    }

    /// Every encoding of the bits above bit 0, so "there is no I/O BAR this
    /// function refuses" cannot come back as "there is one it does not".
    #[test]
    fn every_io_bar_is_refused_whatever_the_bits_above_it_say() {
        for high_bits in 0u32..16 {
            let low = (high_bits << 1) | IO_SPACE;
            assert!(
                matches!(decode(low), Err(Unusable::IoSpace { .. })),
                "{low:#x} decoded as memory",
            );
        }
    }

    #[test]
    fn the_reserved_type_encodings_are_refused_by_name() {
        assert_eq!(decode(0xFEBD_0002), Err(Unusable::ReservedType(0b01)));
        assert_eq!(decode(0xFEBD_0006), Err(Unusable::ReservedType(0b11)));
    }

    /// Firmware that assigned nothing leaves the register zero, and mapping
    /// that would put a device's registers at physical 0. Both widths, because
    /// a 64-bit BAR's zero is spread over two registers and the check has to be
    /// after they are joined.
    #[test]
    fn an_unassigned_bar_is_refused_rather_than_mapped_at_zero() {
        assert_eq!(decode(0), Err(Unusable::Unassigned));
        let Ok(Width::Wide(w)) = decode(0x0000_0004) else { unreachable!() };
        assert_eq!(w.with_high(0), Err(Unusable::Unassigned));
        // ...and a 64-bit BAR whose *low* half is zero is still an address.
        assert_eq!(wide(0x0000_0004, 0x0000_0001), 0x1_0000_0000);
    }

    #[test]
    fn prefetchable_is_read_and_is_not_part_of_the_address() {
        let Ok(Width::Narrow(memory)) = decode(0xFEBD_0008) else { unreachable!() };
        assert!(memory.prefetchable());
        assert_eq!(memory.address(), 0xFEBD_0000);

        let Ok(Width::Narrow(memory)) = decode(0xFEBD_0000) else { unreachable!() };
        assert!(!memory.prefetchable());
    }

    /// A 64-bit BAR is two registers, so the caller must be told to read the
    /// second — the type says so by having no address in it until it is given
    /// one.
    #[test]
    fn a_wide_bar_has_no_address_until_the_high_half_arrives() {
        assert!(matches!(decode(0xFEBD_0004), Ok(Width::Wide(_))));
    }

    /// The BARs of every device this kernel drives, as QEMU publishes them:
    /// the decode has to keep answering for the ordinary case, which is what
    /// the boot itself then exercises.
    #[test]
    fn the_shapes_real_controllers_publish_still_decode() {
        // virtio-pci: 64-bit, prefetchable, in the 0xFE range.
        assert_eq!(wide(0xFE00_000C, 0), 0xFE00_0000);
        // NVMe and xHCI: 64-bit, non-prefetchable.
        assert_eq!(wide(0xFEBD_0004, 0), 0xFEBD_0000);
        // A 32-bit BAR, which plenty of functions still publish.
        assert_eq!(narrow(0xFEB8_0000), 0xFEB8_0000);
    }
}

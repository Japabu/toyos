//! Which USB protocol each root-hub port register speaks (xHCI 1.2 §7.2).
//!
//! A controller publishes one Supported Protocol capability per protocol, each
//! naming a contiguous run of port registers. Without it a driver cannot tell
//! the USB3 view of a receptacle from the USB2 view, and treats a trained
//! SuperSpeed link like a USB2 port that needs resetting into existence.
//!
//! **This is firmware's data, so it is untrusted.** Every field is checked and
//! a capability that makes no sense is refused by name; the ports it claimed
//! stay unknown, and an unknown port is driven the conservative USB2 way.

/// What a run of port registers speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    /// USB 2.0 and earlier. A device is enabled by resetting the port.
    Usb2,
    /// USB 3.x. The link trains itself and the port is Enabled when it is up,
    /// so a reset is a thing to do when it is *not*, never a step on the way in.
    Usb3,
}

/// Why a Supported Protocol capability was not usable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bad {
    /// The capability id is not 2.
    NotAProtocol(u8),
    /// The name string is not "USB ", so whatever this describes is not the
    /// thing §7.2 describes.
    NotUsb(u32),
    /// Compatible Port Offset is 1-based, so zero names no port.
    ZeroPortOffset,
    /// It claims no ports.
    NoPorts,
    /// Its ports run past the ones the controller says it has.
    PastTheLastPort { first: u16, count: u16, max: u8 },
    /// A major revision this driver has no behaviour for. Not a defect in the
    /// capability — a machine to describe rather than guess about.
    UnknownRevision(u8),
}

/// One capability, decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SupportedProtocol {
    pub protocol: Protocol,
    pub major: u8,
    pub minor: u8,
    /// The first port register this covers, zero-based as the driver indexes
    /// them — the capability states it 1-based.
    pub first_port: u8,
    pub port_count: u8,
}

/// The name string §7.2 requires, read as a little-endian dword: "USB ".
const NAME_USB: u32 = u32::from_le_bytes(*b"USB ");

impl SupportedProtocol {
    /// Decode a capability from its first three dwords, against a controller
    /// that says it has `max_ports` port registers.
    pub fn decode(dw0: u32, dw1: u32, dw2: u32, max_ports: u8) -> Result<Self, Bad> {
        let id = dw0 as u8;
        if id != 2 {
            return Err(Bad::NotAProtocol(id));
        }
        if dw1 != NAME_USB {
            return Err(Bad::NotUsb(dw1));
        }
        let major = (dw0 >> 24) as u8;
        let minor = (dw0 >> 16) as u8;
        let first = (dw2 & 0xFF) as u16;
        let count = ((dw2 >> 8) & 0xFF) as u16;
        if first == 0 {
            return Err(Bad::ZeroPortOffset);
        }
        if count == 0 {
            return Err(Bad::NoPorts);
        }
        // Widened before adding, so a capability claiming ports 255..255+255
        // is refused rather than wrapping into a range that looks reasonable.
        if first - 1 + count > max_ports as u16 {
            return Err(Bad::PastTheLastPort { first, count, max: max_ports });
        }
        let protocol = match major {
            2 => Protocol::Usb2,
            3 => Protocol::Usb3,
            other => return Err(Bad::UnknownRevision(other)),
        };
        Ok(Self {
            protocol,
            major,
            minor,
            first_port: (first - 1) as u8,
            port_count: count as u8,
        })
    }

    /// The port registers this covers, as the driver indexes them.
    pub fn ports(&self) -> core::ops::Range<u8> {
        self.first_port..self.first_port + self.port_count
    }
}

/// What every port register on one controller speaks.
///
/// A port no capability claimed stays unknown, and unknown is not a default
/// that guesses: it is driven the USB2 way, which is what the driver did for
/// every port before it could read this at all.
#[derive(Clone, Copy)]
pub struct Protocols {
    usb3: [u64; 4],
    known: [u64; 4],
}

impl Default for Protocols {
    fn default() -> Self {
        Self::UNKNOWN
    }
}

impl Protocols {
    pub const UNKNOWN: Self = Self { usb3: [0; 4], known: [0; 4] };

    /// Record what one capability claimed. A port claimed twice keeps the first
    /// claim: two capabilities covering one register is a controller
    /// contradicting itself, and the later word is no better than the earlier.
    pub fn record(&mut self, found: &SupportedProtocol) {
        for port in found.ports() {
            let (word, bit) = (port as usize / 64, 1u64 << (port % 64));
            if self.known[word] & bit != 0 {
                continue;
            }
            self.known[word] |= bit;
            if found.protocol == Protocol::Usb3 {
                self.usb3[word] |= bit;
            }
        }
    }

    /// What this port speaks, or `None` where no capability said.
    pub fn of(&self, port_idx: u8) -> Option<Protocol> {
        let (word, bit) = (port_idx as usize / 64, 1u64 << (port_idx % 64));
        if self.known[word] & bit == 0 {
            return None;
        }
        Some(if self.usb3[word] & bit != 0 { Protocol::Usb3 } else { Protocol::Usb2 })
    }

    /// How many ports each protocol claimed, for the line that reports what the
    /// controller said about itself.
    pub fn counts(&self, max_ports: u8) -> (u32, u32) {
        let mut usb2 = 0;
        let mut usb3 = 0;
        for port in 0..max_ports {
            match self.of(port) {
                Some(Protocol::Usb2) => usb2 += 1,
                Some(Protocol::Usb3) => usb3 += 1,
                None => {}
            }
        }
        (usb2, usb3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability a Tiger Lake PCH publishes for its USB2 ports, byte for
    /// byte in the layout §7.2 gives: id 2, next 4, minor 0, major 2, "USB ",
    /// ports 5..=12.
    const USB2_CAP: (u32, u32, u32) = (0x0200_0402, NAME_USB, 0x0000_0805);
    /// And for its SuperSpeed ports: major 3, minor 1, ports 1..=4.
    const USB3_CAP: (u32, u32, u32) = (0x0310_0402, NAME_USB, 0x0000_0401);

    #[test]
    fn a_conformant_pair_covers_the_machine() {
        let usb2 = SupportedProtocol::decode(USB2_CAP.0, USB2_CAP.1, USB2_CAP.2, 12).unwrap();
        assert_eq!(usb2.protocol, Protocol::Usb2);
        assert_eq!(usb2.major, 2);
        assert_eq!(usb2.ports(), 4..12, "ports 5..=12 are indices 4..12");

        let usb3 = SupportedProtocol::decode(USB3_CAP.0, USB3_CAP.1, USB3_CAP.2, 12).unwrap();
        assert_eq!(usb3.protocol, Protocol::Usb3);
        assert_eq!((usb3.major, usb3.minor), (3, 0x10));
        assert_eq!(usb3.ports(), 0..4);

        let mut map = Protocols::UNKNOWN;
        map.record(&usb2);
        map.record(&usb3);
        for port in 0..4 {
            assert_eq!(map.of(port), Some(Protocol::Usb3), "port {port}");
        }
        for port in 4..12 {
            assert_eq!(map.of(port), Some(Protocol::Usb2), "port {port}");
        }
        assert_eq!(map.of(12), None, "a port no capability claimed stays unknown");
        assert_eq!(map.counts(12), (8, 4));
    }

    /// Every field a controller chooses, at the value that would be a defect.
    #[test]
    fn firmware_that_makes_no_sense_is_refused_by_name() {
        let cases: [(u32, u32, u32, u8, Bad); 6] = [
            (0x0200_0401, NAME_USB, 0x0000_0805, 12, Bad::NotAProtocol(1)),
            (USB2_CAP.0, 0xDEAD_BEEF, USB2_CAP.2, 12, Bad::NotUsb(0xDEAD_BEEF)),
            (USB2_CAP.0, NAME_USB, 0x0000_0800, 12, Bad::ZeroPortOffset),
            (USB2_CAP.0, NAME_USB, 0x0000_0005, 12, Bad::NoPorts),
            (
                USB2_CAP.0,
                NAME_USB,
                0x0000_FFFF,
                12,
                Bad::PastTheLastPort { first: 255, count: 255, max: 12 },
            ),
            (0x0900_0402, NAME_USB, 0x0000_0805, 12, Bad::UnknownRevision(9)),
        ];
        for (dw0, dw1, dw2, max, want) in cases {
            assert_eq!(
                SupportedProtocol::decode(dw0, dw1, dw2, max),
                Err(want),
                "dw0={dw0:#010x} dw2={dw2:#010x}"
            );
        }
    }

    /// The claim that would have wrapped: the last port and a count that runs
    /// off the end of a byte.
    #[test]
    fn a_range_that_would_wrap_is_refused_rather_than_folded() {
        assert!(SupportedProtocol::decode(USB2_CAP.0, NAME_USB, 0x0000_02FF, 255).is_err());
        // And the exact fit is accepted, so the bound is not off by one.
        let last = SupportedProtocol::decode(USB2_CAP.0, NAME_USB, 0x0000_01FF, 255).unwrap();
        assert_eq!(last.ports(), 254..255);
    }

    /// Two capabilities claiming one port is a controller contradicting itself.
    /// The first claim stands, and the second cannot silently retype a port the
    /// driver may already have acted on.
    #[test]
    fn a_port_claimed_twice_keeps_the_first_claim() {
        let mut map = Protocols::UNKNOWN;
        map.record(&SupportedProtocol::decode(USB3_CAP.0, USB3_CAP.1, USB3_CAP.2, 12).unwrap());
        map.record(&SupportedProtocol::decode(USB2_CAP.0, NAME_USB, 0x0000_0C01, 12).unwrap());
        assert_eq!(map.of(0), Some(Protocol::Usb3));
        assert_eq!(map.of(4), Some(Protocol::Usb2), "the part it claimed alone still lands");
    }
}

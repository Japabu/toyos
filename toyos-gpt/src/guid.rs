use core::fmt;

/// A GUID exactly as it sits on the disk and in a UEFI device path node.
///
/// Bytes, not fields. The mixed-endian layout (three little-endian integers
/// then eight raw bytes) is the one thing about GUIDs that everybody gets
/// wrong once, and the only operations this crate needs are equality and
/// printing — neither of which has to know where the field boundaries are.
/// Firmware hands us these sixteen bytes and the disk holds those same sixteen
/// bytes, so the comparison that decides which partition is ours is a
/// `[u8; 16]` comparison with no conversion in the middle to get backwards.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    /// The all-zero GUID, which GPT gives the meaning "this entry is unused".
    pub const ZERO: Self = Self([0; 16]);

    /// `C12A7328-F81F-11D2-BA4B-00A0C93EC93B` — the EFI System Partition type.
    ///
    /// A *type*, shared by every ESP that has ever existed, so it can say
    /// "this looks like the right kind of thing" and can never say "this one
    /// is mine". Selecting on it is the defect this whole module exists to
    /// make impossible.
    pub const EFI_SYSTEM: Self = Self::from_fields(
        0xC12A_7328,
        0xF81F,
        0x11D2,
        [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B],
    );

    pub const fn from_fields(a: u32, b: u16, c: u16, d: [u8; 8]) -> Self {
        let a = a.to_le_bytes();
        let b = b.to_le_bytes();
        let c = c.to_le_bytes();
        Self([
            a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5],
            d[6], d[7],
        ])
    }

    pub const fn is_zero(&self) -> bool {
        let mut i = 0;
        while i < 16 {
            if self.0[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-\
             {:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12],
            b[13], b[14], b[15]
        )
    }
}

impl fmt::Debug for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ESP type GUID's canonical text and its on-disk bytes, from the UEFI
    /// specification. Both directions, because a mixed-endian mistake that is
    /// self-consistent would survive either one alone.
    #[test]
    fn esp_type_guid_round_trips() {
        const ON_DISK: [u8; 16] = [
            0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E,
            0xC9, 0x3B,
        ];
        assert_eq!(Guid::EFI_SYSTEM.0, ON_DISK);

        let text = heapless_format(Guid(ON_DISK));
        assert_eq!(&text[..], b"C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
    }

    #[test]
    fn zero_is_zero() {
        assert!(Guid::ZERO.is_zero());
        assert!(!Guid::EFI_SYSTEM.is_zero());
        let mut almost = [0u8; 16];
        almost[15] = 1;
        assert!(!Guid(almost).is_zero());
    }

    fn heapless_format(g: Guid) -> [u8; 36] {
        use core::fmt::Write;
        struct Sink([u8; 36], usize);
        impl Write for Sink {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                for &b in s.as_bytes() {
                    self.0[self.1] = b;
                    self.1 += 1;
                }
                Ok(())
            }
        }
        let mut sink = Sink([0; 36], 0);
        write!(sink, "{g}").unwrap();
        assert_eq!(sink.1, 36);
        sink.0
    }
}

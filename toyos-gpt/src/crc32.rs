//! CRC-32, the zlib/Ethernet one — **not** the CRC-32c in `bcachefs`.
//!
//! Reflected polynomial 0xEDB88320 (0x04C11DB7 unreflected). bcachefs uses
//! Castagnoli, 0x82F63B78 reflected, and the two produce different values for
//! every input longer than nothing; a GPT checked with the wrong one rejects
//! every table on earth. The two implementations look identical apart from one
//! constant, which is exactly why this file says so.

const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
};

/// A CRC-32 taken over pieces. The partition entry array is read a block at a
/// time and never held whole, and the header's own CRC is taken over the
/// header with four bytes of itself zeroed — neither is one contiguous slice.
#[derive(Clone, Copy)]
pub struct Crc32(u32);

impl Crc32 {
    pub const fn new() -> Self {
        Self(0xFFFF_FFFF)
    }

    pub fn update(&mut self, data: &[u8]) {
        for &byte in data {
            let idx = ((self.0 ^ byte as u32) & 0xFF) as usize;
            self.0 = (self.0 >> 8) ^ TABLE[idx];
        }
    }

    pub const fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(data);
    crc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The check value the CRC catalogue publishes for CRC-32/ISO-HDLC. An
    /// external oracle is the point: it is what says this is the polynomial
    /// GPT uses and not the one three files away in `bcachefs`.
    #[test]
    fn catalogue_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn not_castagnoli() {
        assert_ne!(crc32(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn streaming_matches_one_shot() {
        let data: [u8; 300] = core::array::from_fn(|i| (i * 7) as u8);
        let mut piecewise = Crc32::new();
        for chunk in data.chunks(37) {
            piecewise.update(chunk);
        }
        assert_eq!(piecewise.finish(), crc32(&data));
    }
}

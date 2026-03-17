/// Software CRC-32c (Castagnoli) implementation.
///
/// Uses the polynomial 0x1EDC6F41. This is the same CRC used by iSCSI, ext4,
/// btrfs, and bcachefs. On x86 with SSE4.2, the hardware `crc32` instruction
/// computes this same polynomial.

const CRC32C_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0x82F63B78;
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

/// Compute CRC-32c over a byte slice.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let idx = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32C_TABLE[idx];
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // CRC-32c of empty input
        assert_eq!(crc32c(b""), 0x00000000);
        // CRC-32c of "123456789"
        assert_eq!(crc32c(b"123456789"), 0xE3069283);
    }

    #[test]
    fn zeroes() {
        // CRC of all zeroes should be deterministic
        let zeros = [0u8; 4096];
        let c = crc32c(&zeros);
        assert_eq!(c, crc32c(&zeros));
        assert_ne!(c, 0);
    }
}

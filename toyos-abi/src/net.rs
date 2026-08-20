/// NIC device info returned when claiming the NIC device.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NicInfo {
    /// The DMA region (an entire 2 MiB page), installed by the read that
    /// answers this description.
    pub dma: crate::RawHandle,
    /// Byte offset of the RX buffer region within the DMA page.
    pub rx_buf_offset: u32,
    /// Byte offset of the TX buffer within the DMA page.
    pub tx_buf_offset: u32,
    pub mac: [u8; 6],
    pub rx_buf_count: u16,
    pub rx_buf_size: u16,
    pub net_hdr_size: u16,
}

/// Every byte belongs to a field: this crosses the boundary through
/// `as_bytes`, so a gap would publish whatever the kernel stack held.
const _: () = assert!(core::mem::size_of::<NicInfo>() == 4 + 4 + 4 + 6 + 2 + 2 + 2);

impl NicInfo {
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `self` is a valid `&Self` (non-null, aligned, readable for
        // `size_of::<Self>()` bytes), and the const assert above proves the
        // `repr(C)` layout has no padding, so every byte the slice exposes is
        // an initialized field, not a gap.
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }
}

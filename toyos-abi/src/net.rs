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

impl NicInfo {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }
}

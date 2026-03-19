/// NIC device info returned when claiming the NIC device.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NicInfo {
    /// Shared memory token for the contiguous RX buffer region.
    pub rx_buf_token: u32,
    /// Shared memory token for the TX buffer (single page).
    pub tx_buf_token: u32,
    pub mac: [u8; 6],
    /// Number of RX buffers.
    pub rx_buf_count: u16,
    /// Size of each RX buffer in bytes.
    pub rx_buf_size: u16,
    /// Size of the virtio net header prepended to each frame.
    pub net_hdr_size: u16,
}

impl NicInfo {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }
}

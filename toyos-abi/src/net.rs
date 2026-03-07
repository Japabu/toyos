/// NIC device info returned when claiming the NIC device.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NicInfo {
    pub rx_buf_tokens: [u32; 3],
    pub tx_buf_token: u32,
    pub mac: [u8; 6],
    pub rx_buf_count: u8,
    pub net_hdr_size: u8,
}

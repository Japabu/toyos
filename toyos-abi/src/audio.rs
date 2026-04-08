//! Audio device info for the virtio-sound driver.

/// Audio device info returned when claiming the audio device.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioInfo {
    pub dma_token: u32,
    pub buf_offsets: [u32; 5],
    pub num_buffers: u8,
    pub sample_rate: u32,
    pub channels: u8,
    pub period_bytes: u32,
}

impl AudioInfo {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }
}


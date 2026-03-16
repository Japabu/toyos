//! Audio device interface.
//!
//! The audio device follows the same capability pattern as NIC and framebuffer:
//! claim the device via `open_device(Audio)`, read `AudioInfo` from the fd,
//! map DMA buffer tokens as shared memory, then use `audio_submit`/`audio_poll`
//! for non-blocking control.

use crate::syscall;

/// Audio device info returned when claiming the audio device.
/// Each `buf_tokens` entry is a shared memory token for one DMA page (4096 bytes)
/// that can be mapped and written to directly.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioInfo {
    pub buf_tokens: [u32; 5],
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

/// soundd protocol message types.
pub const MSG_AUDIO_OPEN: u32 = 1;
pub const MSG_AUDIO_OPENED: u32 = 2;

/// Request to open an audio stream (client → soundd).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioOpenRequest {
    pub pipe_id: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub format: u16,
}

/// Response after opening an audio stream (soundd → client).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioOpenResponse {
    pub stream_id: u32,
}

/// Submit a filled DMA buffer to the audio device.
pub fn audio_submit(buf_idx: u32, len: u32) {
    syscall::audio_submit(buf_idx, len);
}

/// Poll for completed (reusable) DMA buffers.
/// Returns a bitmask where bit N is set if buffer N is available.
pub fn audio_poll() -> u32 {
    syscall::audio_poll()
}

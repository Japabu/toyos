//! Audio device and soundd protocol.

use crate::syscall;

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

// ---------------------------------------------------------------------------
// soundd IPC protocol
// ---------------------------------------------------------------------------

pub const MSG_AUDIO_OPEN: u32 = 1;
pub const MSG_AUDIO_OPENED: u32 = 2;
pub const MSG_AUDIO_SET_VOLUME: u32 = 3;

/// Client → soundd: request to open an audio stream.
/// soundd allocates a shared memory ring and returns it in the response.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioOpenRequest {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: u16,
}

/// soundd → client: stream opened, here's the shared memory ring.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioOpenResponse {
    pub stream_id: u32,
    pub shm_token: u32,
    pub ring_size: u32,
}

/// Client → soundd: set per-stream volume.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioSetVolume {
    pub stream_id: u32,
    pub volume: u32, // 0-256, where 256 = 1.0
}

// ---------------------------------------------------------------------------
// Audio hardware syscalls
// ---------------------------------------------------------------------------

pub fn audio_submit(buf_idx: u32, len: u32) {
    syscall::audio_submit(buf_idx, len);
}

pub fn audio_poll() -> u32 {
    syscall::audio_poll()
}

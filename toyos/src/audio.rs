//! soundd IPC protocol and audio hardware wrappers.

use toyos_abi::syscall;

pub const MSG_AUDIO_OPEN: u32 = 1;
pub const MSG_AUDIO_OPENED: u32 = 2;
pub const MSG_AUDIO_SET_VOLUME: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioOpenRequest {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioOpenResponse {
    pub stream_id: u32,
    pub shm_token: u32,
    pub ring_size: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioSetVolume {
    pub stream_id: u32,
    pub volume: u32,
}

pub fn audio_submit(buf_idx: u32, len: u32) {
    syscall::audio_submit(buf_idx, len);
}

pub fn audio_poll() -> u32 {
    syscall::audio_poll()
}

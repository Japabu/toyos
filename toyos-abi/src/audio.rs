//! Audio device info and shared memory protocol types.

use core::sync::atomic::AtomicU32;

/// Audio device info returned when claiming the audio device.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioInfo {
    pub dma_token: u32,
    pub buf_offsets: [u32; 8],
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

/// Shared memory header for the client↔soundd slot-ring protocol.
///
/// Client increments `write_idx` after filling a slot; soundd increments
/// `read_idx` after consuming one. Ring is full when `write_idx - read_idx >= slot_count`.
#[repr(C, align(64))]
pub struct AudioSlotHeader {
    pub write_idx: AtomicU32,
    pub read_idx: AtomicU32,
    pub slot_count: u32,
    pub _reserved: u32,
}

impl AudioSlotHeader {
    pub const SIZE: usize = core::mem::size_of::<Self>();
}


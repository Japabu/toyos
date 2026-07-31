//! Audio device info and shared memory protocol types.

use core::sync::atomic::AtomicU32;

/// Audio device info returned when claiming the audio device.
///
/// The padding is spelled out because `as_bytes` hands this whole struct to
/// userland. `repr(C)` puts three bytes after each `u8` to realign the `u32`
/// that follows, and a struct literal never writes them — so six bytes of
/// whatever the kernel stack held at that moment crossed the boundary on every
/// read. Naming them makes them ordinary fields that the initializer has to
/// fill, and the assertion below is what keeps a future field from
/// reintroducing a gap.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioInfo {
    pub dma_token: u32,
    pub buf_offsets: [u32; 8],
    pub num_buffers: u8,
    pub _pad0: [u8; 3],
    pub sample_rate: u32,
    pub channels: u8,
    pub _pad1: [u8; 3],
    pub period_bytes: u32,
}

/// Every byte of `AudioInfo` belongs to a field. If the sum below stops
/// matching `size_of`, the compiler has inserted padding again and `as_bytes`
/// is publishing uninitialised memory.
const _: () = {
    let named = 4 + 32 + 1 + 3 + 4 + 1 + 3 + 4;
    assert!(core::mem::size_of::<AudioInfo>() == named);
};

impl AudioInfo {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }
}

/// One batch of DMA buffer completions, recorded at interrupt time.
///
/// Reads on the audio fd (after the initial `AudioInfo` read) return an
/// array of these: the kernel writes as many pending records as fit in the
/// caller's buffer and returns the byte count. Each record is one completion
/// interrupt: `mask` bit N set means DMA buffer N finished playback, and
/// `timestamp_nanos` is `nanos_since_boot` captured in the interrupt handler
/// — the clock source for soundd's DLL. Records are returned oldest-first.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioCompletionRecord {
    pub mask: u32,
    pub _pad: u32,
    pub timestamp_nanos: u64,
}

impl AudioCompletionRecord {
    pub const SIZE: usize = core::mem::size_of::<Self>();
}

/// Same rule as `AudioInfo`: this one crosses the boundary too, and its `_pad`
/// is the reason it has no gap today.
const _: () = assert!(AudioCompletionRecord::SIZE == 4 + 4 + 8);

/// Shared memory header for the client↔soundd slot-ring protocol.
///
/// Client increments `write_idx` after filling a slot; soundd increments
/// `read_idx` after it has finished mixing from one. Ring is full when
/// `write_idx - read_idx >= slot_count` (slot_count arrives in
/// `MSG_STREAM_OPENED`). The two indices live on separate cache lines:
/// they are written by different processes at audio-period rate.
#[repr(C, align(64))]
pub struct AudioSlotHeader {
    pub write_idx: AtomicU32,
    _pad0: [u32; 15],
    pub read_idx: AtomicU32,
    _pad1: [u32; 15],
}

impl AudioSlotHeader {
    pub const SIZE: usize = core::mem::size_of::<Self>();
}

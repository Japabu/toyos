//! What a completion is, and the shared memory protocol soundd serves clients.

use core::sync::atomic::AtomicU32;

/// One batch of DMA buffer completions, recorded at interrupt time.
///
/// Reads on a sound device's fd (after the initial info read) return an array
/// of these: the kernel writes as many pending records as fit in the caller's
/// buffer and returns the byte count. `mask` bit N set means period N finished
/// playing, and `timestamp_nanos` is `nanos_since_boot` captured in the
/// interrupt handler — the clock source for soundd's DLL, and the reason the
/// mask is derived there rather than by the driver at wake time. Records are
/// returned oldest-first.
///
/// Both stubs produce it, so the two backends differ in nothing a mixer sees.
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

/// Every byte belongs to a field. This crosses the boundary, and `_pad` is why
/// there is no gap for whatever the kernel stack held to travel in.
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

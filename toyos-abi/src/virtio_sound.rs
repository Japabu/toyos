//! What the kernel's virtio-sound stub hands its driver.
//!
//! The line through this device is the one HDA's is: **the kernel writes every
//! address.** A split virtqueue names memory in exactly one place, its
//! descriptor table, and the three tables here live in a page no process maps.
//! What a driver gets is the region those descriptors point into, the two ring
//! indices that select one of them, and one register write to say it has.
//!
//! So the layout below is the whole interface. It is a set of constants rather
//! than fields of [`VirtioSoundInfo`] because both halves have to agree on it
//! for the descriptors to mean anything, and a number the kernel reports is a
//! number a driver could believe instead of the one the descriptors were built
//! from.

/// The pipeline, in periods and bytes.
///
/// The shape the HDA stub presents too, and deliberately: soundd's mix loop,
/// its client ring depth and gate A's recorded counters are all sized against
/// eight periods of 512 bytes, so a second backend that chose differently would
/// be a second instrument as well as a second device.
pub const PERIODS: usize = 8;
pub const PERIOD_BYTES: usize = 512;

/// Virtqueue indices, fixed by the virtio sound device specification.
pub const CONTROL_QUEUE: u16 = 0;
pub const EVENT_QUEUE: u16 = 1;
pub const TX_QUEUE: u16 = 2;

/// Descriptors per queue. Powers of two, as virtio 1.2 §2.7 requires.
///
/// The TX queue holds one three-descriptor chain per period and nothing else,
/// so its slots are exhausted by [`PERIODS`] and never by a driver's pace.
pub const TX_QUEUE_SIZE: u16 = 32;
pub const CONTROL_QUEUE_SIZE: u16 = 16;
pub const EVENT_QUEUE_SIZE: u16 = 16;

/// Descriptors in one TX chain: the transfer header, the PCM period, and the
/// status the device writes back.
pub const TX_CHAIN: u16 = 3;

/// The first descriptor of the chain carrying period `idx`.
///
/// A function and not a search: the chains are built once, at bind, and the
/// driver publishes one by index. It never writes a descriptor, so there is
/// nothing here for it to get wrong beyond naming the wrong period.
pub const fn tx_chain_head(idx: usize) -> u16 {
    idx as u16 * TX_CHAIN
}

/// How many buffers the event queue keeps posted.
pub const EVENT_BUFS: usize = 8;
/// Stride between them. The event structure is eight bytes; the rest is so a
/// buffer index and a descriptor index are the same number.
pub const EVENT_BUF_STRIDE: usize = 16;

/// Stride between transfer headers, for the same reason.
pub const XFER_STRIDE: usize = 16;
/// Stride between the per-period status structures the device writes.
pub const STATUS_STRIDE: usize = 8;

/// How much of a control request or response the descriptors describe.
///
/// One pair of buffers serves every command, so the length is fixed at bind and
/// has to cover the longest of them. The device reads the header first and takes
/// only what that command defines, so a buffer longer than the message is not a
/// message with garbage after it.
pub const CTRL_BUF_BYTES: usize = 128;

/// An avail ring's bytes: flags, index, and one descriptor index per slot.
pub const fn avail_bytes(size: u16) -> usize {
    4 + size as usize * 2
}
/// A used ring's: flags, index, and one `(id, len)` pair per slot.
pub const fn used_bytes(size: u16) -> usize {
    4 + size as usize * 8
}

// Byte offsets into the shared region. Every ring here is one the driver
// publishes to or reads from; the TX used ring is absent because the interrupt
// handler is its only consumer, and the descriptor tables are absent because
// they are the addresses.
pub const OFF_PCM: usize = 0x0000;
pub const OFF_TX_AVAIL: usize = 0x1000;
pub const OFF_TX_XFER: usize = 0x1080;
pub const OFF_TX_STATUS: usize = 0x1100;
pub const OFF_CTRL_AVAIL: usize = 0x1200;
pub const OFF_CTRL_USED: usize = 0x1240;
pub const OFF_CTRL_REQ: usize = 0x1300;
pub const OFF_CTRL_RESP: usize = 0x1400;
pub const OFF_EVENT_AVAIL: usize = 0x1500;
pub const OFF_EVENT_USED: usize = 0x1540;
pub const OFF_EVENT_BUFS: usize = 0x1600;
pub const SHARED_BYTES: usize = 0x2000;

/// Nothing overlaps and everything fits.
///
/// The device writes four of these regions and the driver writes three, at
/// addresses the kernel computed once — so an overlap is not a bug a test would
/// find, it is one period of PCM landing on a used ring.
const _: () = {
    let regions = [
        (OFF_PCM, PERIODS * PERIOD_BYTES),
        (OFF_TX_AVAIL, avail_bytes(TX_QUEUE_SIZE)),
        (OFF_TX_XFER, PERIODS * XFER_STRIDE),
        (OFF_TX_STATUS, PERIODS * STATUS_STRIDE),
        (OFF_CTRL_AVAIL, avail_bytes(CONTROL_QUEUE_SIZE)),
        (OFF_CTRL_USED, used_bytes(CONTROL_QUEUE_SIZE)),
        (OFF_CTRL_REQ, CTRL_BUF_BYTES),
        (OFF_CTRL_RESP, CTRL_BUF_BYTES),
        (OFF_EVENT_AVAIL, avail_bytes(EVENT_QUEUE_SIZE)),
        (OFF_EVENT_USED, used_bytes(EVENT_QUEUE_SIZE)),
        (OFF_EVENT_BUFS, EVENT_BUFS * EVENT_BUF_STRIDE),
    ];
    let mut i = 0;
    while i < regions.len() {
        let (start, len) = regions[i];
        assert!(start + len <= SHARED_BYTES);
        let mut j = i + 1;
        while j < regions.len() {
            let (other, other_len) = regions[j];
            assert!(start + len <= other || other + other_len <= start);
            j += 1;
        }
        i += 1;
    }
    assert!(PERIODS * TX_CHAIN as usize <= TX_QUEUE_SIZE as usize);
    assert!(EVENT_BUFS <= EVENT_QUEUE_SIZE as usize);
};

/// The device the kernel brought up, as the driver needs to see it.
///
/// No physical address and no register window: a shared-memory token, the three
/// offsets inside the notification region that are the driver's whole write
/// surface, and what the device said about itself in its configuration space.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VirtioSoundInfo {
    /// The shared region, mapped writable, laid out by the constants above.
    pub dma: crate::RawHandle,
    /// Byte offsets into the notification region — the offset space
    /// [`device_reg_write`] names for this device, and the only one it has.
    ///
    /// [`device_reg_write`]: crate::syscall::device_reg_write
    pub notify_control: u32,
    pub notify_event: u32,
    pub notify_tx: u32,
    /// The device's configuration space, read once by the kernel, which decided
    /// nothing with it. Which stream to open and at what rate is the driver's.
    pub jacks: u32,
    pub streams: u32,
    pub chmaps: u32,
}

/// Every byte belongs to a field: this crosses the boundary through `as_bytes`,
/// so a gap would publish whatever the kernel stack held.
const _: () = assert!(core::mem::size_of::<VirtioSoundInfo>() == 7 * 4);

impl VirtioSoundInfo {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(
                self as *const Self as *const u8,
                core::mem::size_of::<Self>(),
            )
        }
    }
}

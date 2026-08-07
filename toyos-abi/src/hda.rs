//! What the kernel's HDA stub hands its driver.
//!
//! `specs/hda-driver-plan.md` §4.1 is the design. The line through the device
//! is **who touches a register**: the kernel programs every register whose
//! value is an address or indexes a structure it allocated, and the driver
//! reaches the rest through [`syscall::device_reg_read`] and
//! [`syscall::device_reg_write`], each checked against an allow-list and
//! refused by name. Nothing here names a physical address.
//!
//! Completions come back as [`AudioCompletionRecord`](crate::audio::AudioCompletionRecord),
//! the same record virtio-sound produces, because the mask is derived from a
//! position read in the interrupt handler and the two backends then differ in
//! nothing a mixer can see.
//!
//! [`syscall::device_reg_read`]: crate::syscall::device_reg_read
//! [`syscall::device_reg_write`]: crate::syscall::device_reg_write

/// The controller and stream the kernel brought up, as the driver needs to see
/// them.
///
/// No register window and no physical address: everything here is a shared
/// memory token, a shape the driver has to know to fill the ring, or a number
/// it has to send a codec.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HdaInfo {
    /// The PCM ring, mapped writable. `periods` buffers of `period_bytes` laid
    /// end to end from the start of the region, with the buffer descriptor
    /// list already pointing at them.
    pub pcm_token: u32,
    pub period_bytes: u32,
    /// Byte offset of the output stream descriptor inside the register window,
    /// so the driver names `SDnCTL` and `SDnFMT` by the same arithmetic the
    /// allow-list does.
    pub stream_offset: u32,
    /// Every codec address `STATESTS` reported, one bit per link address. The
    /// driver enumerates all of them and chooses by capability; the kernel
    /// read this register to know the link is alive and decided nothing with
    /// it.
    pub statests: u16,
    /// The stream tag the kernel put in the descriptor. It has to reach the
    /// codec's converter, and sending that verb is the driver's.
    pub stream_tag: u8,
    pub periods: u8,
}

/// Every byte belongs to a field: this crosses the boundary through
/// `as_bytes`, so a gap would publish whatever the kernel stack held.
const _: () = {
    let named = 4 + 4 + 4 + 2 + 1 + 1;
    assert!(core::mem::size_of::<HdaInfo>() == named);
};

impl HdaInfo {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(
                self as *const Self as *const u8,
                core::mem::size_of::<Self>(),
            )
        }
    }
}

/// How wide a register access is.
///
/// Not a convenience: HDA registers are 8, 16 and 32 bits and a 32-bit write to
/// a 16-bit register is a write to its neighbour — `SDnCTL` and `SDnSTS` are
/// adjacent bytes of one dword, and the second is the kernel's alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum RegWidth {
    U8 = 1,
    U16 = 2,
    U32 = 4,
}

impl RegWidth {
    pub fn from_raw(raw: u64) -> Option<Self> {
        match raw {
            1 => Some(Self::U8),
            2 => Some(Self::U16),
            4 => Some(Self::U32),
            _ => None,
        }
    }

    pub const fn bytes(self) -> u64 {
        self as u64
    }

    /// The widest value this access can carry. A caller handing a wider one is
    /// naming bits the register does not have.
    pub const fn max_value(self) -> u32 {
        match self {
            Self::U8 => u8::MAX as u32,
            Self::U16 => u16::MAX as u32,
            Self::U32 => u32::MAX,
        }
    }
}

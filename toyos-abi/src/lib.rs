#![no_std]

pub mod audio;
pub mod boot;
pub mod input;
pub mod io_uring;
pub mod net;
pub mod ring;
pub mod syscall;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fd(pub i32);

/// A process ID. Identifies a process — owns address space, FDs, vruntime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pid(pub u32);

impl Pid {
    pub const MAX: Self = Pid(u32::MAX);
    pub fn raw(self) -> u32 { self.0 }
    pub fn from_raw(v: u32) -> Self { Pid(v) }
}

impl core::fmt::Display for Pid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl core::ops::Add for Pid {
    type Output = Self;
    fn add(self, rhs: Self) -> Self { Pid(self.0 + rhs.0) }
}

/// A thread ID. Identifies a schedulable entity — goes in run queues.
/// Every process has at least one thread (the main thread).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tid(pub u32);

impl Tid {
    pub const MAX: Self = Tid(u32::MAX);
    pub fn raw(self) -> u32 { self.0 }
    pub fn from_raw(v: u32) -> Self { Tid(v) }
}

impl core::fmt::Display for Tid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl core::ops::Add for Tid {
    type Output = Self;
    fn add(self, rhs: Self) -> Self { Tid(self.0 + rhs.0) }
}

/// GPU framebuffer info passed between kernel and userland.
/// Shared definition so both sides agree on the layout.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FramebufferInfo {
    pub token: [u32; 2],
    pub cursor_token: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
    pub flags: u32,
}

impl FramebufferInfo {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }
}

// SAFETY: FramebufferInfo is #[repr(C)] and contains only u32 fields — no padding, no pointers.
unsafe impl Sync for FramebufferInfo {}
unsafe impl Send for FramebufferInfo {}

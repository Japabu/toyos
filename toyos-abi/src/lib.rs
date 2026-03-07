#![no_std]

pub mod message;
pub mod net;
pub mod ring;
pub mod syscall;

/// A file descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fd(pub i32);

/// A process ID.
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

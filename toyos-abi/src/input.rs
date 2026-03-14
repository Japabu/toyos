//! Wire-format types for HID input devices (keyboard, mouse).
//!
//! These cross the kernel→userland boundary via file descriptor reads.

/// Keyboard modifier flags.
pub const MOD_SHIFT: u8 = 1;
pub const MOD_CTRL: u8 = 2;
pub const MOD_ALT: u8 = 4;
pub const MOD_GUI: u8 = 8;
pub const MOD_RELEASED: u8 = 0x10;

/// A keyboard event as delivered by the kernel through the keyboard fd.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawKeyEvent {
    pub keycode: u8,
    pub modifiers: u8,
    pub len: u8,
    pub translated: [u8; 5],
}

impl RawKeyEvent {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }
}

/// A mouse event as delivered by the kernel through the mouse fd.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MouseEvent {
    pub buttons: u8,
    pub dx: i8,
    pub dy: i8,
    pub scroll: i8,
}

impl MouseEvent {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }
}

//! Wire-format types for HID input devices (keyboard, mouse).
//!
//! These cross the kernel→userland boundary as reads on a device handle.

/// Keyboard modifier flags.
pub const MOD_SHIFT: u8 = 1;
pub const MOD_CTRL: u8 = 2;
pub const MOD_ALT: u8 = 4;
pub const MOD_GUI: u8 = 8;
pub const MOD_RELEASED: u8 = 0x10;

/// One key transition, as the kernel delivers it through the keyboard handle.
///
/// The whole of what the kernel knows about a key: which physical key moved,
/// which way, and what the machine's modifiers were when it did. There is no
/// layout in the kernel, so there is nothing here that a layout could change.
///
/// **`modifiers` is the union across every keyboard the machine has**, which
/// is why it is here rather than derived by whoever translates: Shift held on
/// one keyboard and a letter typed on another must produce a capital, and a
/// surface that reconstructs the mask from the transitions it saw has not seen
/// the ones that arrived while another surface had the focus.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawKeyEvent {
    pub keycode: u8,
    pub modifiers: u8,
}

/// Every byte belongs to a field: this crosses the boundary through
/// `as_bytes`, so a gap would publish whatever the kernel stack held.
const _: () = assert!(core::mem::size_of::<RawKeyEvent>() == 1 + 1);

impl RawKeyEvent {
    pub fn pressed(&self) -> bool { self.modifiers & MOD_RELEASED == 0 }
    pub fn released(&self) -> bool { self.modifiers & MOD_RELEASED != 0 }
    pub fn shift(&self) -> bool { self.modifiers & MOD_SHIFT != 0 }
    pub fn ctrl(&self) -> bool { self.modifiers & MOD_CTRL != 0 }
    pub fn alt(&self) -> bool { self.modifiers & MOD_ALT != 0 }
    pub fn gui(&self) -> bool { self.modifiers & MOD_GUI != 0 }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `self` is a valid `&Self` (non-null, aligned, readable for
        // `size_of::<Self>()` bytes), and the const assert above proves the
        // `repr(C)` layout has no padding, so every byte the slice exposes is
        // an initialized field, not a gap.
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }
}

/// A mouse/tablet event as delivered by the kernel through the mouse handle.
/// Carries absolute coordinates (0–32767) from the USB tablet.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MouseEvent {
    pub buttons: u8,
    pub scroll: i8,
    pub abs_x: u16,
    pub abs_y: u16,
}

/// Every byte belongs to a field: this crosses the boundary through
/// `as_bytes`, so a gap would publish whatever the kernel stack held.
const _: () = assert!(core::mem::size_of::<MouseEvent>() == 1 + 1 + 2 + 2);

impl MouseEvent {
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `self` is a valid `&Self` (non-null, aligned, readable for
        // `size_of::<Self>()` bytes), and the const assert above proves the
        // `repr(C)` layout has no padding, so every byte the slice exposes is
        // an initialized field, not a gap.
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }
}

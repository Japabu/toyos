use core::ffi::c_void;
use core::ptr::NonNull;

use super::DisplayHandle;

/// Raw display handle for ToyOS.
///
/// ## Thread Safety
///
/// This handle contains no borrowed data, so it is `Send` and `Sync`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToyOsDisplayHandle {}

impl ToyOsDisplayHandle {
    /// Create a new empty display handle.
    pub fn new() -> Self {
        Self {}
    }
}

impl DisplayHandle<'static> {
    /// Create a ToyOS-based display handle.
    ///
    /// As no data is borrowed by this handle, it is completely safe to create.
    pub fn toyos() -> Self {
        // SAFETY: No data is borrowed.
        unsafe { Self::borrow_raw(ToyOsDisplayHandle::new().into()) }
    }
}

/// Raw window handle for ToyOS.
///
/// ## Thread Safety
///
/// The window handle carries a pointer to the underlying window object.
/// This type is `Send` and `Sync`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToyOsWindowHandle {
    /// A pointer to the ToyOS window object.
    pub window: NonNull<c_void>,
}

unsafe impl Send for ToyOsWindowHandle {}
unsafe impl Sync for ToyOsWindowHandle {}

impl ToyOsWindowHandle {
    /// Create a new handle to a window.
    pub fn new(window: NonNull<c_void>) -> Self {
        Self { window }
    }
}

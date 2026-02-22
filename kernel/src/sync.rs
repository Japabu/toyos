use core::cell::UnsafeCell;

/// Interior-mutability cell for single-core kernel globals.
///
/// Safe because: single core, no preemption, interrupts masked during syscalls.
/// `#[repr(transparent)]` ensures layout matches `T`, so `#[no_mangle]` statics
/// used from naked asm (SYSCALL_KERNEL_RSP, SYSCALL_USER_RSP) keep their layout.
#[repr(transparent)]
pub struct SyncCell<T>(UnsafeCell<T>);

const _: () = assert!(size_of::<SyncCell<u64>>() == 8);

// SAFETY: Single-core kernel with no concurrent access.
unsafe impl<T> Sync for SyncCell<T> {}

impl<T> SyncCell<T> {
    pub const fn new(val: T) -> Self {
        Self(UnsafeCell::new(val))
    }

    pub fn get(&self) -> &T {
        unsafe { &*self.0.get() }
    }

    pub fn get_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }

    pub fn as_ptr(&self) -> *mut T {
        self.0.get()
    }
}

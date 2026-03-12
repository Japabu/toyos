use core::marker::PhantomData;

use crate::UserAddr;

/// Marker for types safe to interpret from / write to validated user pointers.
///
/// # Safety
/// Must be `#[repr(C)]`, `Copy`, have no padding, and be valid for any bit pattern.
pub unsafe trait UserSafe: Copy {}

// Primitives used in syscall arguments.
unsafe impl UserSafe for u32 {}
unsafe impl UserSafe for u64 {}
unsafe impl UserSafe for [u32; 2] {}

// Kernel types.
unsafe impl UserSafe for crate::fd::Stat {}

// ABI types (toyos-abi cannot depend on external trait crates).
unsafe impl UserSafe for toyos_abi::syscall::SpawnArgs {}
unsafe impl UserSafe for toyos_abi::message::RawMessage {}
unsafe impl UserSafe for toyos_abi::input::RawKeyEvent {}
unsafe impl UserSafe for toyos_abi::input::MouseEvent {}

/// Check that all pages in [ptr..ptr+size) are in the user half of the address space.
/// Pages may not yet be mapped (demand paging); the kernel-mode page fault handler
/// will map them when the kernel dereferences the validated pointer.
fn check_user_range(ptr: UserAddr, size: u64) -> bool {
    if size == 0 { return true; }
    let raw = ptr.raw();
    let Some(end) = raw.checked_add(size) else { return false };
    // Reject kernel-space pointers (above canonical hole)
    end <= 0x0000_8000_0000_0000
}

/// Context for a single syscall invocation. All user pointer access goes
/// through this type, tying reference lifetimes to the syscall scope.
///
/// Create one on the stack in `syscall_dispatch`, pass `&ctx` to handlers.
/// The lifetime `'a` prevents validated references from escaping the syscall.
pub struct SyscallContext<'a> {
    _scope: PhantomData<&'a mut ()>,
}

impl<'a> SyscallContext<'a> {
    /// # Safety
    /// Caller guarantees the current process's page tables remain active
    /// for the lifetime `'a`.
    pub unsafe fn new() -> Self {
        Self { _scope: PhantomData }
    }

    /// Validate a user pointer range and return a shared byte slice.
    pub fn user_slice(&self, ptr: UserAddr, len: u64) -> Option<&'a [u8]> {
        let len = len as usize;
        if len == 0 {
            return Some(&[]);
        }
        if !check_user_range(ptr, len as u64) {
            return None;
        }
        Some(unsafe { core::slice::from_raw_parts(ptr.raw() as *const u8, len) })
    }

    /// Validate a user pointer range and return a mutable byte slice.
    pub fn user_slice_mut(&self, ptr: UserAddr, len: u64) -> Option<&'a mut [u8]> {
        let len = len as usize;
        if len == 0 {
            return Some(&mut []);
        }
        if !check_user_range(ptr, len as u64) {
            return None;
        }
        Some(unsafe { core::slice::from_raw_parts_mut(ptr.raw() as *mut u8, len) })
    }

    /// Validate a user pointer range as a UTF-8 string.
    pub fn user_str(&self, ptr: UserAddr, len: u64) -> Option<&'a str> {
        let slice = self.user_slice(ptr, len)?;
        core::str::from_utf8(slice).ok()
    }

    /// Validate a user pointer to a typed struct (immutable).
    pub fn user_ref<T: UserSafe>(&self, ptr: UserAddr) -> Option<&'a T> {
        let size = core::mem::size_of::<T>() as u64;
        if size == 0 || !check_user_range(ptr, size) {
            return None;
        }
        if ptr.raw() as usize % core::mem::align_of::<T>() != 0 {
            return None;
        }
        Some(unsafe { &*(ptr.raw() as *const T) })
    }

    /// Validate a user pointer to a typed struct (mutable).
    pub fn user_mut<T: UserSafe>(&self, ptr: UserAddr) -> Option<&'a mut T> {
        let size = core::mem::size_of::<T>() as u64;
        if size == 0 || !check_user_range(ptr, size) {
            return None;
        }
        if ptr.raw() as usize % core::mem::align_of::<T>() != 0 {
            return None;
        }
        Some(unsafe { &mut *(ptr.raw() as *mut T) })
    }

    /// Validate a user pointer to a slice of typed structs.
    pub fn user_slice_of<T: UserSafe>(&self, ptr: UserAddr, count: usize) -> Option<&'a [T]> {
        if count == 0 {
            return Some(&[]);
        }
        let byte_len = count.checked_mul(core::mem::size_of::<T>())?;
        if !check_user_range(ptr, byte_len as u64) {
            return None;
        }
        if ptr.raw() as usize % core::mem::align_of::<T>() != 0 {
            return None;
        }
        Some(unsafe { core::slice::from_raw_parts(ptr.raw() as *const T, count) })
    }
}

use core::marker::PhantomData;
use crate::arch::paging;

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
    pub fn user_slice(&self, ptr: u64, len: u64) -> Option<&'a [u8]> {
        let len = len as usize;
        if len == 0 {
            return Some(&[]);
        }
        if !paging::is_user_mapped(ptr, len as u64) {
            return None;
        }
        Some(unsafe { core::slice::from_raw_parts(ptr as *const u8, len) })
    }

    /// Validate a user pointer range and return a mutable byte slice.
    pub fn user_slice_mut(&self, ptr: u64, len: u64) -> Option<&'a mut [u8]> {
        let len = len as usize;
        if len == 0 {
            return Some(&mut []);
        }
        if !paging::is_user_mapped(ptr, len as u64) {
            return None;
        }
        Some(unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len) })
    }

    /// Validate a user pointer range as a UTF-8 string.
    pub fn user_str(&self, ptr: u64, len: u64) -> Option<&'a str> {
        let slice = self.user_slice(ptr, len)?;
        core::str::from_utf8(slice).ok()
    }

    /// Validate a user pointer to a typed struct (immutable).
    pub fn user_ref<T: UserSafe>(&self, ptr: u64) -> Option<&'a T> {
        let size = core::mem::size_of::<T>() as u64;
        if size == 0 || !paging::is_user_mapped(ptr, size) {
            return None;
        }
        if ptr as usize % core::mem::align_of::<T>() != 0 {
            return None;
        }
        Some(unsafe { &*(ptr as *const T) })
    }

    /// Validate a user pointer to a typed struct (mutable).
    pub fn user_mut<T: UserSafe>(&self, ptr: u64) -> Option<&'a mut T> {
        let size = core::mem::size_of::<T>() as u64;
        if size == 0 || !paging::is_user_mapped(ptr, size) {
            return None;
        }
        if ptr as usize % core::mem::align_of::<T>() != 0 {
            return None;
        }
        Some(unsafe { &mut *(ptr as *mut T) })
    }

    /// Validate a user pointer to a slice of typed structs.
    pub fn user_slice_of<T: UserSafe>(&self, ptr: u64, count: usize) -> Option<&'a [T]> {
        if count == 0 {
            return Some(&[]);
        }
        let byte_len = count.checked_mul(core::mem::size_of::<T>())?;
        if !paging::is_user_mapped(ptr, byte_len as u64) {
            return None;
        }
        if ptr as usize % core::mem::align_of::<T>() != 0 {
            return None;
        }
        Some(unsafe { core::slice::from_raw_parts(ptr as *const T, count) })
    }
}

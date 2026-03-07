use bytemuck::Pod;
use core::marker::PhantomData;
use crate::arch::paging;

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

    /// Validate a user pointer to a `Pod` struct (immutable).
    /// `Pod` guarantees no padding, valid for any bit pattern, properly aligned.
    pub fn user_ref<T: Pod>(&self, ptr: u64) -> Option<&'a T> {
        let size = core::mem::size_of::<T>() as u64;
        if size == 0 || !paging::is_user_mapped(ptr, size) {
            return None;
        }
        if ptr as usize % core::mem::align_of::<T>() != 0 {
            return None;
        }
        Some(unsafe { &*(ptr as *const T) })
    }

    /// Validate a user pointer to a `Pod` struct (mutable).
    pub fn user_mut<T: Pod>(&self, ptr: u64) -> Option<&'a mut T> {
        let size = core::mem::size_of::<T>() as u64;
        if size == 0 || !paging::is_user_mapped(ptr, size) {
            return None;
        }
        if ptr as usize % core::mem::align_of::<T>() != 0 {
            return None;
        }
        Some(unsafe { &mut *(ptr as *mut T) })
    }

    /// Validate a user pointer to a single `Pod` struct.
    pub fn user_pod<T: Pod>(&self, ptr: u64) -> Option<&'a T> {
        let size = core::mem::size_of::<T>();
        if !paging::is_user_mapped(ptr, size as u64) {
            return None;
        }
        if ptr as usize % core::mem::align_of::<T>() != 0 {
            return None;
        }
        Some(unsafe { &*(ptr as *const T) })
    }

    /// Validate a user pointer and read a `Copy` struct.
    /// Like `user_pod` but doesn't require `Pod` — caller asserts the type is
    /// `#[repr(C)]` with no padding invariants.
    ///
    /// # Safety
    /// `T` must be `#[repr(C)]` and valid for any bit pattern (no padding traps).
    pub unsafe fn user_read<T: Copy>(&self, ptr: u64) -> Option<T> {
        let size = core::mem::size_of::<T>();
        if size == 0 || !paging::is_user_mapped(ptr, size as u64) {
            return None;
        }
        if ptr as usize % core::mem::align_of::<T>() != 0 {
            return None;
        }
        Some(core::ptr::read(ptr as *const T))
    }

    /// Validate a user pointer to a slice of `Pod` structs.
    /// Checks mapping, alignment, and arithmetic overflow.
    pub fn user_pod_slice<T: Pod>(&self, ptr: u64, count: usize) -> Option<&'a [T]> {
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

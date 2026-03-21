//! Safe user memory access via page table walk + kernel direct map.
//!
//! User virtual addresses are translated to physical via page table walk,
//! then accessed through the kernel's high-half direct map (PHYS_OFFSET).
//! SMAP stays enabled 100% — no stac/clac anywhere.
//!
//! Returns kernel-accessible references (&T, &[u8], &str) that point into
//! the direct map. These are valid for the duration of the syscall.

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

// ABI types.
unsafe impl UserSafe for toyos_abi::syscall::SpawnArgs {}

unsafe impl UserSafe for toyos_abi::input::RawKeyEvent {}
unsafe impl UserSafe for toyos_abi::input::MouseEvent {}

/// Check that [ptr..ptr+size) is in the user half of the address space.
fn check_user_range(ptr: UserAddr, size: u64) -> bool {
    if size == 0 { return true; }
    let raw = ptr.raw();
    let Some(end) = raw.checked_add(size) else { return false };
    end <= 0x0000_8000_0000_0000
}

/// Translate a user virtual address to a kernel-accessible pointer via
/// page table walk + direct map. Triggers demand paging if needed.
fn translate(user_addr: u64) -> Option<*mut u8> {
    let addr_space = crate::process::current_address_space();
    if let Some(dm) = addr_space.virt_to_phys(UserAddr::new(user_addr)) {
        return Some(dm.as_mut_ptr());
    }
    if !crate::process::handle_page_fault(user_addr, 0) {
        return None;
    }
    addr_space.virt_to_phys(UserAddr::new(user_addr)).map(|dm| dm.as_mut_ptr())
}


/// Context for a single syscall invocation. All user pointer access goes
/// through this type, tying reference lifetimes to the syscall scope.
///
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
    /// The returned slice points into the kernel direct map.
    ///
    /// Safe only if the user buffer is physically contiguous (single 2MB page
    /// or contiguous allocation like stack/TLS/mmap). For buffers that may
    /// span independently demand-paged 2MB pages, the physical pages might not
    /// be contiguous — the slice would read wrong memory at page boundaries.
    ///
    /// Currently safe because: stack (contiguous OwnedAlloc), TLS (contiguous),
    /// mmap (contiguous), pipes (single 2MB page). Demand-paged ELF code is
    /// never accessed via user_slice (only via page fault handler).
    pub fn user_slice(&self, ptr: UserAddr, len: u64) -> Option<&'a [u8]> {
        let len = len as usize;
        if len == 0 {
            return Some(&[]);
        }
        if !check_user_range(ptr, len as u64) {
            return None;
        }
        let kptr = translate(ptr.raw())?;
        // Verify contiguity at every 2MB page boundary crossing.
        // One translate() per boundary — negligible for typical syscall buffers.
        let start = ptr.raw();
        let end = start + len as u64;
        let mut boundary = (start & !(crate::mm::PAGE_2M - 1)) + crate::mm::PAGE_2M;
        while boundary < end {
            let k = translate(boundary)?;
            let expected = unsafe { kptr.add((boundary - start) as usize) };
            if k != expected {
                return None;
            }
            boundary += crate::mm::PAGE_2M;
        }
        if len > 1 {
            let end_kptr = translate(end - 1)?;
            let expected_end = unsafe { kptr.add(len - 1) };
            if end_kptr != expected_end {
                return None;
            }
        }
        Some(unsafe { core::slice::from_raw_parts(kptr as *const u8, len) })
    }

    /// Validate a user pointer range and return a mutable byte slice.
    /// Same contiguity constraints as user_slice.
    pub fn user_slice_mut(&self, ptr: UserAddr, len: u64) -> Option<&'a mut [u8]> {
        let len = len as usize;
        if len == 0 {
            return Some(&mut []);
        }
        if !check_user_range(ptr, len as u64) {
            return None;
        }
        let kptr = translate(ptr.raw())?;
        let start = ptr.raw();
        let end = start + len as u64;
        let mut boundary = (start & !(crate::mm::PAGE_2M - 1)) + crate::mm::PAGE_2M;
        while boundary < end {
            let k = translate(boundary)?;
            let expected = unsafe { kptr.add((boundary - start) as usize) };
            if k != expected {
                return None;
            }
            boundary += crate::mm::PAGE_2M;
        }
        if len > 1 {
            let end_kptr = translate(end - 1)?;
            let expected_end = unsafe { kptr.add(len - 1) };
            if end_kptr != expected_end {
                return None;
            }
        }
        Some(unsafe { core::slice::from_raw_parts_mut(kptr, len) })
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
        let kptr = translate(ptr.raw())?;
        Some(unsafe { &*(kptr as *const T) })
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
        let kptr = translate(ptr.raw())?;
        Some(unsafe { &mut *(kptr as *mut T) })
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
        let kptr = translate(ptr.raw())?;
        // Verify contiguity at every 2MB page boundary crossing.
        let start = ptr.raw();
        let end = start + byte_len as u64;
        let mut boundary = (start & !(crate::mm::PAGE_2M - 1)) + crate::mm::PAGE_2M;
        while boundary < end {
            let k = translate(boundary)?;
            let expected = unsafe { kptr.add((boundary - start) as usize) };
            if k != expected {
                return None;
            }
            boundary += crate::mm::PAGE_2M;
        }
        if byte_len > 1 {
            let end_kptr = translate(end - 1)?;
            let expected_end = unsafe { kptr.add(byte_len - 1) };
            if end_kptr != expected_end {
                return None;
            }
        }
        Some(unsafe { core::slice::from_raw_parts(kptr as *const T, count) })
    }

    /// Validate a user pointer to a mutable slice of typed structs.
    /// Same contiguity constraints as user_slice_of.
    #[allow(dead_code)]
    pub fn user_slice_of_mut<T: UserSafe>(&self, ptr: UserAddr, count: usize) -> Option<&'a mut [T]> {
        if count == 0 {
            return Some(&mut []);
        }
        let byte_len = count.checked_mul(core::mem::size_of::<T>())?;
        if !check_user_range(ptr, byte_len as u64) {
            return None;
        }
        if ptr.raw() as usize % core::mem::align_of::<T>() != 0 {
            return None;
        }
        let kptr = translate(ptr.raw())?;
        // Verify contiguity at every 2MB page boundary crossing.
        let start = ptr.raw();
        let end = start + byte_len as u64;
        let mut boundary = (start & !(crate::mm::PAGE_2M - 1)) + crate::mm::PAGE_2M;
        while boundary < end {
            let k = translate(boundary)?;
            let expected = unsafe { kptr.add((boundary - start) as usize) };
            if k != expected {
                return None;
            }
            boundary += crate::mm::PAGE_2M;
        }
        if byte_len > 1 {
            let end_kptr = translate(end - 1)?;
            let expected_end = unsafe { kptr.add(byte_len - 1) };
            if end_kptr != expected_end {
                return None;
            }
        }
        Some(unsafe { core::slice::from_raw_parts_mut(kptr as *mut T, count) })
    }
}

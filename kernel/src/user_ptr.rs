//! Safe user memory access via page table walk + kernel direct map.
//!
//! User virtual addresses are translated to physical via page table walk,
//! then accessed through the kernel's high-half direct map (PHYS_OFFSET).
//! SMAP stays enabled 100% — no stac/clac anywhere.
//!
//! Returns kernel-accessible references (&T, &[u8], &str) that point into
//! the direct map. These are valid for the duration of the syscall.

use core::marker::PhantomData;

use toyos_abi::syscall::SyscallError;

use crate::UserAddr;

/// Longest string, in bytes, the kernel accepts from userspace — for every
/// syscall that takes one.
///
/// The bound lives on the primitive rather than at its ~20 call sites because
/// every consumer either copies the string onto the kernel heap or splits it
/// into borrowed tokens; none of them stream it, so none of them wants a
/// different answer. The number is set by the largest *derived* allocation,
/// not by the string itself: 64 KiB of `"a\0"` is 32768 spawn argv tokens, and
/// the `Vec<&str>` holding them is 512 KiB — comfortably under the allocator's
/// 2 MiB single-allocation ceiling. A tighter PATH_MAX for the path syscalls
/// would buy a second constant and a second check site for no safety.
pub const MAX_USER_STR: u64 = 64 * 1024;

/// Marker for types safe to interpret from / write to validated user pointers.
///
/// # Safety
/// Must be `#[repr(C)]`, `Copy`, have no padding, and be valid for any bit pattern.
pub unsafe trait UserSafe: Copy {}

// Primitives used in syscall arguments.
unsafe impl UserSafe for u32 {}
unsafe impl UserSafe for u64 {}
unsafe impl UserSafe for [u32; 2] {}
unsafe impl UserSafe for [u64; 2] {}

// Kernel types.
unsafe impl UserSafe for crate::fd::Stat {}

// ABI types.
unsafe impl UserSafe for toyos_abi::syscall::SpawnArgs {}

unsafe impl UserSafe for toyos_abi::input::RawKeyEvent {}
unsafe impl UserSafe for toyos_abi::input::MouseEvent {}

/// Translate a user virtual address to its direct-map address, demand-paging
/// it in if it is not mapped yet.
///
/// `pub(crate)` for the futex, whose word the *scheduler* dereferences long
/// after the syscall that named it has returned — the one user address the
/// kernel keeps rather than reads.
pub(crate) fn translate_user(addr: UserAddr) -> Option<crate::mm::DirectMap> {
    let pt = crate::process::current_address_space();
    if let Some(dm) = pt.lock().translate(addr) {
        return Some(dm);
    }
    if !crate::process::handle_page_fault(addr.raw(), 0) {
        return None;
    }
    let result = pt.lock().translate(addr);
    result
}

fn translate(user_addr: u64) -> Option<*mut u8> {
    translate_user(UserAddr::new(user_addr)).map(|dm| dm.as_mut_ptr())
}

/// The direct-map address of a `T` the kernel may read or write at `ptr`.
///
/// One translation answers for one 2 MiB page, so [`user_span::is_user_object`]
/// is what stands between a value near a page boundary and a copy that walks
/// off the end of a *physical* page into whatever the PMM handed out next.
fn object<T: UserSafe>(ptr: UserAddr) -> Result<*mut u8, SyscallError> {
    let ok = crate::mm::user_span::is_user_object(
        ptr.raw(),
        core::mem::size_of::<T>() as u64,
        core::mem::align_of::<T>() as u64,
    );
    if !ok {
        return Err(SyscallError::BadAddress);
    }
    translate(ptr.raw()).ok_or(SyscallError::BadAddress)
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
        if !crate::mm::user_span::in_user_half(ptr.raw(), len as u64) {
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
        if !crate::mm::user_span::in_user_half(ptr.raw(), len as u64) {
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

    /// Validate a user pointer range as a UTF-8 string of at most
    /// [`MAX_USER_STR`] bytes.
    ///
    /// Unlike the slice accessors this returns a typed error, because an
    /// over-long or non-UTF-8 string is a bad *argument*, not a bad address —
    /// the range may be perfectly mapped. `user_slice` stays unbounded on
    /// purpose: read/write buffers are borrowed, never copied, so their size
    /// costs the kernel nothing.
    pub fn user_str(&self, ptr: UserAddr, len: u64) -> Result<&'a str, SyscallError> {
        if len > MAX_USER_STR {
            return Err(SyscallError::InvalidArgument);
        }
        let slice = self.user_slice(ptr, len).ok_or(SyscallError::BadAddress)?;
        core::str::from_utf8(slice).map_err(|_| SyscallError::InvalidArgument)
    }

    /// Read a typed value out of user memory.
    ///
    /// A copy rather than a borrow: a `&T` over a page userland can still write
    /// is a claim the compiler enforces and the hardware does not, and the
    /// kernel would be reading a value that can change between two of its own
    /// reads. Every `UserSafe` type is at most 48 bytes, so the copy costs less
    /// than the second lock-and-translate the borrow already paid.
    pub fn copy_in<T: UserSafe>(&self, ptr: UserAddr) -> Result<T, SyscallError> {
        let kptr = object::<T>(ptr)?;
        Ok(unsafe { (kptr as *const T).read_volatile() })
    }

    /// Write a typed value into user memory.
    pub fn copy_out<T: UserSafe>(&self, ptr: UserAddr, value: &T) -> Result<(), SyscallError> {
        let kptr = object::<T>(ptr)?;
        unsafe { (kptr as *mut T).write_volatile(*value) };
        Ok(())
    }

    /// Validate a user pointer to a typed struct (immutable).
    pub fn user_ref<T: UserSafe>(&self, ptr: UserAddr) -> Option<&'a T> {
        let size = core::mem::size_of::<T>() as u64;
        if size == 0 || !crate::mm::user_span::in_user_half(ptr.raw(), size) {
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
        if size == 0 || !crate::mm::user_span::in_user_half(ptr.raw(), size) {
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
        if !crate::mm::user_span::in_user_half(ptr.raw(), byte_len as u64) {
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
        if !crate::mm::user_span::in_user_half(ptr.raw(), byte_len as u64) {
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

//! Safe user memory access via page table walk + kernel direct map.
//!
//! User virtual addresses are translated to physical via page table walk,
//! then accessed through the kernel's high-half direct map (PHYS_OFFSET).
//! SMAP stays enabled 100% — no stac/clac anywhere.
//!
//! **Nothing here hands out a reference to user memory.** Small values are
//! copied ([`SyscallContext::copy_in`], [`SyscallContext::copy_out`]), strings
//! are copied, and bulk buffers are a [`UserBytes`] / [`UserBytesMut`] window
//! the kernel reads or writes but never borrows — see [`UserBytes`] for why the
//! borrow was the bug.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
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

// **Every impl below is irreducible, and the reason is the same one for all
// fourteen: `UserSafe` states a property of a type's *bytes* — `#[repr(C)]`,
// no padding, every bit pattern a value — and Rust offers no way to observe
// any of the three.** There is no `const fn` that reports a struct's padding
// and no trait bound that means "valid for any bit pattern", so the only
// mechanical alternative is a derive macro, which would be a proc-macro crate
// this kernel does not have and would move the same claim rather than check
// it. What each impl below can do is say what it checked, and every one was
// checked by reading the definition: `#[repr(C)]`, `Copy`, integer fields
// only, and an explicit `_pad` wherever alignment would otherwise have left a
// hole. Integer fields only is what makes "any bit pattern" true — nothing
// here holds an enum, a `bool`, a `char`, a reference or a `NonZero`.
//
// Padding matters in both directions. `copy_out` writes `size_of::<T>()`
// bytes into a page userland reads, so a padding byte would be uninitialized
// kernel stack leaving the kernel; `copy_in` reads the same width back, so a
// padding byte would be a value the kernel never wrote.

// Primitives used in syscall arguments.
// SAFETY: a primitive integer, and an array of them, is `#[repr(C)]`-compatible
// by definition, has no padding, and every bit pattern is a value.
unsafe impl UserSafe for u32 {}
// SAFETY: see `u32`.
unsafe impl UserSafe for u64 {}
// SAFETY: see `u32` — an array adds no padding between elements.
unsafe impl UserSafe for [u32; 2] {}
// SAFETY: see `u32`.
unsafe impl UserSafe for [u64; 2] {}

// Kernel types.
// SAFETY: `#[repr(C)] Copy`, three `u64`s (`object::ops::Stat`) — 24 bytes, no
// padding, and `file_type` is a `u64` and not the enum it names precisely so
// that every bit pattern stays a value.
unsafe impl UserSafe for crate::object::ops::Stat {}

// ABI types.
// SAFETY: `#[repr(C)] Copy`, ten `u64`s — 80 bytes, no padding. Every field is
// a pointer or a length the kernel validates where it uses it, never here.
unsafe impl UserSafe for toyos_abi::syscall::SpawnArgs {}
// SAFETY: `#[repr(C)] Copy`, `RawHandle` (a `#[repr(transparent)]` `u32`), an
// explicit `_pad: u32`, then six `u64`s — 56 bytes, no padding.
unsafe impl UserSafe for toyos_abi::syscall::NamespaceBuild {}
// SAFETY: `#[repr(C)] Copy`, `u64`, `u64`, `i64` — 24 bytes, no padding.
unsafe impl UserSafe for toyos_abi::syscall::SchedInfo {}
// SAFETY: `#[repr(C)] Copy`, `u64`s and `u32`s in pairs — 128 bytes, no
// padding: the two `u32` pairs (`fault_demand_count`/`fault_zero_count` and
// `io_read_ops`/`pid`) are what keeps every `u64` 8-aligned, and `pid` was
// named padding until it was given a meaning.
unsafe impl UserSafe for toyos_abi::syscall::ProcessStats {}
// SAFETY: `#[repr(C)] Copy`, `[RawHandle; 2]` then six `u32`s — 32 bytes,
// align 4, no padding.
unsafe impl UserSafe for toyos_abi::FramebufferInfo {}
// SAFETY: `#[repr(C)] Copy`, `RawHandle`, an explicit `_pad: u32`, `u64` — 16
// bytes, no padding.
unsafe impl UserSafe for toyos_abi::syscall::InboxSetup {}

// SAFETY: `#[repr(C)] Copy`, two `u8`s — 2 bytes, align 1, no padding.
// `keycode` and `modifiers` are `u8` and not enums or bitflags exactly so that
// a transition the kernel does not recognise is still a value.
unsafe impl UserSafe for toyos_abi::input::RawKeyEvent {}
// SAFETY: `#[repr(C)] Copy`, `u8`, `i8`, `u16`, `u16` — 6 bytes, align 2, and
// the two bytes ahead of `abs_x` are what makes it 2-aligned, so there is no
// padding.
unsafe impl UserSafe for toyos_abi::input::MouseEvent {}

/// 88 bytes of `u32`, `u32`, `u64`, `u64` and `[u64; 8]` — `#[repr(C)]`, no
/// padding by its own `const` size assertion, and valid for every bit pattern:
/// the kernel clamps each number where it uses it rather than trusting it where
/// it arrives (`log::read::Cursor::from_reader`).
// SAFETY: the doc comment above is the argument, and this one carries the
// evidence: `toyos_abi::log`'s `const _: () = assert!(size_of::<LogCursor>()
// == 24 + 8 * MAX_LOG_SHARDS)` fails to compile if a padding byte ever appears
// — the only member of this list whose no-padding claim a gate checks.
unsafe impl UserSafe for toyos_abi::log::LogCursor {}

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
/// One translation answers for one 2 MiB page, so
/// [`toyos_userbound::is_user_object`] is what stands between a value near a
/// page boundary and a copy that walks off the end of a *physical* page into
/// whatever the PMM handed out next.
fn object<T: UserSafe>(ptr: UserAddr) -> Result<*mut u8, SyscallError> {
    let ok = toyos_userbound::is_user_object(
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

    /// A bulk buffer the kernel reads out of and never borrows.
    pub fn user_bytes(&self, ptr: UserAddr, len: u64) -> Option<UserBytes<'a>> {
        let len = len as usize;
        let kptr = if len == 0 { core::ptr::NonNull::dangling().as_ptr() } else { window(ptr, len)? };
        Some(UserBytes { kptr, len, _scope: PhantomData })
    }

    /// A bulk buffer the kernel writes into and never borrows.
    pub fn user_bytes_mut(&self, ptr: UserAddr, len: u64) -> Option<UserBytesMut<'a>> {
        let len = len as usize;
        let kptr = if len == 0 { core::ptr::NonNull::dangling().as_ptr() } else { window(ptr, len)? };
        Some(UserBytesMut { kptr, len, _scope: PhantomData })
    }

    /// Copy a user string of at most [`MAX_USER_STR`] bytes into kernel memory.
    ///
    /// Copied, not borrowed: a `&str` over a page userland can rewrite while
    /// the VFS walks it is a path that can be one thing when it is resolved and
    /// another when it is opened. The allocation is the string's own length,
    /// and every consumer was already copying it or splitting it into tokens
    /// that outlive nothing.
    ///
    /// The error is typed because an over-long or non-UTF-8 string is a bad
    /// *argument*, not a bad address — the range may be perfectly mapped.
    pub fn user_str(&self, ptr: UserAddr, len: u64) -> Result<String, SyscallError> {
        if len > MAX_USER_STR {
            return Err(SyscallError::InvalidArgument);
        }
        let bytes = self.user_vec(ptr, len)?;
        String::from_utf8(bytes).map_err(|_| SyscallError::InvalidArgument)
    }

    /// Read a typed value out of user memory.
    ///
    /// A copy rather than a borrow: a `&T` over a page userland can still write
    /// is a claim the compiler enforces and the hardware does not, and the
    /// kernel would be reading a value that can change between two of its own
    /// reads. Every `UserSafe` type is at most 128 bytes, so the copy costs less
    /// than the second lock-and-translate the borrow already paid.
    pub fn copy_in<T: UserSafe>(&self, ptr: UserAddr) -> Result<T, SyscallError> {
        let kptr = object::<T>(ptr)?;
        // SAFETY: `object::<T>` returned, so `is_user_object` accepted
        // `size_of::<T>()` bytes at `ptr` with `align_of::<T>()`, and the
        // translation that followed put the whole object inside one 2 MiB
        // physical page — the direct map is what makes that address readable
        // with SMAP on. `T: UserSafe` is the claim that every bit pattern of
        // those bytes is a `T`.
        //
        // Irreducible: this is where a user address becomes a kernel value,
        // and the only alternative to a raw read is a reference — which is
        // precisely what [`UserBytes`]'s header refuses, because another
        // thread of the same process may write these bytes at any instant.
        // `read_volatile` and not a plain read for the same reason: one read,
        // not one the compiler may split, fold or repeat.
        Ok(unsafe { (kptr as *const T).read_volatile() })
    }

    /// Write a typed value into user memory.
    pub fn copy_out<T: UserSafe>(&self, ptr: UserAddr, value: &T) -> Result<(), SyscallError> {
        let kptr = object::<T>(ptr)?;
        // SAFETY: `copy_in`'s argument in the other direction — same
        // validation, same one-page window, and `T: UserSafe` guarantees the
        // `size_of::<T>()` bytes this writes are all initialized, so no
        // padding byte of kernel stack leaves the kernel. Irreducible for
        // `copy_in`'s reason.
        unsafe { (kptr as *mut T).write_volatile(*value) };
        Ok(())
    }

    /// Copy `len` bytes of user memory onto the kernel heap.
    ///
    /// Every caller has a bound of its own on `len` — [`MAX_USER_STR`] for the
    /// strings, the env blob's own check — because this is the one accessor
    /// that puts a userland-chosen size on the allocator.
    pub fn user_vec(&self, ptr: UserAddr, len: u64) -> Result<Vec<u8>, SyscallError> {
        let bytes = self.user_bytes(ptr, len).ok_or(SyscallError::BadAddress)?;
        let mut out = vec![0u8; bytes.len()];
        bytes.read_at(0, &mut out);
        Ok(out)
    }
}

/// A bulk buffer in user memory the kernel copies *out of*, and never borrows.
///
/// **The borrow was the bug.** A `&[u8]` or `&mut [u8]` over a page userland
/// can write carries `noalias` and `dereferenceable` into LLVM, so the compiler
/// is entitled to assume the bytes do not change under it — to hoist a read out
/// of a loop, to fold two of them into one, to reorder a check with the use it
/// guards. Another thread of the same process can change them between any two
/// instructions. Every "read it once and validate the copy" rule the kernel has
/// is unenforceable while the reference exists, because the copy and the check
/// are two reads of the same reference and nothing stops the compiler undoing
/// the split.
///
/// So this type hands out no reference. `read_at` is a raw
/// `copy_nonoverlapping` out of the window, which is what the bytes are: a
/// snapshot as of the moment the kernel looked, exactly like a device read,
/// with no promise of stability attached. A torn buffer is then a *value* the
/// caller has to be prepared for, and the kernel is prepared for it by never
/// deciding anything twice from the same window.
///
/// Not per-byte volatile, deliberately: volatile buys nothing here that the
/// absence of a reference has not already bought — a data race is a data race
/// either way — and it costs the read and write path several times its
/// throughput. The unsoundness was the `&`, not the `memcpy`.
///
/// The window is physically contiguous, proven at every 2 MiB boundary when it
/// is built, which is what makes an offset into it mean anything.
///
/// Read-only, and [`UserBytesMut`] is the other direction, because `&[u8]` and
/// `&mut [u8]` told a syscall's two paths apart and a single type would not:
/// `SYS_WRITE`'s buffer is the caller's to read and nothing below `ops::try_write`
/// has any business storing into it.
pub struct UserBytes<'a> {
    kptr: *const u8,
    len: usize,
    _scope: PhantomData<&'a ()>,
}

impl UserBytes<'_> {
    pub fn len(&self) -> usize {
        self.len
    }

    /// Copy `dst.len()` bytes out of the window at `off`.
    ///
    /// The bound is an assertion rather than a refusal because `off` and
    /// `dst.len()` are the kernel's own arithmetic: userland chose the window's
    /// size, and every offset into it is computed against `len()`.
    pub fn read_at(&self, off: usize, dst: &mut [u8]) {
        assert!(
            off.checked_add(dst.len()).is_some_and(|end| end <= self.len),
            "UserBytes::read_at {off}+{} past a {}-byte window",
            dst.len(),
            self.len
        );
        // SAFETY: the assert above proves `off + dst.len() <= self.len`, and
        // `window` proved the whole `self.len` range is one physically
        // contiguous direct-map window — so `add` stays inside it and the
        // copy reads only bytes this `UserBytes` covers. `dst` is a `&mut`
        // the caller owns, so the two ranges cannot overlap.
        //
        // Irreducible, and deliberately so: the type's own header argues that
        // a `&[u8]` here is the bug, because `noalias` would let the compiler
        // assume bytes another thread of the same process can rewrite do not
        // change. A raw `copy_nonoverlapping` *is* the safe abstraction here;
        // there is nothing above it to move to.
        unsafe {
            core::ptr::copy_nonoverlapping(self.kptr.add(off), dst.as_mut_ptr(), dst.len());
        }
    }

    /// The `len`-byte window at `off` inside this one — what `buf[a..b]` was.
    pub fn sub(&self, off: usize, len: usize) -> UserBytes<'_> {
        assert!(
            off.checked_add(len).is_some_and(|end| end <= self.len),
            "UserBytes::sub {off}+{len} past a {}-byte window",
            self.len
        );
        // SAFETY: the assert proves `off + len <= self.len`, so the result is
        // still inside the contiguous window `window` validated — which is
        // exactly `add`'s requirement. Irreducible: narrowing a window is
        // pointer arithmetic, and the safe spelling (`wrapping_add`) would
        // drop the in-bounds claim this assert has just established.
        UserBytes { kptr: unsafe { self.kptr.add(off) }, len, _scope: PhantomData }
    }
}

/// A bulk buffer in user memory the kernel copies *into*. [`UserBytes`] carries
/// the argument for why neither hands out a reference.
///
/// Write-only, which is one property stronger than `&mut [u8]` was: a kernel
/// that never reads back what it put in a user buffer cannot be made to act on
/// a value another thread of that process substituted in between.
pub struct UserBytesMut<'a> {
    kptr: *mut u8,
    len: usize,
    _scope: PhantomData<&'a mut ()>,
}

impl UserBytesMut<'_> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Copy `src` into the window at `off`.
    ///
    /// The bound is an assertion for the same reason [`UserBytes::read_at`]'s
    /// is: userland chose the size and the kernel computed the offset.
    pub fn write_at(&mut self, off: usize, src: &[u8]) {
        assert!(
            off.checked_add(src.len()).is_some_and(|end| end <= self.len),
            "UserBytesMut::write_at {off}+{} past a {}-byte window",
            src.len(),
            self.len
        );
        // SAFETY: [`UserBytes::read_at`]'s argument in the other direction —
        // the assert proves `off + src.len() <= self.len` and `window` proved
        // the range is one contiguous direct-map window. Irreducible for the
        // same reason: a `&mut [u8]` over a page the process also maps is the
        // borrow this type exists to refuse.
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), self.kptr.add(off), src.len());
        }
    }

    /// Zero `len` bytes of the window at `off`.
    pub fn fill_zero(&mut self, off: usize, len: usize) {
        assert!(
            off.checked_add(len).is_some_and(|end| end <= self.len),
            "UserBytesMut::fill_zero {off}+{len} past a {}-byte window",
            self.len
        );
        // SAFETY: same as `write_at`, with a constant source byte instead of a
        // slice — the assert bounds `off + len` against the validated window.
        unsafe { core::ptr::write_bytes(self.kptr.add(off), 0, len) };
    }

    /// The `len`-byte window at `off` inside this one — what `buf[a..b]` was.
    pub fn sub(&mut self, off: usize, len: usize) -> UserBytesMut<'_> {
        assert!(
            off.checked_add(len).is_some_and(|end| end <= self.len),
            "UserBytesMut::sub {off}+{len} past a {}-byte window",
            self.len
        );
        // SAFETY: [`UserBytes::sub`]'s argument exactly.
        UserBytesMut { kptr: unsafe { self.kptr.add(off) }, len, _scope: PhantomData }
    }
}

/// Bytes the kernel copies *from*, wherever they live.
///
/// It existed for two callers: `file_cache::write_page` was reached both by a
/// syscall carrying a [`UserBytes`] window and by `log_file`, whose bytes were
/// the kernel's own. **`log_file`, the kernel's own file sink, is gone** — the
/// kernel writes no file at all — and `/bin/logd` reaches the page cache
/// through `SYS_WRITE` like any other program, so the second caller is a
/// kernel-owned buffer no more. The abstraction stays because the page cache
/// naming the capability it needs is still the right shape and the kernel still
/// has non-syscall writers; if it ever has none, this goes with them.
pub trait ByteSource {
    fn len(&self) -> usize;
    fn read_at(&self, off: usize, dst: &mut [u8]);
}

impl ByteSource for [u8] {
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }

    fn read_at(&self, off: usize, dst: &mut [u8]) {
        dst.copy_from_slice(&self[off..off + dst.len()]);
    }
}

impl ByteSource for UserBytes<'_> {
    fn len(&self) -> usize {
        UserBytes::len(self)
    }

    fn read_at(&self, off: usize, dst: &mut [u8]) {
        UserBytes::read_at(self, off, dst);
    }
}

/// Validate `[ptr, ptr+len)` as one physically contiguous user window and
/// return its direct-map address.
///
/// One `translate` per 2 MiB boundary crossed, plus the last byte: a window
/// whose pages are not physically adjacent would make an offset into it name a
/// page belonging to somebody else.
///
/// **The two address computations here are `wrapping_add`, and that is not a
/// spelling choice.** Both exist to be *compared* against a translation, and
/// what they are testing is precisely whether the byte at that offset is still
/// inside the same physical allocation. `add` promises the compiler it already
/// is — so on the run where the pages are *not* contiguous, which is the run
/// this function exists to catch, `add` would be undefined behaviour and the
/// comparison it feeds would be one the optimizer is entitled to fold away.
/// Neither result is ever dereferenced.
fn window(ptr: UserAddr, len: usize) -> Option<*mut u8> {
    if !toyos_userbound::in_user_half(ptr.raw(), len as u64) {
        return None;
    }
    let kptr = translate(ptr.raw())?;
    let start = ptr.raw();
    let end = start + len as u64;
    let mut boundary = (start & !(crate::mm::PAGE_2M - 1)) + crate::mm::PAGE_2M;
    while boundary < end {
        let k = translate(boundary)?;
        if k != kptr.wrapping_add((boundary - start) as usize) {
            return None;
        }
        boundary += crate::mm::PAGE_2M;
    }
    if len > 1 && translate(end - 1)? != kptr.wrapping_add(len - 1) {
        return None;
    }
    Some(kptr)
}

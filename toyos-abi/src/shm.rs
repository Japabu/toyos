//! Shared memory with RAII.

use crate::Pid;
use crate::syscall;

/// A shared memory region with automatic cleanup.
///
/// When dropped, the region is unmapped and released.
pub struct SharedMemory {
    token: u32,
    ptr: *mut u8,
    size: usize,
}

// SharedMemory contains a raw pointer but is safe to send between threads —
// the kernel manages the underlying mapping per-process.
unsafe impl Send for SharedMemory {}
unsafe impl Sync for SharedMemory {}

impl SharedMemory {
    /// Allocate a new shared memory region and map it into this process.
    pub fn allocate(size: usize) -> Self {
        let token = syscall::alloc_shared(size);
        let ptr = unsafe { syscall::map_shared(token) };
        assert!(!ptr.is_null(), "map_shared failed");
        Self { token, ptr, size }
    }

    /// Map an existing shared memory region by token.
    ///
    /// The caller must know the region size (typically received via IPC
    /// alongside the token).
    pub fn map(token: u32, size: usize) -> Self {
        let ptr = unsafe { syscall::map_shared(token) };
        assert!(!ptr.is_null(), "map_shared failed");
        Self { token, ptr, size }
    }

    /// The opaque token identifying this shared memory region.
    pub fn token(&self) -> u32 {
        self.token
    }

    /// Grant another process permission to map this region.
    pub fn grant(&self, pid: u32) {
        syscall::grant_shared(self.token, Pid(pid));
    }

    /// Raw pointer to the mapped memory.
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Size of the region in bytes.
    pub fn len(&self) -> usize {
        self.size
    }

    /// View the region as a byte slice.
    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr, self.size) }
    }

    /// View the region as a mutable byte slice.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.size) }
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        syscall::release_shared(self.token);
    }
}

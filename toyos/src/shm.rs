//! Shared memory with RAII.

use toyos_abi::Pid;
use toyos_abi::syscall;

/// A shared memory region with automatic cleanup.
///
/// When dropped, the region is unmapped and released.
pub struct SharedMemory {
    token: u32,
    ptr: *mut u8,
    size: usize,
}

unsafe impl Send for SharedMemory {}
unsafe impl Sync for SharedMemory {}

impl SharedMemory {
    pub fn allocate(size: usize) -> Self {
        let token = syscall::alloc_shared(size);
        let ptr = unsafe { syscall::map_shared(token) };
        assert!(!ptr.is_null(), "map_shared failed");
        Self { token, ptr, size }
    }

    pub fn map(token: u32, size: usize) -> Self {
        let ptr = unsafe { syscall::map_shared(token) };
        assert!(!ptr.is_null(), "map_shared failed");
        Self { token, ptr, size }
    }

    pub fn token(&self) -> u32 {
        self.token
    }

    pub fn grant(&self, pid: u32) {
        syscall::grant_shared(self.token, Pid(pid));
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.size
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr, self.size) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.size) }
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        syscall::release_shared(self.token);
    }
}

//! Shared memory with RAII.

use toyos_abi::Pid;
use toyos_abi::syscall::{self, SyscallError};

/// A shared memory region with automatic cleanup.
///
/// When dropped, the region is unmapped and released.
///
/// **Nothing here is infallible, because none of the three syscalls under it
/// is.** A token arrives over a wire and names a region the kernel will not
/// let this process map; a grant names a process that has exited since it
/// asked. A server holding an infallible signature over either dies of its
/// client — which is what the compositor did, on `grant`, when doom aborted.
pub struct SharedMemory {
    token: u32,
    ptr: *mut u8,
    size: usize,
}

unsafe impl Send for SharedMemory {}
unsafe impl Sync for SharedMemory {}

impl SharedMemory {
    pub fn allocate(size: usize) -> Result<Self, SyscallError> {
        let token = syscall::alloc_shared(size)?;
        Self::map(token, size).inspect_err(|_| syscall::release_shared(token))
    }

    pub fn map(token: u32, size: usize) -> Result<Self, SyscallError> {
        let ptr = unsafe { syscall::try_map_shared(token) }?;
        assert!(!ptr.is_null(), "map_shared returned null");
        Ok(Self { token, ptr, size })
    }

    pub fn token(&self) -> u32 {
        self.token
    }

    /// Let `pid` map this region.
    ///
    /// `InvalidArgument` names a process the table does not have: a peer that
    /// exited between asking for the memory and being handed it.
    pub fn grant(&self, pid: u32) -> Result<(), SyscallError> {
        syscall::grant_shared(self.token, Pid(pid))
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

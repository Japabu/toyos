//! Shared memory with RAII.
//!
//! A region is a **handle**, not a token. Holding one is the whole of being
//! allowed to map it, so there is no grant and no list of pids to keep: giving
//! a peer access is [`SharedMemory::share`] plus
//! [`Connection::send_handles`](crate::ipc::Connection::send_handles), and a
//! peer that never receives it can never name it.
//!
//! The mapping goes away with the last handle, so dropping this is all the
//! cleanup there is.

use toyos_abi::syscall::{self, SyscallError};

use crate::{AsHandle, OwnedHandle, RawHandle};

pub struct SharedMemory {
    handle: OwnedHandle,
    ptr: *mut u8,
    size: usize,
}

unsafe impl Send for SharedMemory {}
unsafe impl Sync for SharedMemory {}

impl SharedMemory {
    /// A fresh region, rounded up to whole 2 MiB pages and mapped.
    ///
    /// Fallible: a size the kernel cannot express is `InvalidArgument` and
    /// memory it does not have is `ResourceExhausted`. A daemon reaches both
    /// through a client's request, so neither may be an assertion here.
    pub fn create(size: usize) -> Result<Self, SyscallError> {
        Self::adopt(syscall::shm_create(size)?, size)
    }

    /// Map a region a peer sent, or one a device description named.
    ///
    /// `size` is the caller's own belief about the region and is not checked
    /// against it — a peer that says 4 MiB and sends 2 is a peer, and every
    /// reader of this memory is already writing bounds against the size it
    /// negotiated.
    pub fn adopt(handle: RawHandle, size: usize) -> Result<Self, SyscallError> {
        let handle = OwnedHandle(handle);
        let ptr = unsafe { syscall::shm_map(handle.fd()) }?;
        assert!(!ptr.is_null(), "shm_map answered null");
        Ok(Self { handle, ptr, size })
    }

    /// A second handle to the same region, for sending to a peer.
    ///
    /// The send *moves* what it is given, so a sender that wants to keep the
    /// region duplicates first — which is the same rule spawn endowment has.
    pub fn share(&self) -> Result<RawHandle, SyscallError> {
        syscall::dup(self.handle.fd())
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr, self.size) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.size) }
    }
}

impl AsHandle for SharedMemory {
    fn as_handle(&self) -> RawHandle {
        self.handle.fd()
    }
}

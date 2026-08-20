//! The set of service names one process can resolve.
//!
//! Immutable once built. A narrower namespace is a *new* one built from an
//! existing one, so a child given a subset cannot widen it back, and a name a
//! process was not given resolves to nothing — there is no second place to ask.

use toyos_abi::handle::HANDLE_INVALID;
use toyos_abi::syscall::{
    self, NameRef, NamespaceBuild, NamespaceEntry, SyscallError, MAX_NAMESPACE_ENTRIES,
};

use crate::ipc::Connection;
use crate::port::Connector;
use crate::{AsHandle, OwnedHandle, RawHandle};

pub struct Namespace(pub(crate) OwnedHandle);

impl Namespace {
    /// Open a connection to `name`.
    ///
    /// `NotFound` is "this process was not given that name" and
    /// [`SyscallError::Gone`] is "the server exited". There is no third answer,
    /// and in particular there is no "not yet".
    pub fn open(&self, name: &str) -> Result<Connection, SyscallError> {
        syscall::namespace_open(self.0.raw(), name).map(|h| Connection(OwnedHandle(h)))
    }

    pub fn into_raw(self) -> RawHandle {
        self.0.into_raw()
    }

    /// # Safety
    /// `raw` must be a live namespace handle this process owns and nothing else
    /// answers for.
    pub unsafe fn from_raw(raw: RawHandle) -> Self {
        Self(OwnedHandle(raw))
    }
}

impl AsHandle for Namespace {
    fn as_handle(&self) -> RawHandle {
        self.0.raw()
    }
}

/// Collects the names and the connectors one `SYS_NAMESPACE_BUILD` will carry.
///
/// The connectors are **borrowed**: building a namespace does not consume them,
/// because the same connector goes into several children's namespaces and init
/// does exactly that.
pub struct Builder<'a> {
    base: RawHandle,
    names: heapless_names::Names,
    keep: heapless_names::Vec<NameRef, MAX_NAMESPACE_ENTRIES>,
    add: heapless_names::Vec<NamespaceEntry, MAX_NAMESPACE_ENTRIES>,
    connectors: core::marker::PhantomData<&'a Connector>,
    overflowed: bool,
}

/// A fixed-capacity name blob and vector, so building a namespace needs no
/// allocator — `/bin/init` builds one per program before anything else runs.
mod heapless_names {
    use toyos_abi::syscall::{MAX_NAMESPACE_ENTRIES, MAX_SERVICE_NAME};

    pub const BLOB: usize = MAX_NAMESPACE_ENTRIES * MAX_SERVICE_NAME;

    pub struct Names {
        pub bytes: [u8; BLOB],
        pub len: usize,
    }

    impl Names {
        pub const fn new() -> Self {
            Self { bytes: [0; BLOB], len: 0 }
        }

        /// The name's `(offset, length)`, or `None` when the blob is full.
        pub fn push(&mut self, name: &str) -> Option<(u32, u32)> {
            if name.len() > MAX_SERVICE_NAME || self.len + name.len() > BLOB {
                return None;
            }
            let off = self.len;
            self.bytes[off..off + name.len()].copy_from_slice(name.as_bytes());
            self.len += name.len();
            Some((off as u32, name.len() as u32))
        }
    }

    pub struct Vec<T, const N: usize> {
        items: [T; N],
        len: usize,
    }

    impl<T: Copy, const N: usize> Vec<T, N> {
        /// `filler` rather than `Default`: the element types live in
        /// `toyos-abi` and the orphan rule puts their impls out of reach here.
        pub fn new(filler: T) -> Self {
            Self { items: [filler; N], len: 0 }
        }

        pub fn push(&mut self, item: T) -> bool {
            if self.len == N {
                return false;
            }
            self.items[self.len] = item;
            self.len += 1;
            true
        }

        pub fn as_slice(&self) -> &[T] {
            &self.items[..self.len]
        }
    }
}

/// A namespace with nothing carried over from anywhere.
///
/// The lifetime is the caller's: `add` borrows the connectors, so a builder
/// lives no longer than they do.
pub fn build<'a>() -> Builder<'a> {
    Builder {
        base: HANDLE_INVALID,
        names: heapless_names::Names::new(),
        keep: heapless_names::Vec::new(NameRef { off: 0, len: 0 }),
        add: heapless_names::Vec::new(NamespaceEntry {
            off: 0,
            len: 0,
            connector: HANDLE_INVALID,
            _pad: 0,
        }),
        connectors: core::marker::PhantomData,
        overflowed: false,
    }
}

impl<'a> Builder<'a> {
    /// Carry `names` over from `base`. A name `base` does not hold is simply
    /// absent from the result: narrowing is an intersection, and asking for a
    /// name you do not hold grants nothing either way.
    pub fn keep(mut self, base: &Namespace, names: &[&str]) -> Self {
        self.base = base.0.raw();
        for name in names {
            match self.names.push(name) {
                Some((off, len)) => {
                    self.overflowed |= !self.keep.push(NameRef { off, len });
                }
                None => self.overflowed = true,
            }
        }
        self
    }

    pub fn add(mut self, name: &str, connector: &'a Connector) -> Self {
        match self.names.push(name) {
            Some((off, len)) => {
                let entry = NamespaceEntry {
                    off,
                    len,
                    connector: connector.as_handle(),
                    _pad: 0,
                };
                self.overflowed |= !self.add.push(entry);
            }
            None => self.overflowed = true,
        }
        self
    }

    pub fn finish(self) -> Result<Namespace, SyscallError> {
        if self.overflowed {
            return Err(SyscallError::InvalidArgument);
        }
        let args = NamespaceBuild {
            base: self.base,
            _pad: 0,
            keep_ptr: self.keep.as_slice().as_ptr() as u64,
            keep_n: self.keep.as_slice().len() as u64,
            add_ptr: self.add.as_slice().as_ptr() as u64,
            add_n: self.add.as_slice().len() as u64,
            names_ptr: self.names.bytes.as_ptr() as u64,
            names_len: self.names.len as u64,
        };
        // SAFETY: every pointer above names this stack frame's own storage,
        // and the syscall reads it before returning.
        unsafe { syscall::namespace_build(&args) }.map(|h| Namespace(OwnedHandle(h)))
    }
}

//! A port, and the two ends of it.
//!
//! Two types, so a client holding a [`Connector`] cannot accept the service's
//! connections: that is a state the compiler refuses rather than a
//! `PermissionDenied` at run time.
//!
//! **A port exists before either process runs.** Whoever creates it hands the
//! acceptor to the server and the connector to its clients, so a connection
//! works from a client's first instruction whether or not the server has
//! reached `accept` or has even been spawned. There is nothing to retry and no
//! timeout anywhere.

use toyos_abi::syscall::{self, SyscallError};

use crate::ipc::Connection;
use crate::{AsHandle, OwnedHandle, RawHandle};

/// The server's end: connections are accepted from it, and nothing is written
/// to it.
pub struct Acceptor(pub(crate) OwnedHandle);

/// A client's end: connections are opened through it, and its queue cannot be
/// read.
pub struct Connector(pub(crate) OwnedHandle);

/// Who accepted, and who connected.
///
/// **`client_pid` goes when handle transfer arrives.** Peer identity is not the
/// kernel's to assert; it survives only because the compositor and soundd still
/// grant shared memory to it.
pub struct Accepted {
    pub conn: Connection,
    pub client_pid: u32,
}

/// Make a port. Grants nothing on its own — a port with no clients is not
/// authority.
pub fn create() -> Result<(Acceptor, Connector), SyscallError> {
    let port = syscall::port_create()?;
    Ok((Acceptor(OwnedHandle(port.acceptor)), Connector(OwnedHandle(port.connector))))
}

impl Acceptor {
    /// Take the oldest queued connection, blocking until there is one.
    pub fn accept(&self) -> Result<Accepted, SyscallError> {
        let r = syscall::accept(self.0.fd())?;
        Ok(Accepted { conn: Connection(OwnedHandle(r.fd)), client_pid: r.client_pid })
    }

    /// Give up ownership, for a handle about to be endowed or transferred.
    pub fn into_raw(self) -> RawHandle {
        self.0.into_raw()
    }

    /// # Safety
    /// `raw` must be a live acceptor handle this process owns and nothing else
    /// answers for.
    pub unsafe fn from_raw(raw: RawHandle) -> Self {
        Self(OwnedHandle(raw))
    }
}

impl Connector {
    pub fn into_raw(self) -> RawHandle {
        self.0.into_raw()
    }

    /// # Safety
    /// `raw` must be a live connector handle this process owns and nothing else
    /// answers for.
    pub unsafe fn from_raw(raw: RawHandle) -> Self {
        Self(OwnedHandle(raw))
    }
}

impl AsHandle for Acceptor {
    fn as_handle(&self) -> RawHandle {
        self.0.fd()
    }
}

impl AsHandle for Connector {
    fn as_handle(&self) -> RawHandle {
        self.0.fd()
    }
}

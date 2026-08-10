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

/// Make a port. Grants nothing on its own — a port with no clients is not
/// authority.
pub fn create() -> Result<(Acceptor, Connector), SyscallError> {
    let port = syscall::port_create()?;
    Ok((Acceptor(OwnedHandle(port.acceptor)), Connector(OwnedHandle(port.connector))))
}

impl Acceptor {
    /// Take the oldest queued connection, blocking until there is one.
    ///
    /// **It answers with the connection and nothing else.** Who connected is
    /// not the kernel's to assert: a server that wants to name its client
    /// reads it out of the protocol's first frame, where it is already the
    /// client's own claim about itself and already distrusted.
    pub fn accept(&self) -> Result<Connection, SyscallError> {
        syscall::accept(self.0.fd()).map(|fd| Connection(OwnedHandle(fd)))
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
    /// A second connector to the same port, for a holder handing one on while
    /// keeping its own.
    pub fn duplicate(&self) -> Result<Self, SyscallError> {
        syscall::dup(self.0.fd()).map(|h| Self(OwnedHandle(h)))
    }

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

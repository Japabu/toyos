use std::fmt;
use std::io;
use std::net::SocketAddr;

use crate::net::TcpStream;
use crate::{event, Interest, Registry, Token};
use toyos_abi::Fd;
use toyos_abi::syscall;

/// A TCP listener backed by kernel pipes via netd.
///
/// netd writes a byte to the notify pipe when a new connection arrives.
/// Polling the notify_fd for readability indicates a connection is ready to accept.
pub struct TcpListener {
    socket_id: toyos::net::TcpSocketId,
    notify_fd: Fd,
    local_addr: SocketAddr,
}

impl TcpListener {
    /// Bind a TCP listener to the given address.
    pub fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
        let ip = match addr {
            SocketAddr::V4(v4) => v4.ip().octets(),
            SocketAddr::V6(_) => {
                return Err(io::Error::new(io::ErrorKind::Unsupported, "IPv6 not supported"));
            }
        };

        let bound = toyos::net::tcp_bind(ip, addr.port())
            .map_err(super::toyos_stream::net_err_to_io)?;

        let bound_addr = SocketAddr::from((ip, bound.bound_port));

        Ok(TcpListener {
            socket_id: bound.socket_id,
            notify_fd: bound.notify.into_fd(),
            local_addr: bound_addr,
        })
    }

    /// Accept a new connection.
    ///
    /// Returns `WouldBlock` if no connections are pending.
    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        // Try to read a notification byte (non-blocking)
        let mut byte = [0u8; 1];
        match syscall::read_nonblock(self.notify_fd, &mut byte) {
            Err(toyos_abi::syscall::SyscallError::WouldBlock) => {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            Err(e) => {
                return Err(io::Error::new(io::ErrorKind::Other, e.to_string()));
            }
            Ok(_) => {} // notification received, proceed with accept
        }

        let accepted = toyos::net::tcp_accept(self.socket_id)
            .map_err(super::toyos_stream::net_err_to_io)?;

        let peer_addr = SocketAddr::from((accepted.remote_addr, accepted.remote_port));
        let stream = TcpStream::from_accepted(accepted);

        Ok((stream, peer_addr))
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    pub fn set_ttl(&self, _ttl: u32) -> io::Result<()> {
        Ok(())
    }

    pub fn ttl(&self) -> io::Result<u32> {
        Ok(64)
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        Ok(None)
    }
}

impl event::Source for TcpListener {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        let _ = interests;
        registry
            .selector()
            .register_fd(self.notify_fd, token, Interest::READABLE)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        let _ = interests;
        registry
            .selector()
            .reregister_fd(self.notify_fd, token, Interest::READABLE)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        registry.selector().deregister_fd(self.notify_fd)
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        let _ = toyos::net::tcp_close(self.socket_id);
        syscall::close(self.notify_fd);
    }
}

impl fmt::Debug for TcpListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpListener")
            .field("socket_id", &self.socket_id)
            .field("local_addr", &self.local_addr)
            .finish()
    }
}

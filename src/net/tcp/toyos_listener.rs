use std::fmt;
use std::io;
use std::net::SocketAddr;

use crate::net::TcpStream;
use crate::{event, Interest, Registry, Token};
use toyos_abi::syscall::{self, Fd};

/// A TCP listener backed by kernel pipes via netd.
///
/// netd writes a byte to the notify pipe when a new connection arrives.
/// Polling the notify_fd for readability indicates a connection is ready to accept.
pub struct TcpListener {
    socket_id: u32,
    notify_fd: u64,
    local_addr: SocketAddr,
}

impl TcpListener {
    /// Bind a TCP listener to the given address.
    pub fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
        use toyos_abi::net::*;

        let netd_pid = super::toyos_stream::find_netd()?;

        // Create notify pipe for connection arrival notifications
        let notify_pipe = syscall::pipe();
        let notify_pipe_id = syscall::pipe_id(notify_pipe.write)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        let ip = match addr {
            SocketAddr::V4(v4) => v4.ip().octets(),
            SocketAddr::V6(_) => {
                return Err(io::Error::new(io::ErrorKind::Unsupported, "IPv6 not supported"));
            }
        };

        let req = TcpBindPipedRequest {
            addr: ip,
            port: addr.port(),
            _pad: 0,
            notify_pipe_id,
        };

        super::toyos_stream::send_netd_msg(netd_pid, MSG_TCP_BIND_PIPED, &req)?;
        let resp: TcpBindResponse = super::toyos_stream::recv_netd_response()?;

        // Close write end — netd opened it via pipe_open
        syscall::close(notify_pipe.write);

        let bound_addr = SocketAddr::from((ip, resp.bound_port));

        Ok(TcpListener {
            socket_id: resp.socket_id,
            notify_fd: notify_pipe.read.0,
            local_addr: bound_addr,
        })
    }

    /// Accept a new connection.
    ///
    /// Returns `WouldBlock` if no connections are pending.
    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        use toyos_abi::net::*;

        // Try to read a notification byte (non-blocking)
        let mut byte = [0u8; 1];
        match syscall::read_nonblock(Fd(self.notify_fd), &mut byte) {
            Err(toyos_abi::syscall::SyscallError::WouldBlock) => {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            Err(e) => {
                return Err(io::Error::new(io::ErrorKind::Other, e.to_string()));
            }
            Ok(_) => {} // notification received, proceed with accept
        }

        let netd_pid = super::toyos_stream::find_netd()?;

        // Create pipes for the new connection
        let rx_pipe = syscall::pipe_with_capacity(65536);
        let tx_pipe = syscall::pipe_with_capacity(65536);

        let rx_pipe_id = syscall::pipe_id(rx_pipe.write)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let tx_pipe_id = syscall::pipe_id(tx_pipe.read)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        let req = TcpAcceptPipedRequest {
            socket_id: self.socket_id,
            _pad: 0,
            rx_pipe_id,
            tx_pipe_id,
        };

        super::toyos_stream::send_netd_msg(netd_pid, MSG_TCP_ACCEPT_PIPED, &req)?;
        let resp: TcpAcceptPipedResponse = super::toyos_stream::recv_netd_response()?;

        // Close pipe ends we don't use
        syscall::close(rx_pipe.write);
        syscall::close(tx_pipe.read);

        let peer_addr = SocketAddr::from((resp.remote_addr, resp.remote_port));

        let stream = TcpStream::from_piped(
            rx_pipe.read.0,
            tx_pipe.write.0,
            peer_addr,
            resp.local_port,
            resp.socket_id,
        );

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
        // Only readable interest makes sense for a listener (connection ready)
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
        // Close the listener socket via netd
        use toyos_abi::net::*;
        if let Ok(netd_pid) = super::toyos_stream::find_netd() {
            let req = TcpCloseRequest {
                socket_id: self.socket_id,
            };
            let _ = super::toyos_stream::send_netd_msg(netd_pid, MSG_TCP_CLOSE, &req);
            let _ = super::toyos_stream::recv_netd_response::<[u8; 0]>();
        }
        syscall::close(Fd(self.notify_fd));
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

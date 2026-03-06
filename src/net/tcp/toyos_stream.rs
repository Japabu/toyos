use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr};

use crate::{event, Interest, Registry, Token};
use toyos_abi::syscall::{self, Fd, SyscallError};

/// A non-blocking TCP stream backed by kernel pipes via netd.
pub struct TcpStream {
    rx_fd: u64,
    tx_fd: u64,
    peer_addr: SocketAddr,
    local_port: u16,
    socket_id: u32,
}

impl TcpStream {
    /// Issue a non-blocking connect to the specified address via netd.
    pub fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
        use toyos_abi::net::*;

        let netd_pid = find_netd()?;

        // Create pipes for rx (netd->client) and tx (client->netd)
        let rx_pipe = syscall::pipe_with_capacity(65536);
        let tx_pipe = syscall::pipe_with_capacity(65536);

        let rx_pipe_id = syscall::pipe_id(rx_pipe.write)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let tx_pipe_id = syscall::pipe_id(tx_pipe.read)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        let ip = match addr {
            SocketAddr::V4(v4) => v4.ip().octets(),
            SocketAddr::V6(_) => {
                return Err(io::Error::new(io::ErrorKind::Unsupported, "IPv6 not supported"));
            }
        };

        let req = TcpConnectPipedRequest {
            addr: ip,
            port: addr.port(),
            _pad: 0,
            timeout_ms: 30000,
            _pad2: 0,
            rx_pipe_id,
            tx_pipe_id,
        };

        send_netd_msg(netd_pid, MSG_TCP_CONNECT_PIPED, &req)?;
        let resp: TcpConnectResponse = recv_netd_response()?;

        // Close pipe ends we don't use (netd opened them via pipe_open)
        syscall::close(rx_pipe.write);
        syscall::close(tx_pipe.read);

        Ok(TcpStream {
            rx_fd: rx_pipe.read.0,
            tx_fd: tx_pipe.write.0,
            peer_addr: addr,
            local_port: resp.local_port,
            socket_id: resp.socket_id,
        })
    }

    /// Create a TcpStream from pre-existing pipe FDs (used by TcpListener::accept).
    pub(crate) fn from_piped(
        rx_fd: u64,
        tx_fd: u64,
        peer_addr: SocketAddr,
        local_port: u16,
        socket_id: u32,
    ) -> TcpStream {
        TcpStream {
            rx_fd,
            tx_fd,
            peer_addr,
            local_port,
            socket_id,
        }
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.peer_addr)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::from(([0, 0, 0, 0], self.local_port)))
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        use toyos_abi::net::*;

        let netd_pid = find_netd()?;
        let how_val: u32 = match how {
            Shutdown::Read => 0,
            Shutdown::Write => 1,
            Shutdown::Both => 2,
        };
        let req = TcpShutdownRequest {
            socket_id: self.socket_id,
            how: how_val,
        };
        send_netd_msg(netd_pid, MSG_TCP_SHUTDOWN, &req)?;
        let _: [u8; 0] = recv_netd_response()?;
        Ok(())
    }

    pub fn set_nodelay(&self, _nodelay: bool) -> io::Result<()> {
        Ok(()) // No-op: smoltcp doesn't implement Nagle
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        Ok(true)
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

    pub fn peek(&self, _buf: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "peek not supported"))
    }

    pub fn try_io<F, T>(&self, f: F) -> io::Result<T>
    where
        F: FnOnce() -> io::Result<T>,
    {
        f()
    }
}

impl Read for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match syscall::read_nonblock(Fd(self.rx_fd), buf) {
            Ok(0) => Ok(0), // EOF
            Ok(n) => Ok(n),
            Err(SyscallError::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
        }
    }
}

impl Read for &'_ TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match syscall::read_nonblock(Fd(self.rx_fd), buf) {
            Ok(0) => Ok(0),
            Ok(n) => Ok(n),
            Err(SyscallError::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
        }
    }
}

impl Write for TcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match syscall::write_nonblock(Fd(self.tx_fd), buf) {
            Ok(n) => Ok(n),
            Err(SyscallError::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for &'_ TcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match syscall::write_nonblock(Fd(self.tx_fd), buf) {
            Ok(n) => Ok(n),
            Err(SyscallError::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl event::Source for TcpStream {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        let sel = registry.selector();
        if interests.is_readable() {
            sel.register_fd(self.rx_fd, token, Interest::READABLE)?;
        }
        if interests.is_writable() {
            sel.register_fd(self.tx_fd, token, Interest::WRITABLE)?;
        }
        Ok(())
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        let sel = registry.selector();
        // Remove old registrations
        sel.deregister_fd(self.rx_fd)?;
        sel.deregister_fd(self.tx_fd)?;
        // Re-add with new interests
        if interests.is_readable() {
            sel.register_fd(self.rx_fd, token, Interest::READABLE)?;
        }
        if interests.is_writable() {
            sel.register_fd(self.tx_fd, token, Interest::WRITABLE)?;
        }
        Ok(())
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        let sel = registry.selector();
        sel.deregister_fd(self.rx_fd)?;
        sel.deregister_fd(self.tx_fd)?;
        Ok(())
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        syscall::close(Fd(self.rx_fd));
        syscall::close(Fd(self.tx_fd));
    }
}

impl fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpStream")
            .field("rx_fd", &self.rx_fd)
            .field("tx_fd", &self.tx_fd)
            .field("peer_addr", &self.peer_addr)
            .finish()
    }
}

// --- netd IPC helpers (pub(crate) for use by toyos_listener) ---

pub(crate) fn find_netd() -> io::Result<u64> {
    syscall::find_pid("netd")
        .map(|pid| pid.0 as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "netd not found"))
}

pub(crate) fn send_netd_msg<T: Copy>(netd_pid: u64, msg_type: u32, payload: &T) -> io::Result<()> {
    let msg = toyos_abi::message::Message::new(msg_type, payload);
    toyos_abi::message::send(netd_pid, msg);
    Ok(())
}

pub(crate) fn recv_netd_response<T: Copy>() -> io::Result<T> {
    use toyos_abi::net::*;

    let msg = toyos_abi::message::recv();

    if msg.msg_type == MSG_ERROR {
        let error_code: ErrorResponse = msg.take_payload();
        let kind = match error_code.code {
            ERR_CONNECTION_REFUSED => io::ErrorKind::ConnectionRefused,
            ERR_CONNECTION_RESET => io::ErrorKind::ConnectionReset,
            ERR_TIMED_OUT => io::ErrorKind::TimedOut,
            ERR_ADDR_IN_USE => io::ErrorKind::AddrInUse,
            ERR_NOT_CONNECTED => io::ErrorKind::NotConnected,
            ERR_INVALID_INPUT => io::ErrorKind::InvalidInput,
            _ => io::ErrorKind::Other,
        };
        return Err(io::Error::new(kind, "netd error"));
    }

    if msg.msg_type != MSG_RESULT {
        msg.free_payload();
        return Err(io::Error::new(io::ErrorKind::Other, "unexpected netd response"));
    }

    Ok(msg.take_payload())
}

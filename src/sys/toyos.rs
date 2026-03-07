// Copyright 2015 The Rust Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! ToyOS platform implementation for socket2.
//!
//! ToyOS uses a microkernel architecture where networking is handled by
//! the `netd` daemon via IPC message passing and kernel pipes. This module
//! implements socket2's sys interface by translating socket operations into
//! netd IPC calls.

use std::collections::HashMap;
use std::io::{self, IoSlice};
use std::marker::PhantomData;
use std::mem::{self, MaybeUninit};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use toyos_abi::syscall::{self, Fd, SyscallError};
use toyos_net::{self, NetError};

use crate::{SockAddr, TcpKeepalive};

// ---------------------------------------------------------------------------
// Public type aliases
// ---------------------------------------------------------------------------

pub(crate) use std::ffi::c_int;

pub(crate) type Bool = c_int;
pub(crate) type RawSocket = c_int;
pub(crate) type socklen_t = u32;
pub(crate) type sa_family_t = u16;

// ---------------------------------------------------------------------------
// Address family constants
// ---------------------------------------------------------------------------

pub(crate) const AF_INET: c_int = 2;
pub(crate) const AF_INET6: c_int = 10;
pub(crate) const AF_UNIX: c_int = 1;

// ---------------------------------------------------------------------------
// Socket type constants
// ---------------------------------------------------------------------------

pub(crate) const SOCK_STREAM: c_int = 1;
pub(crate) const SOCK_DGRAM: c_int = 2;

// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

pub(crate) const IPPROTO_IP: c_int = 0;
pub(crate) const IPPROTO_TCP: c_int = 6;
pub(crate) const IPPROTO_UDP: c_int = 17;
pub(crate) const IPPROTO_IPV6: c_int = 41;
pub(crate) const IPPROTO_ICMP: c_int = 1;
pub(crate) const IPPROTO_ICMPV6: c_int = 58;

// ---------------------------------------------------------------------------
// Socket-level option constants
// ---------------------------------------------------------------------------

pub(crate) const SOL_SOCKET: c_int = 1;
pub(crate) const SO_BROADCAST: c_int = 6;
pub(crate) const SO_ERROR: c_int = 4;
pub(crate) const SO_KEEPALIVE: c_int = 9;
pub(crate) const SO_LINGER: c_int = 13;
pub(crate) const SO_RCVBUF: c_int = 8;
pub(crate) const SO_RCVTIMEO: c_int = 20;
pub(crate) const SO_REUSEADDR: c_int = 2;
pub(crate) const SO_SNDBUF: c_int = 7;
pub(crate) const SO_SNDTIMEO: c_int = 21;
pub(crate) const SO_TYPE: c_int = 3;
pub(crate) const SO_OOBINLINE: c_int = 10;

// ---------------------------------------------------------------------------
// TCP option constants
// ---------------------------------------------------------------------------

pub(crate) const TCP_NODELAY: c_int = 1;

// ---------------------------------------------------------------------------
// IP option constants
// ---------------------------------------------------------------------------

pub(crate) const IP_TTL: c_int = 2;
pub(crate) const IP_TOS: c_int = 1;
pub(crate) const IP_MULTICAST_TTL: c_int = 33;
pub(crate) const IP_MULTICAST_LOOP: c_int = 34;
pub(crate) const IP_ADD_MEMBERSHIP: c_int = 35;
pub(crate) const IP_DROP_MEMBERSHIP: c_int = 36;
pub(crate) const IP_MULTICAST_IF: c_int = 32;

// ---------------------------------------------------------------------------
// IPv6 option constants
// ---------------------------------------------------------------------------

pub(crate) const IPV6_UNICAST_HOPS: c_int = 16;
pub(crate) const IPV6_V6ONLY: c_int = 26;
pub(crate) const IPV6_MULTICAST_LOOP: c_int = 19;
pub(crate) const IPV6_MULTICAST_HOPS: c_int = 18;
pub(crate) const IPV6_MULTICAST_IF: c_int = 17;
pub(crate) const IPV6_ADD_MEMBERSHIP: c_int = 20;
pub(crate) const IPV6_DROP_MEMBERSHIP: c_int = 21;

// ---------------------------------------------------------------------------
// Message flag constants
// ---------------------------------------------------------------------------

pub(crate) const MSG_PEEK: c_int = 2;
pub(crate) const MSG_OOB: c_int = 1;
pub(crate) const MSG_TRUNC: c_int = 0x20;

// ---------------------------------------------------------------------------
// Struct types (layout-compatible with POSIX, no libc dependency)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct in_addr {
    pub s_addr: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct sockaddr_in {
    pub sin_family: sa_family_t,
    pub sin_port: u16,
    pub sin_addr: in_addr,
    pub sin_zero: [u8; 8],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct in6_addr {
    pub s6_addr: [u8; 16],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct sockaddr_in6 {
    pub sin6_family: sa_family_t,
    pub sin6_port: u16,
    pub sin6_flowinfo: u32,
    pub sin6_addr: in6_addr,
    pub sin6_scope_id: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct sockaddr_storage {
    pub ss_family: sa_family_t,
    _padding: [u8; 126],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct linger {
    pub l_onoff: c_int,
    pub l_linger: c_int,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct IpMreq {
    pub imr_multiaddr: in_addr,
    pub imr_interface: in_addr,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct Ipv6Mreq {
    pub ipv6mr_multiaddr: in6_addr,
    pub ipv6mr_interface: u32,
}

// ---------------------------------------------------------------------------
// MaybeUninitSlice (no libc iovec, just a pointer+len pair)
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct MaybeUninitSlice<'a> {
    ptr: *mut u8,
    len: usize,
    _lifetime: PhantomData<&'a mut [MaybeUninit<u8>]>,
}

unsafe impl<'a> Send for MaybeUninitSlice<'a> {}
unsafe impl<'a> Sync for MaybeUninitSlice<'a> {}

impl<'a> MaybeUninitSlice<'a> {
    pub(crate) fn new(buf: &'a mut [MaybeUninit<u8>]) -> MaybeUninitSlice<'a> {
        MaybeUninitSlice {
            ptr: buf.as_mut_ptr().cast(),
            len: buf.len(),
            _lifetime: PhantomData,
        }
    }

    pub(crate) fn as_slice(&self) -> &[MaybeUninit<u8>] {
        unsafe { std::slice::from_raw_parts(self.ptr.cast(), self.len) }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [MaybeUninit<u8>] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.cast(), self.len) }
    }
}

// ---------------------------------------------------------------------------
// msghdr (POSIX-compatible layout for MsgHdr/MsgHdrMut in lib.rs)
// ---------------------------------------------------------------------------

#[repr(C)]
pub(crate) struct msghdr {
    pub msg_name: *mut u8,
    pub msg_namelen: u32,
    _pad0: u32,
    pub msg_iov: *mut IoSlice<'static>,
    pub msg_iovlen: usize,
    pub msg_control: *mut u8,
    pub msg_controllen: usize,
    pub msg_flags: c_int,
}

unsafe impl Send for msghdr {}
unsafe impl Sync for msghdr {}

pub(crate) fn set_msghdr_name(msg: &mut msghdr, name: &crate::SockAddr) {
    msg.msg_name = name.as_ptr() as *mut _;
    msg.msg_namelen = name.len();
}

pub(crate) fn set_msghdr_iov(msg: &mut msghdr, ptr: *mut IoSlice<'static>, len: usize) {
    msg.msg_iov = ptr;
    msg.msg_iovlen = len;
}

pub(crate) fn set_msghdr_control(msg: &mut msghdr, ptr: *mut u8, len: usize) {
    msg.msg_control = ptr;
    msg.msg_controllen = len;
}

pub(crate) fn set_msghdr_flags(msg: &mut msghdr, flags: c_int) {
    msg.msg_flags = flags;
}

pub(crate) fn msghdr_flags(msg: &msghdr) -> crate::RecvFlags {
    crate::RecvFlags(msg.msg_flags)
}

pub(crate) fn msghdr_control_len(msg: &msghdr) -> usize {
    msg.msg_controllen
}

// ---------------------------------------------------------------------------
// RecvFlags Debug
// ---------------------------------------------------------------------------

impl std::fmt::Debug for crate::RecvFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecvFlags")
            .field("truncated", &self.is_truncated())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// impl_debug macros for Domain, Type, Protocol
// ---------------------------------------------------------------------------

impl std::fmt::Debug for crate::Domain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            AF_INET => f.write_str("AF_INET"),
            AF_INET6 => f.write_str("AF_INET6"),
            n => write!(f, "{n}"),
        }
    }
}

impl std::fmt::Debug for crate::Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            SOCK_STREAM => f.write_str("SOCK_STREAM"),
            SOCK_DGRAM => f.write_str("SOCK_DGRAM"),
            n => write!(f, "{n}"),
        }
    }
}

impl std::fmt::Debug for crate::Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            IPPROTO_TCP => f.write_str("IPPROTO_TCP"),
            IPPROTO_UDP => f.write_str("IPPROTO_UDP"),
            n => write!(f, "{n}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Internal state machine
// ---------------------------------------------------------------------------

static NEXT_FD: AtomicI32 = AtomicI32::new(1000);

fn sockets() -> &'static Mutex<HashMap<c_int, SocketState>> {
    static SOCKETS: OnceLock<Mutex<HashMap<c_int, SocketState>>> = OnceLock::new();
    SOCKETS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn alloc_fd() -> c_int {
    NEXT_FD.fetch_add(1, Ordering::Relaxed)
}

enum SocketState {
    /// Created but not yet connected or bound.
    Unconnected {
        domain: c_int,
        sock_type: c_int,
        nonblocking: bool,
        nodelay: bool,
        reuse_addr: bool,
        recv_timeout: Option<Duration>,
        send_timeout: Option<Duration>,
        bind_addr: Option<([u8; 4], u16)>,
    },
    /// TCP stream (connected). kernel_fd is a real kernel Socket descriptor.
    Connected {
        kernel_fd: Fd,
        socket_id: u32,
        local_port: u16,
        peer_addr: [u8; 4],
        peer_port: u16,
        nonblocking: bool,
        nodelay: bool,
        recv_timeout: Option<Duration>,
        send_timeout: Option<Duration>,
    },
    /// TCP listener (bound + listening). kernel_fd is the notify pipe read end.
    Listening {
        kernel_fd: Fd,
        socket_id: u32,
        local_addr: [u8; 4],
        local_port: u16,
        nonblocking: bool,
    },
}

fn net_err_to_io(e: NetError) -> io::Error {
    let kind = match e {
        NetError::ConnectionRefused => io::ErrorKind::ConnectionRefused,
        NetError::ConnectionReset => io::ErrorKind::ConnectionReset,
        NetError::TimedOut => io::ErrorKind::TimedOut,
        NetError::AddrInUse => io::ErrorKind::AddrInUse,
        NetError::NotConnected => io::ErrorKind::NotConnected,
        NetError::InvalidInput => io::ErrorKind::InvalidInput,
        NetError::NetdNotFound => io::ErrorKind::NotConnected,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, "netd error")
}

// ---------------------------------------------------------------------------
// Address extraction from SockAddr
// ---------------------------------------------------------------------------

fn sockaddr_to_v4(addr: &SockAddr) -> io::Result<([u8; 4], u16)> {
    if addr.family() != AF_INET as sa_family_t {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only IPv4 is supported",
        ));
    }
    let sin = unsafe { &*(addr.as_ptr() as *const sockaddr_in) };
    let ip = sin.sin_addr.s_addr.to_ne_bytes();
    let port = u16::from_be(sin.sin_port);
    Ok((ip, port))
}

fn make_sockaddr_v4(ip: [u8; 4], port: u16) -> SockAddr {
    let addr = std::net::SocketAddrV4::new(Ipv4Addr::from(ip), port);
    SockAddr::from(std::net::SocketAddr::V4(addr))
}

// ---------------------------------------------------------------------------
// Socket type (owned handle with Drop)
// ---------------------------------------------------------------------------

pub(crate) struct Socket(c_int);

impl Drop for Socket {
    fn drop(&mut self) {
        if let Some(state) = sockets().lock().unwrap().remove(&self.0) {
            match state {
                SocketState::Connected { kernel_fd, socket_id, .. }
                | SocketState::Listening { kernel_fd, socket_id, .. } => {
                    toyos_net::tcp_close(socket_id);
                    syscall::close(kernel_fd);
                }
                SocketState::Unconnected { .. } => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Raw socket handle functions
// ---------------------------------------------------------------------------

pub(crate) unsafe fn socket_from_raw(fd: RawSocket) -> Socket {
    Socket(fd)
}

pub(crate) fn socket_as_raw(socket: &Socket) -> RawSocket {
    socket.0
}

pub(crate) fn socket_into_raw(socket: Socket) -> RawSocket {
    let fd = socket.0;
    mem::forget(socket);
    fd
}

// ---------------------------------------------------------------------------
// Core socket operations
// ---------------------------------------------------------------------------

pub(crate) fn socket(family: c_int, ty: c_int, _protocol: c_int) -> io::Result<RawSocket> {
    let fd = alloc_fd();
    sockets().lock().unwrap().insert(
        fd,
        SocketState::Unconnected {
            domain: family,
            sock_type: ty,
            nonblocking: false,
            nodelay: false,
            reuse_addr: false,
            recv_timeout: None,
            send_timeout: None,
            bind_addr: None,
        },
    );
    Ok(fd)
}

pub(crate) fn bind(fd: RawSocket, addr: &SockAddr) -> io::Result<()> {
    let (ip, port) = sockaddr_to_v4(addr)?;
    let mut map = sockets().lock().unwrap();
    let state = map
        .get_mut(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;
    match state {
        SocketState::Unconnected { bind_addr, .. } => {
            *bind_addr = Some((ip, port));
            Ok(())
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket already connected or listening",
        )),
    }
}

pub(crate) fn connect(fd: RawSocket, addr: &SockAddr) -> io::Result<()> {
    let (ip, port) = sockaddr_to_v4(addr)?;

    let conn = toyos_net::tcp_connect(ip, port, 30000).map_err(net_err_to_io)?;

    // Wrap pipe fds into a kernel socket descriptor
    let rx_pipe_id = syscall::pipe_id(conn.rx_fd)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("pipe_id: {e:?}")))?;
    let tx_pipe_id = syscall::pipe_id(conn.tx_fd)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("pipe_id: {e:?}")))?;
    let kernel_fd = syscall::socket_create(rx_pipe_id, tx_pipe_id)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("socket_create: {e:?}")))?;

    // Close raw pipe fds — socket descriptor holds the refcounts
    syscall::close(conn.rx_fd);
    syscall::close(conn.tx_fd);

    let mut map = sockets().lock().unwrap();
    let old = map
        .get(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;

    let (nonblocking, nodelay, recv_timeout, send_timeout) = match old {
        SocketState::Unconnected {
            nonblocking,
            nodelay,
            recv_timeout,
            send_timeout,
            ..
        } => (*nonblocking, *nodelay, *recv_timeout, *send_timeout),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket already connected",
            ))
        }
    };

    map.insert(
        fd,
        SocketState::Connected {
            kernel_fd,
            socket_id: conn.socket_id,
            local_port: conn.local_port,
            peer_addr: ip,
            peer_port: port,
            nonblocking,
            nodelay,
            recv_timeout,
            send_timeout,
        },
    );

    // Apply queued nodelay option
    if nodelay {
        let _ = toyos_net::tcp_set_option(conn.socket_id, toyos_net::OPT_NODELAY, 1);
    }

    Ok(())
}

pub(crate) fn poll_connect(socket: &crate::Socket, _timeout: Duration) -> io::Result<()> {
    // On ToyOS, connect is synchronous — if we're Connected, it succeeded.
    let map = sockets().lock().unwrap();
    match map.get(&socket.as_raw()) {
        Some(SocketState::Connected { .. }) => Ok(()),
        _ => Err(io::ErrorKind::NotConnected.into()),
    }
}

pub(crate) fn listen(fd: RawSocket, _backlog: c_int) -> io::Result<()> {
    let mut map = sockets().lock().unwrap();
    let state = map
        .get(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;

    let (ip, port, nonblocking) = match state {
        SocketState::Unconnected {
            bind_addr,
            nonblocking,
            ..
        } => {
            let (ip, port) = bind_addr.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "socket not bound")
            })?;
            (ip, port, *nonblocking)
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket already connected or listening",
            ))
        }
    };

    let bound = toyos_net::tcp_bind(ip, port).map_err(net_err_to_io)?;

    map.insert(
        fd,
        SocketState::Listening {
            kernel_fd: bound.notify_fd,
            socket_id: bound.socket_id,
            local_addr: ip,
            local_port: bound.bound_port,
            nonblocking,
        },
    );

    Ok(())
}

pub(crate) fn accept(fd: RawSocket) -> io::Result<(RawSocket, SockAddr)> {
    let map = sockets().lock().unwrap();
    let state = map
        .get(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;

    let (socket_id, kernel_fd, nonblocking) = match state {
        SocketState::Listening {
            socket_id,
            kernel_fd,
            nonblocking,
            ..
        } => (*socket_id, *kernel_fd, *nonblocking),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket not listening",
            ))
        }
    };
    drop(map);

    // Wait for a connection notification
    let mut buf = [0u8; 1];
    if nonblocking {
        match syscall::read_nonblock(kernel_fd, &mut buf) {
            Ok(_) => {}
            Err(SyscallError::WouldBlock) => return Err(io::ErrorKind::WouldBlock.into()),
            Err(e) => return Err(io::Error::new(io::ErrorKind::Other, format!("{e:?}"))),
        }
    } else {
        match syscall::read(kernel_fd, &mut buf) {
            Ok(_) => {}
            Err(e) => return Err(io::Error::new(io::ErrorKind::Other, format!("{e:?}"))),
        }
    }

    let accepted = toyos_net::tcp_accept(socket_id).map_err(net_err_to_io)?;

    // Wrap pipe fds into a kernel socket descriptor
    let rx_pipe_id = syscall::pipe_id(accepted.rx_fd)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("pipe_id: {e:?}")))?;
    let tx_pipe_id = syscall::pipe_id(accepted.tx_fd)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("pipe_id: {e:?}")))?;
    let new_kernel_fd = syscall::socket_create(rx_pipe_id, tx_pipe_id)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("socket_create: {e:?}")))?;

    // Close raw pipe fds — socket descriptor holds the refcounts
    syscall::close(accepted.rx_fd);
    syscall::close(accepted.tx_fd);

    let new_fd = alloc_fd();
    sockets().lock().unwrap().insert(
        new_fd,
        SocketState::Connected {
            kernel_fd: new_kernel_fd,
            socket_id: accepted.socket_id,
            local_port: accepted.local_port,
            peer_addr: accepted.remote_addr,
            peer_port: accepted.remote_port,
            nonblocking: false,
            nodelay: false,
            recv_timeout: None,
            send_timeout: None,
        },
    );

    let peer = make_sockaddr_v4(accepted.remote_addr, accepted.remote_port);
    Ok((new_fd, peer))
}

pub(crate) fn getsockname(fd: RawSocket) -> io::Result<SockAddr> {
    let map = sockets().lock().unwrap();
    let state = map
        .get(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;
    match state {
        SocketState::Connected { local_port, .. } => {
            Ok(make_sockaddr_v4([0, 0, 0, 0], *local_port))
        }
        SocketState::Listening {
            local_addr,
            local_port,
            ..
        } => Ok(make_sockaddr_v4(*local_addr, *local_port)),
        SocketState::Unconnected { bind_addr, .. } => match bind_addr {
            Some((ip, port)) => Ok(make_sockaddr_v4(*ip, *port)),
            None => Ok(make_sockaddr_v4([0, 0, 0, 0], 0)),
        },
    }
}

pub(crate) fn getpeername(fd: RawSocket) -> io::Result<SockAddr> {
    let map = sockets().lock().unwrap();
    let state = map
        .get(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;
    match state {
        SocketState::Connected {
            peer_addr,
            peer_port,
            ..
        } => Ok(make_sockaddr_v4(*peer_addr, *peer_port)),
        _ => Err(io::ErrorKind::NotConnected.into()),
    }
}

pub(crate) fn try_clone(fd: RawSocket) -> io::Result<RawSocket> {
    let map = sockets().lock().unwrap();
    let state = map
        .get(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;
    match state {
        SocketState::Connected { kernel_fd, socket_id, local_port, peer_addr, peer_port, nonblocking, nodelay, recv_timeout, send_timeout } => {
            let new_kernel_fd = syscall::dup(*kernel_fd)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("dup: {e:?}")))?;
            let new_fd = alloc_fd();
            let cloned = SocketState::Connected {
                kernel_fd: new_kernel_fd,
                socket_id: *socket_id,
                local_port: *local_port,
                peer_addr: *peer_addr,
                peer_port: *peer_port,
                nonblocking: *nonblocking,
                nodelay: *nodelay,
                recv_timeout: *recv_timeout,
                send_timeout: *send_timeout,
            };
            drop(map);
            sockets().lock().unwrap().insert(new_fd, cloned);
            Ok(new_fd)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "can only clone connected sockets",
        )),
    }
}

pub(crate) fn set_nonblocking(fd: RawSocket, nonblocking: bool) -> io::Result<()> {
    let mut map = sockets().lock().unwrap();
    let state = map
        .get_mut(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;
    match state {
        SocketState::Unconnected {
            nonblocking: nb, ..
        }
        | SocketState::Connected {
            nonblocking: nb, ..
        }
        | SocketState::Listening {
            nonblocking: nb, ..
        } => {
            *nb = nonblocking;
            Ok(())
        }
    }
}

pub(crate) fn shutdown(fd: RawSocket, how: Shutdown) -> io::Result<()> {
    let map = sockets().lock().unwrap();
    let state = map
        .get(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;
    let socket_id = match state {
        SocketState::Connected { socket_id, .. } => *socket_id,
        _ => return Err(io::ErrorKind::NotConnected.into()),
    };
    drop(map);

    let how_val = match how {
        Shutdown::Read => 0u32,
        Shutdown::Write => 1,
        Shutdown::Both => 2,
    };
    toyos_net::tcp_shutdown(socket_id, how_val).map_err(net_err_to_io)
}

// ---------------------------------------------------------------------------
// Data transfer
// ---------------------------------------------------------------------------

pub(crate) fn recv(
    fd: RawSocket,
    buf: &mut [MaybeUninit<u8>],
    _flags: c_int,
) -> io::Result<usize> {
    let map = sockets().lock().unwrap();
    let state = map
        .get(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;
    let (kernel_fd, nonblocking, recv_timeout) = match state {
        SocketState::Connected {
            kernel_fd,
            nonblocking,
            recv_timeout,
            ..
        } => (*kernel_fd, *nonblocking, *recv_timeout),
        _ => return Err(io::ErrorKind::NotConnected.into()),
    };
    drop(map);

    // SAFETY: MaybeUninit<u8> has the same layout as u8
    let raw_buf = unsafe {
        std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), buf.len())
    };

    if nonblocking {
        match syscall::read_nonblock(kernel_fd, raw_buf) {
            Ok(n) => Ok(n),
            Err(SyscallError::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, format!("{e:?}"))),
        }
    } else if let Some(timeout) = recv_timeout {
        let poll_entry = kernel_fd.0 as u64 | syscall::POLL_READABLE;
        let result = syscall::poll_timeout(&[poll_entry], Some(timeout.as_nanos() as u64));
        if result.fd(0) {
            match syscall::read(kernel_fd, raw_buf) {
                Ok(n) => Ok(n),
                Err(e) => Err(io::Error::new(io::ErrorKind::Other, format!("{e:?}"))),
            }
        } else {
            Err(io::ErrorKind::TimedOut.into())
        }
    } else {
        match syscall::read(kernel_fd, raw_buf) {
            Ok(n) => Ok(n),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, format!("{e:?}"))),
        }
    }
}

pub(crate) fn send(fd: RawSocket, buf: &[u8], _flags: c_int) -> io::Result<usize> {
    let map = sockets().lock().unwrap();
    let state = map
        .get(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;
    let (kernel_fd, nonblocking, send_timeout) = match state {
        SocketState::Connected {
            kernel_fd,
            nonblocking,
            send_timeout,
            ..
        } => (*kernel_fd, *nonblocking, *send_timeout),
        _ => return Err(io::ErrorKind::NotConnected.into()),
    };
    drop(map);

    if nonblocking {
        match syscall::write_nonblock(kernel_fd, buf) {
            Ok(n) => Ok(n),
            Err(SyscallError::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, format!("{e:?}"))),
        }
    } else if let Some(timeout) = send_timeout {
        let poll_entry = kernel_fd.0 as u64 | syscall::POLL_WRITABLE;
        let result = syscall::poll_timeout(&[poll_entry], Some(timeout.as_nanos() as u64));
        if result.fd(0) {
            match syscall::write(kernel_fd, buf) {
                Ok(n) => Ok(n),
                Err(e) => Err(io::Error::new(io::ErrorKind::Other, format!("{e:?}"))),
            }
        } else {
            Err(io::ErrorKind::TimedOut.into())
        }
    } else {
        match syscall::write(kernel_fd, buf) {
            Ok(n) => Ok(n),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, format!("{e:?}"))),
        }
    }
}

pub(crate) fn recv_from(
    fd: RawSocket,
    buf: &mut [MaybeUninit<u8>],
    flags: c_int,
) -> io::Result<(usize, SockAddr)> {
    // TCP only — recv_from returns the peer address
    let n = recv(fd, buf, flags)?;
    let peer = getpeername(fd)?;
    Ok((n, peer))
}

pub(crate) fn send_to(
    fd: RawSocket,
    buf: &[u8],
    _addr: &SockAddr,
    flags: c_int,
) -> io::Result<usize> {
    // TCP only — send_to ignores the address
    send(fd, buf, flags)
}

pub(crate) fn peek_sender(fd: RawSocket) -> io::Result<SockAddr> {
    getpeername(fd)
}

// ---------------------------------------------------------------------------
// Socket options
// ---------------------------------------------------------------------------

pub(crate) unsafe fn getsockopt<T: Copy>(
    fd: RawSocket,
    opt: c_int,
    val: c_int,
) -> io::Result<T> {
    let map = sockets().lock().unwrap();
    let state = map
        .get(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;

    // Most options return c_int (0 or 1 for booleans, actual value otherwise)
    macro_rules! ret {
        ($v:expr) => {{
            let v: c_int = $v;
            let mut result = MaybeUninit::<T>::zeroed();
            let src = &v as *const c_int as *const u8;
            let dst = result.as_mut_ptr() as *mut u8;
            let len = std::cmp::min(mem::size_of::<T>(), mem::size_of::<c_int>());
            std::ptr::copy_nonoverlapping(src, dst, len);
            return Ok(result.assume_init());
        }};
    }

    match (opt, val) {
        (SOL_SOCKET, SO_ERROR) => ret!(0),
        (SOL_SOCKET, SO_TYPE) => match state {
            SocketState::Unconnected { sock_type, .. } => ret!(*sock_type),
            SocketState::Connected { .. } | SocketState::Listening { .. } => ret!(SOCK_STREAM),
        },
        (SOL_SOCKET, SO_KEEPALIVE) => ret!(0),
        (SOL_SOCKET, SO_BROADCAST) => ret!(0),
        (SOL_SOCKET, SO_REUSEADDR) => match state {
            SocketState::Unconnected { reuse_addr, .. } => ret!(if *reuse_addr { 1 } else { 0 }),
            _ => ret!(0),
        },
        (SOL_SOCKET, SO_RCVBUF) => ret!(65536),
        (SOL_SOCKET, SO_SNDBUF) => ret!(65536),
        (SOL_SOCKET, SO_LINGER) => {
            let l = linger {
                l_onoff: 0,
                l_linger: 0,
            };
            let mut result = MaybeUninit::<T>::zeroed();
            let len = std::cmp::min(mem::size_of::<T>(), mem::size_of::<linger>());
            std::ptr::copy_nonoverlapping(
                &l as *const linger as *const u8,
                result.as_mut_ptr() as *mut u8,
                len,
            );
            return Ok(result.assume_init());
        }
        (IPPROTO_TCP, TCP_NODELAY) => match state {
            SocketState::Unconnected { nodelay, .. }
            | SocketState::Connected { nodelay, .. } => ret!(if *nodelay { 1 } else { 0 }),
            _ => ret!(0),
        },
        (IPPROTO_IP, IP_TTL) | (IPPROTO_IPV6, IPV6_UNICAST_HOPS) => ret!(64),
        (IPPROTO_IP, IP_MULTICAST_TTL) | (IPPROTO_IPV6, IPV6_MULTICAST_HOPS) => ret!(1),
        (IPPROTO_IP, IP_MULTICAST_LOOP) | (IPPROTO_IPV6, IPV6_MULTICAST_LOOP) => ret!(1),
        (IPPROTO_IPV6, IPV6_V6ONLY) => ret!(0),
        _ => ret!(0),
    }
}

pub(crate) unsafe fn setsockopt<T>(
    fd: RawSocket,
    opt: c_int,
    val: c_int,
    payload: T,
) -> io::Result<()> {
    let value = if mem::size_of::<T>() >= mem::size_of::<c_int>() {
        *(&payload as *const T as *const c_int)
    } else {
        0
    };

    let mut map = sockets().lock().unwrap();
    let state = map
        .get_mut(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;

    match (opt, val) {
        (SOL_SOCKET, SO_REUSEADDR) => {
            if let SocketState::Unconnected { reuse_addr, .. } = state {
                *reuse_addr = value != 0;
            }
        }
        (SOL_SOCKET, SO_KEEPALIVE) => {} // no-op
        (SOL_SOCKET, SO_BROADCAST) => {} // no-op
        (SOL_SOCKET, SO_RCVBUF) => {}    // no-op
        (SOL_SOCKET, SO_SNDBUF) => {}    // no-op
        (SOL_SOCKET, SO_LINGER) => {}    // no-op
        (IPPROTO_TCP, TCP_NODELAY) => {
            match state {
                SocketState::Unconnected { nodelay, .. } => *nodelay = value != 0,
                SocketState::Connected {
                    nodelay, socket_id, ..
                } => {
                    *nodelay = value != 0;
                    let sid = *socket_id;
                    drop(map);
                    let _ = toyos_net::tcp_set_option(sid, toyos_net::OPT_NODELAY, value as u32);
                    return Ok(());
                }
                _ => {}
            }
        }
        (IPPROTO_IP, IP_TTL) | (IPPROTO_IPV6, IPV6_UNICAST_HOPS) => {} // no-op
        (IPPROTO_IP, IP_MULTICAST_TTL) => {}                            // no-op
        (IPPROTO_IP, IP_MULTICAST_LOOP) | (IPPROTO_IPV6, IPV6_MULTICAST_LOOP) => {} // no-op
        (IPPROTO_IPV6, IPV6_V6ONLY) => {}                               // no-op
        (IPPROTO_IP, IP_ADD_MEMBERSHIP) | (IPPROTO_IP, IP_DROP_MEMBERSHIP) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "multicast not supported",
            ));
        }
        (IPPROTO_IPV6, IPV6_ADD_MEMBERSHIP) | (IPPROTO_IPV6, IPV6_DROP_MEMBERSHIP) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "multicast not supported",
            ));
        }
        _ => {} // unknown options are silently ignored
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Timeout helpers
// ---------------------------------------------------------------------------

pub(crate) fn timeout_opt(
    fd: RawSocket,
    _opt: c_int,
    val: c_int,
) -> io::Result<Option<Duration>> {
    let map = sockets().lock().unwrap();
    let state = map
        .get(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;

    let timeout = match state {
        SocketState::Unconnected {
            recv_timeout,
            send_timeout,
            ..
        }
        | SocketState::Connected {
            recv_timeout,
            send_timeout,
            ..
        } => {
            if val == SO_RCVTIMEO {
                *recv_timeout
            } else {
                *send_timeout
            }
        }
        _ => None,
    };
    Ok(timeout)
}

pub(crate) fn set_timeout_opt(
    fd: RawSocket,
    _opt: c_int,
    val: c_int,
    duration: Option<Duration>,
) -> io::Result<()> {
    let mut map = sockets().lock().unwrap();
    let state = map
        .get_mut(&fd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;

    match state {
        SocketState::Unconnected {
            recv_timeout,
            send_timeout,
            ..
        }
        | SocketState::Connected {
            recv_timeout,
            send_timeout,
            ..
        } => {
            if val == SO_RCVTIMEO {
                *recv_timeout = duration;
            } else {
                *send_timeout = duration;
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// TCP keepalive (no-op on ToyOS)
// ---------------------------------------------------------------------------

pub(crate) fn set_tcp_keepalive(
    _fd: RawSocket,
    _keepalive: &TcpKeepalive,
) -> io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Address conversion helpers
// ---------------------------------------------------------------------------

pub(crate) const fn to_in_addr(addr: &Ipv4Addr) -> in_addr {
    in_addr {
        s_addr: u32::from_ne_bytes(addr.octets()),
    }
}

pub(crate) fn from_in_addr(addr: in_addr) -> Ipv4Addr {
    Ipv4Addr::from(addr.s_addr.to_ne_bytes())
}

pub(crate) const fn to_in6_addr(addr: &Ipv6Addr) -> in6_addr {
    in6_addr {
        s6_addr: addr.octets(),
    }
}

pub(crate) fn from_in6_addr(addr: in6_addr) -> Ipv6Addr {
    Ipv6Addr::from(addr.s6_addr)
}

// ---------------------------------------------------------------------------
// Vectored I/O
// ---------------------------------------------------------------------------

pub(crate) fn recv_vectored(
    fd: RawSocket,
    bufs: &mut [crate::MaybeUninitSlice<'_>],
    flags: c_int,
) -> io::Result<(usize, crate::RecvFlags)> {
    let total: usize = bufs.iter().map(|b| b.len()).sum();
    let mut tmp = vec![MaybeUninit::uninit(); total];
    let n = recv(fd, &mut tmp, flags)?;
    // Scatter into iovecs
    let mut offset = 0;
    for buf in bufs.iter_mut() {
        let slice: &mut [MaybeUninit<u8>] = buf;
        let to_copy = (n - offset).min(slice.len());
        if to_copy == 0 {
            break;
        }
        slice[..to_copy].copy_from_slice(&tmp[offset..offset + to_copy]);
        offset += to_copy;
    }
    Ok((n, crate::RecvFlags(0)))
}

pub(crate) fn recv_from_vectored(
    fd: RawSocket,
    bufs: &mut [crate::MaybeUninitSlice<'_>],
    flags: c_int,
) -> io::Result<(usize, crate::RecvFlags, crate::SockAddr)> {
    let (n, rflags) = recv_vectored(fd, bufs, flags)?;
    let peer = getpeername(fd)?;
    Ok((n, rflags, peer))
}

pub(crate) fn recvmsg(
    fd: RawSocket,
    msg: &mut crate::MsgHdrMut<'_, '_, '_>,
    flags: c_int,
) -> io::Result<usize> {
    let iov_count = msg.inner.msg_iovlen;
    if iov_count == 0 {
        return Ok(0);
    }
    let bufs = unsafe {
        std::slice::from_raw_parts_mut(msg.inner.msg_iov as *mut crate::MaybeUninitSlice<'_>, iov_count)
    };
    let (n, rflags) = recv_vectored(fd, bufs, flags)?;
    msg.inner.msg_flags = rflags.0;
    Ok(n)
}

pub(crate) fn send_vectored(
    fd: RawSocket,
    bufs: &[IoSlice<'_>],
    flags: c_int,
) -> io::Result<usize> {
    if bufs.len() == 1 {
        return send(fd, &bufs[0], flags);
    }
    let total: usize = bufs.iter().map(|b| b.len()).sum();
    let mut tmp = Vec::with_capacity(total);
    for buf in bufs {
        tmp.extend_from_slice(buf);
    }
    send(fd, &tmp, flags)
}

pub(crate) fn send_to_vectored(
    fd: RawSocket,
    bufs: &[IoSlice<'_>],
    _addr: &crate::SockAddr,
    flags: c_int,
) -> io::Result<usize> {
    send_vectored(fd, bufs, flags)
}

pub(crate) fn sendmsg(
    fd: RawSocket,
    msg: &crate::MsgHdr<'_, '_, '_>,
    flags: c_int,
) -> io::Result<usize> {
    let iov_count = msg.inner.msg_iovlen;
    if iov_count == 0 {
        return Ok(0);
    }
    let bufs = unsafe { std::slice::from_raw_parts(msg.inner.msg_iov, iov_count) };
    send_vectored(fd, bufs, flags)
}

// ---------------------------------------------------------------------------
// Standard fd trait implementations
// ---------------------------------------------------------------------------

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

impl AsFd for crate::Socket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.as_raw()) }
    }
}

impl AsRawFd for crate::Socket {
    fn as_raw_fd(&self) -> RawFd {
        self.as_raw()
    }
}

impl From<crate::Socket> for OwnedFd {
    fn from(sock: crate::Socket) -> OwnedFd {
        unsafe { OwnedFd::from_raw_fd(sock.into_raw()) }
    }
}

impl IntoRawFd for crate::Socket {
    fn into_raw_fd(self) -> RawFd {
        self.into_raw()
    }
}

impl From<OwnedFd> for crate::Socket {
    fn from(fd: OwnedFd) -> crate::Socket {
        unsafe { crate::Socket::from_raw_fd(fd.into_raw_fd()) }
    }
}

impl FromRawFd for crate::Socket {
    unsafe fn from_raw_fd(fd: RawFd) -> crate::Socket {
        crate::Socket::from_raw(fd)
    }
}

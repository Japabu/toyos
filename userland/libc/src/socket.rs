// BSD sockets — TCP and UDP use pipe-backed data transfer via netd.

use alloc::alloc::{alloc as heap_alloc, dealloc as heap_dealloc};
use alloc::vec::Vec;
use core::ptr;
use toyos_abi::Fd;
use toyos_abi::syscall;
use toyos::net::{NetError, TcpSocketId, UdpSocketId, OPT_NODELAY};

// ---------------------------------------------------------------------------
// C types matching POSIX
// ---------------------------------------------------------------------------

type SocklenT = u32;

#[repr(C)]
struct SockaddrIn {
    sin_family: u16,
    sin_port: u16,   // network byte order (big-endian)
    sin_addr: InAddr,
    sin_zero: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InAddr {
    s_addr: u32, // network byte order
}

#[repr(C)]
pub struct Sockaddr {
    sa_family: u16,
    sa_data: [u8; 14],
}

#[repr(C)]
pub struct Addrinfo {
    ai_flags: i32,
    ai_family: i32,
    ai_socktype: i32,
    ai_protocol: i32,
    ai_addrlen: SocklenT,
    ai_addr: *mut Sockaddr,
    ai_canonname: *mut u8,
    ai_next: *mut Addrinfo,
}

const AF_INET: i32 = 2;
const AF_UNSPEC: i32 = 0;
const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;
const IPPROTO_TCP: i32 = 6;

const SOL_SOCKET: i32 = 1;
const SO_ERROR: i32 = 4;
const TCP_NODELAY: i32 = 1;

const EINVAL: i32 = 22;
const EBADF: i32 = 9;
const ENOMEM: i32 = 12;
const EAFNOSUPPORT: i32 = 97;
const ECONNREFUSED: i32 = 111;
const ECONNRESET: i32 = 104;
const ETIMEDOUT: i32 = 110;
const EADDRINUSE: i32 = 98;
const ENOTCONN: i32 = 107;
const EIO: i32 = 5;

// ---------------------------------------------------------------------------
// Internal socket table
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum SocketKind {
    Tcp,
    Udp,
}

#[derive(Clone, Copy)]
struct SocketEntry {
    kind: SocketKind,
    netd_id: u32,       // netd socket_id (0 = not yet connected/bound)
    connected: bool,
    bound: bool,
    local_port: u16,
    remote_addr: [u8; 4],
    remote_port: u16,
    // Pipe fds for data path (0 = not set)
    rx_fd: i32,         // read end of rx pipe (netd→client)
    tx_fd: i32,         // write end of tx pipe (client→netd)
    notify_fd: i32,     // read end of listener notify pipe
}

const MAX_SOCKETS: usize = 128;
// Socket FDs start at 1024 to avoid collisions with file FDs
const SOCKET_FD_BASE: i32 = 1024;

static mut SOCKETS: [Option<SocketEntry>; MAX_SOCKETS] = [None; MAX_SOCKETS];

fn sock_from_fd(fd: i32) -> Option<&'static mut Option<SocketEntry>> {
    let idx = (fd - SOCKET_FD_BASE) as usize;
    if idx >= MAX_SOCKETS {
        return None;
    }
    unsafe { Some(&mut SOCKETS[idx]) }
}

fn alloc_socket(entry: SocketEntry) -> i32 {
    unsafe {
        for i in 0..MAX_SOCKETS {
            if SOCKETS[i].is_none() {
                SOCKETS[i] = Some(entry);
                return SOCKET_FD_BASE + i as i32;
            }
        }
    }
    -1
}

fn set_errno(e: i32) {
    unsafe { super::stdio::errno = e; }
}

// ---------------------------------------------------------------------------
// netd error conversion
// ---------------------------------------------------------------------------

fn net_err_to_errno(e: NetError) -> i32 {
    match e {
        NetError::ConnectionRefused => ECONNREFUSED,
        NetError::ConnectionReset => ECONNRESET,
        NetError::TimedOut => ETIMEDOUT,
        NetError::AddrInUse => EADDRINUSE,
        NetError::NotConnected => ENOTCONN,
        NetError::InvalidInput => EINVAL,
        _ => EIO,
    }
}

/// Parse sockaddr_in into (ipv4_octets, port).
unsafe fn parse_sockaddr(addr: *const Sockaddr, len: SocklenT) -> Option<([u8; 4], u16)> {
    if addr.is_null() || (len as usize) < core::mem::size_of::<SockaddrIn>() {
        return None;
    }
    let sin = &*(addr as *const SockaddrIn);
    if sin.sin_family != AF_INET as u16 {
        return None;
    }
    let port = u16::from_be(sin.sin_port);
    let ip = sin.sin_addr.s_addr.to_be_bytes(); // s_addr is network order
    Some((ip, port))
}

/// Fill a sockaddr_in from ip + port.
unsafe fn fill_sockaddr(addr: *mut Sockaddr, addrlen: *mut SocklenT, ip: [u8; 4], port: u16) {
    if addr.is_null() || addrlen.is_null() {
        return;
    }
    let sin = &mut *(addr as *mut SockaddrIn);
    sin.sin_family = AF_INET as u16;
    sin.sin_port = port.to_be();
    sin.sin_addr.s_addr = u32::from_be_bytes(ip);
    sin.sin_zero = [0; 8];
    *addrlen = core::mem::size_of::<SockaddrIn>() as SocklenT;
}

// ---------------------------------------------------------------------------
// BSD socket API
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn socket(domain: i32, sock_type: i32, _protocol: i32) -> i32 {
    if domain != AF_INET && domain != AF_UNSPEC {
        set_errno(EAFNOSUPPORT);
        return -1;
    }
    let kind = match sock_type & 0xf {
        SOCK_STREAM => SocketKind::Tcp,
        SOCK_DGRAM => SocketKind::Udp,
        _ => {
            set_errno(EINVAL);
            return -1;
        }
    };
    let entry = SocketEntry {
        kind,
        netd_id: 0,
        connected: false,
        bound: false,
        local_port: 0,
        remote_addr: [0; 4],
        remote_port: 0,
        rx_fd: 0,
        tx_fd: 0,
        notify_fd: 0,
    };
    let fd = alloc_socket(entry);
    if fd < 0 {
        set_errno(ENOMEM);
    }
    fd
}

#[no_mangle]
pub unsafe extern "C" fn connect(fd: i32, addr: *const Sockaddr, addrlen: SocklenT) -> i32 {
    let slot = match sock_from_fd(fd) {
        Some(s) => s,
        None => { set_errno(EBADF); return -1; }
    };
    let entry = match slot.as_mut() {
        Some(e) => e,
        None => { set_errno(EBADF); return -1; }
    };
    let (ip, port) = match parse_sockaddr(addr, addrlen) {
        Some(v) => v,
        None => { set_errno(EINVAL); return -1; }
    };

    match entry.kind {
        SocketKind::Tcp => {
            let conn = match toyos::net::tcp_connect(ip, port, 30000) {
                Ok(c) => c,
                Err(e) => { set_errno(net_err_to_errno(e)); return -1; }
            };
            entry.netd_id = conn.socket_id.0;
            entry.local_port = conn.local_port;
            entry.remote_addr = ip;
            entry.remote_port = port;
            entry.connected = true;
            entry.rx_fd = conn.rx_fd.0;
            entry.tx_fd = conn.tx_fd.0;
            0
        }
        SocketKind::Udp => {
            entry.remote_addr = ip;
            entry.remote_port = port;
            entry.connected = true;
            0
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn bind(fd: i32, addr: *const Sockaddr, addrlen: SocklenT) -> i32 {
    let slot = match sock_from_fd(fd) {
        Some(s) => s,
        None => { set_errno(EBADF); return -1; }
    };
    let entry = match slot.as_mut() {
        Some(e) => e,
        None => { set_errno(EBADF); return -1; }
    };
    let (ip, port) = match parse_sockaddr(addr, addrlen) {
        Some(v) => v,
        None => { set_errno(EINVAL); return -1; }
    };

    match entry.kind {
        SocketKind::Tcp => {
            let bound = match toyos::net::tcp_bind(ip, port) {
                Ok(b) => b,
                Err(e) => { set_errno(net_err_to_errno(e)); return -1; }
            };
            entry.netd_id = bound.socket_id.0;
            entry.local_port = bound.bound_port;
            entry.bound = true;
            entry.notify_fd = bound.notify_fd.0;
            0
        }
        SocketKind::Udp => {
            let bound = match toyos::net::udp_bind(ip, port) {
                Ok(b) => b,
                Err(e) => { set_errno(net_err_to_errno(e)); return -1; }
            };
            entry.netd_id = bound.socket_id.0;
            entry.local_port = bound.bound_port;
            entry.bound = true;
            entry.tx_fd = bound.tx_fd.0;
            entry.rx_fd = bound.rx_fd.0;
            0
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn listen(_fd: i32, _backlog: i32) -> i32 {
    // netd handles listen implicitly via bind — no separate listen step needed
    0
}

#[no_mangle]
pub unsafe extern "C" fn accept(
    fd: i32,
    addr: *mut Sockaddr,
    addrlen: *mut SocklenT,
) -> i32 {
    let slot = match sock_from_fd(fd) {
        Some(s) => s,
        None => { set_errno(EBADF); return -1; }
    };
    let entry = match slot.as_ref() {
        Some(e) => e,
        None => { set_errno(EBADF); return -1; }
    };
    let listener_id = TcpSocketId(entry.netd_id);
    let notify_fd = entry.notify_fd;

    // Block until a connection arrives (read 1 byte from notify pipe)
    let mut notify_byte = [0u8; 1];
    let _ = syscall::read(Fd(notify_fd), &mut notify_byte);

    let accepted = match toyos::net::tcp_accept(listener_id) {
        Ok(a) => a,
        Err(e) => { set_errno(net_err_to_errno(e)); return -1; }
    };

    if !addr.is_null() && !addrlen.is_null() {
        fill_sockaddr(addr, addrlen, accepted.remote_addr, accepted.remote_port);
    }

    let new_entry = SocketEntry {
        kind: SocketKind::Tcp,
        netd_id: accepted.socket_id.0,
        connected: true,
        bound: false,
        local_port: accepted.local_port,
        remote_addr: accepted.remote_addr,
        remote_port: accepted.remote_port,
        rx_fd: accepted.rx_fd.0,
        tx_fd: accepted.tx_fd.0,
        notify_fd: 0,
    };
    let new_fd = alloc_socket(new_entry);
    if new_fd < 0 {
        syscall::close(accepted.rx_fd);
        syscall::close(accepted.tx_fd);
        let _ = toyos::net::tcp_close(accepted.socket_id);
        set_errno(ENOMEM);
    }
    new_fd
}

#[no_mangle]
pub unsafe extern "C" fn send(fd: i32, buf: *const u8, len: usize, _flags: i32) -> isize {
    if buf.is_null() || len == 0 {
        return 0;
    }
    let slot = match sock_from_fd(fd) {
        Some(s) => s,
        None => { set_errno(EBADF); return -1; }
    };
    let entry = match slot.as_ref() {
        Some(e) => e,
        None => { set_errno(EBADF); return -1; }
    };

    match entry.kind {
        SocketKind::Tcp => {
            let data = core::slice::from_raw_parts(buf, len);
            match syscall::write(Fd(entry.tx_fd), data) {
                Ok(n) => n as isize,
                Err(_) => { set_errno(EIO); -1 }
            }
        }
        SocketKind::Udp => {
            if !entry.connected {
                set_errno(ENOTCONN);
                return -1;
            }
            sendto(fd, buf, len, _flags,
                &make_sockaddr_in(entry.remote_addr, entry.remote_port) as *const SockaddrIn as *const Sockaddr,
                core::mem::size_of::<SockaddrIn>() as SocklenT)
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn recv(fd: i32, buf: *mut u8, len: usize, _flags: i32) -> isize {
    if buf.is_null() || len == 0 {
        return 0;
    }
    let slot = match sock_from_fd(fd) {
        Some(s) => s,
        None => { set_errno(EBADF); return -1; }
    };
    let entry = match slot.as_ref() {
        Some(e) => e,
        None => { set_errno(EBADF); return -1; }
    };

    match entry.kind {
        SocketKind::Tcp => {
            let data = core::slice::from_raw_parts_mut(buf, len);
            match syscall::read(Fd(entry.rx_fd), data) {
                Ok(n) => n as isize,
                Err(_) => { set_errno(EIO); -1 }
            }
        }
        SocketKind::Udp => {
            recvfrom(fd, buf, len, _flags, ptr::null_mut(), ptr::null_mut())
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sendto(
    fd: i32,
    buf: *const u8,
    len: usize,
    _flags: i32,
    dest_addr: *const Sockaddr,
    addrlen: SocklenT,
) -> isize {
    if buf.is_null() || len == 0 {
        return 0;
    }
    let slot = match sock_from_fd(fd) {
        Some(s) => s,
        None => { set_errno(EBADF); return -1; }
    };
    let entry = match slot.as_ref() {
        Some(e) => e,
        None => { set_errno(EBADF); return -1; }
    };

    match entry.kind {
        SocketKind::Tcp => {
            // TCP sendto ignores address, just send
            send(fd, buf, len, _flags)
        }
        SocketKind::Udp => {
            let (ip, port) = match parse_sockaddr(dest_addr, addrlen) {
                Some(v) => v,
                None => { set_errno(EINVAL); return -1; }
            };
            // Write data to tx pipe
            let data = core::slice::from_raw_parts(buf, len);
            if let Err(_) = syscall::write(Fd(entry.tx_fd), data) {
                set_errno(EIO);
                return -1;
            }
            // Send control message with metadata
            match toyos::net::udp_send_to(UdpSocketId(entry.netd_id), ip, port, len as u16) {
                Ok(sent) => sent as isize,
                Err(e) => { set_errno(net_err_to_errno(e)); -1 }
            }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn recvfrom(
    fd: i32,
    buf: *mut u8,
    len: usize,
    _flags: i32,
    src_addr: *mut Sockaddr,
    addrlen: *mut SocklenT,
) -> isize {
    if buf.is_null() || len == 0 {
        return 0;
    }
    let slot = match sock_from_fd(fd) {
        Some(s) => s,
        None => { set_errno(EBADF); return -1; }
    };
    let entry = match slot.as_ref() {
        Some(e) => e,
        None => { set_errno(EBADF); return -1; }
    };

    match entry.kind {
        SocketKind::Tcp => {
            // TCP recvfrom ignores address, just recv
            recv(fd, buf, len, _flags)
        }
        SocketKind::Udp => {
            // Send control request and get metadata response
            let recv_resp = match toyos::net::udp_recv_from(UdpSocketId(entry.netd_id), len as u32) {
                Ok(r) => r,
                Err(e) => { set_errno(net_err_to_errno(e)); return -1; }
            };

            if !src_addr.is_null() && !addrlen.is_null() {
                fill_sockaddr(src_addr, addrlen, recv_resp.addr, recv_resp.port);
            }

            let n = (recv_resp.len as usize).min(len);
            if n > 0 {
                // Read data from rx pipe
                let data = core::slice::from_raw_parts_mut(buf, n);
                match syscall::read(Fd(entry.rx_fd), data) {
                    Ok(bytes_read) => bytes_read as isize,
                    Err(_) => { set_errno(EIO); -1 }
                }
            } else {
                0
            }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn shutdown(fd: i32, how: i32) -> i32 {
    let slot = match sock_from_fd(fd) {
        Some(s) => s,
        None => { set_errno(EBADF); return -1; }
    };
    let entry = match slot.as_ref() {
        Some(e) => e,
        None => { set_errno(EBADF); return -1; }
    };

    if let SocketKind::Tcp = entry.kind {
        if let Err(e) = toyos::net::tcp_shutdown(TcpSocketId(entry.netd_id), how as u32) {
            set_errno(net_err_to_errno(e));
            return -1;
        }
    }
    0
}


// ---------------------------------------------------------------------------
// close (for socket fds)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn close_socket(fd: i32) -> bool {
    let slot = match sock_from_fd(fd) {
        Some(s) => s,
        None => return false,
    };
    let entry = match slot.take() {
        Some(e) => e,
        None => return false,
    };

    // Close pipe fds
    if entry.rx_fd != 0 { syscall::close(Fd(entry.rx_fd)); }
    if entry.tx_fd != 0 { syscall::close(Fd(entry.tx_fd)); }
    if entry.notify_fd != 0 { syscall::close(Fd(entry.notify_fd)); }

    // Tell netd to close the socket
    if entry.netd_id != 0 {
        match entry.kind {
            SocketKind::Tcp => { let _ = toyos::net::tcp_close(TcpSocketId(entry.netd_id)); }
            SocketKind::Udp => { let _ = toyos::net::udp_close(UdpSocketId(entry.netd_id)); }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// setsockopt / getsockopt
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn setsockopt(
    fd: i32,
    level: i32,
    optname: i32,
    optval: *const u8,
    _optlen: SocklenT,
) -> i32 {
    let slot = match sock_from_fd(fd) {
        Some(s) => s,
        None => { set_errno(EBADF); return -1; }
    };
    let entry = match slot.as_ref() {
        Some(e) => e,
        None => { set_errno(EBADF); return -1; }
    };

    // TCP_NODELAY is the only option we actually send to netd
    if level == IPPROTO_TCP && optname == TCP_NODELAY && entry.netd_id != 0 {
        let val = if optval.is_null() { 0u32 } else { *(optval as *const i32) as u32 };
        if let Err(e) = toyos::net::tcp_set_option(TcpSocketId(entry.netd_id), OPT_NODELAY, val) {
            set_errno(net_err_to_errno(e));
            return -1;
        }
    }
    // All other options silently succeed (SO_REUSEADDR, SO_KEEPALIVE, etc.)
    0
}

#[no_mangle]
pub unsafe extern "C" fn getsockopt(
    fd: i32,
    level: i32,
    optname: i32,
    optval: *mut u8,
    optlen: *mut SocklenT,
) -> i32 {
    let slot = match sock_from_fd(fd) {
        Some(s) => s,
        None => { set_errno(EBADF); return -1; }
    };
    let entry = match slot.as_ref() {
        Some(e) => e,
        None => { set_errno(EBADF); return -1; }
    };

    if optval.is_null() || optlen.is_null() {
        set_errno(EINVAL);
        return -1;
    }

    if level == IPPROTO_TCP && optname == TCP_NODELAY && entry.netd_id != 0 {
        match toyos::net::tcp_get_option(TcpSocketId(entry.netd_id), OPT_NODELAY) {
            Ok(val) => {
                *(optval as *mut i32) = val as i32;
                *optlen = 4;
                return 0;
            }
            Err(e) => {
                set_errno(net_err_to_errno(e));
                return -1;
            }
        }
    }

    if level == SOL_SOCKET && optname == SO_ERROR {
        *(optval as *mut i32) = 0;
        *optlen = 4;
        return 0;
    }

    // Default: return 0 for unknown options
    if *optlen >= 4 {
        *(optval as *mut i32) = 0;
        *optlen = 4;
    }
    0
}

// ---------------------------------------------------------------------------
// getpeername / getsockname
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn getpeername(
    fd: i32,
    addr: *mut Sockaddr,
    addrlen: *mut SocklenT,
) -> i32 {
    let slot = match sock_from_fd(fd) {
        Some(s) => s,
        None => { set_errno(EBADF); return -1; }
    };
    let entry = match slot.as_ref() {
        Some(e) => e,
        None => { set_errno(EBADF); return -1; }
    };
    if !entry.connected {
        set_errno(ENOTCONN);
        return -1;
    }
    fill_sockaddr(addr, addrlen, entry.remote_addr, entry.remote_port);
    0
}

#[no_mangle]
pub unsafe extern "C" fn getsockname(
    fd: i32,
    addr: *mut Sockaddr,
    addrlen: *mut SocklenT,
) -> i32 {
    let slot = match sock_from_fd(fd) {
        Some(s) => s,
        None => { set_errno(EBADF); return -1; }
    };
    let entry = match slot.as_ref() {
        Some(e) => e,
        None => { set_errno(EBADF); return -1; }
    };
    // Local address: 10.0.2.15 (QEMU default)
    fill_sockaddr(addr, addrlen, [10, 0, 2, 15], entry.local_port);
    0
}

// ---------------------------------------------------------------------------
// DNS: getaddrinfo / freeaddrinfo
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn getaddrinfo(
    node: *const u8,
    _service: *const u8,
    _hints: *const Addrinfo,
    res: *mut *mut Addrinfo,
) -> i32 {
    if node.is_null() || res.is_null() {
        return -1; // EAI_NONAME
    }

    let name_len = super::string::strlen(node);
    let name = core::slice::from_raw_parts(node, name_len);

    // Try parsing as IPv4 literal first
    if let Some(ip) = parse_ipv4(name) {
        return build_addrinfo_result(res, &[(ip, 0)]);
    }

    // Use toyos::net dns_lookup
    let hostname = match core::str::from_utf8(name) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let mut results = [[0u8; 4]; 16];
    let count = match toyos::net::dns_lookup(hostname, &mut results) {
        Ok(n) => n,
        Err(_) => return -1,
    };
    if count == 0 {
        return -1; // EAI_NONAME
    }

    let addrs: Vec<([u8; 4], u16)> = results[..count].iter().map(|ip| (*ip, 0u16)).collect();
    build_addrinfo_result(res, &addrs)
}

unsafe fn build_addrinfo_result(res: *mut *mut Addrinfo, addrs: &[([u8; 4], u16)]) -> i32 {
    let mut prev: *mut Addrinfo = ptr::null_mut();
    // Build linked list in reverse so first result is first in list
    for &(ip, port) in addrs.iter().rev() {
        let layout_ai = core::alloc::Layout::new::<Addrinfo>();
        let ai = heap_alloc(layout_ai) as *mut Addrinfo;
        if ai.is_null() { return -1; }

        let layout_sa = core::alloc::Layout::new::<SockaddrIn>();
        let sa = heap_alloc(layout_sa) as *mut SockaddrIn;
        if sa.is_null() {
            heap_dealloc(ai as *mut u8, layout_ai);
            return -1;
        }

        (*sa).sin_family = AF_INET as u16;
        (*sa).sin_port = port.to_be();
        (*sa).sin_addr.s_addr = u32::from_be_bytes(ip);
        (*sa).sin_zero = [0; 8];

        (*ai).ai_flags = 0;
        (*ai).ai_family = AF_INET;
        (*ai).ai_socktype = SOCK_STREAM;
        (*ai).ai_protocol = 0;
        (*ai).ai_addrlen = core::mem::size_of::<SockaddrIn>() as SocklenT;
        (*ai).ai_addr = sa as *mut Sockaddr;
        (*ai).ai_canonname = ptr::null_mut();
        (*ai).ai_next = prev;

        prev = ai;
    }
    *res = prev;
    0
}

#[no_mangle]
pub unsafe extern "C" fn freeaddrinfo(mut res: *mut Addrinfo) {
    while !res.is_null() {
        let next = (*res).ai_next;
        if !(*res).ai_addr.is_null() {
            heap_dealloc((*res).ai_addr as *mut u8, core::alloc::Layout::new::<SockaddrIn>());
        }
        heap_dealloc(res as *mut u8, core::alloc::Layout::new::<Addrinfo>());
        res = next;
    }
}

#[no_mangle]
pub unsafe extern "C" fn gai_strerror(_errcode: i32) -> *const u8 {
    b"DNS lookup failed\0".as_ptr()
}

// ---------------------------------------------------------------------------
// inet_pton / inet_ntop / htons / ntohs / htonl / ntohl
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn inet_pton(af: i32, src: *const u8, dst: *mut u8) -> i32 {
    if af != AF_INET || src.is_null() || dst.is_null() {
        return 0;
    }
    let len = super::string::strlen(src);
    let s = core::slice::from_raw_parts(src, len);
    match parse_ipv4(s) {
        Some(ip) => {
            ptr::copy_nonoverlapping(ip.as_ptr(), dst, 4);
            1
        }
        None => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn inet_ntop(
    af: i32,
    src: *const u8,
    dst: *mut u8,
    size: SocklenT,
) -> *const u8 {
    if af != AF_INET || src.is_null() || dst.is_null() || size < 16 {
        return ptr::null();
    }
    let a = *src;
    let b = *src.add(1);
    let c = *src.add(2);
    let d = *src.add(3);
    let mut buf = [0u8; 16];
    let n = fmt_ip4(&mut buf, a, b, c, d);
    if n as u32 >= size {
        return ptr::null();
    }
    ptr::copy_nonoverlapping(buf.as_ptr(), dst, n + 1);
    dst as *const u8
}

fn fmt_ip4(buf: &mut [u8; 16], a: u8, b: u8, c: u8, d: u8) -> usize {
    let mut pos = 0;
    for (i, octet) in [a, b, c, d].iter().enumerate() {
        if i > 0 {
            buf[pos] = b'.';
            pos += 1;
        }
        pos += fmt_u8(&mut buf[pos..], *octet);
    }
    buf[pos] = 0;
    pos
}

fn fmt_u8(buf: &mut [u8], val: u8) -> usize {
    if val >= 100 {
        buf[0] = b'0' + val / 100;
        buf[1] = b'0' + (val / 10) % 10;
        buf[2] = b'0' + val % 10;
        3
    } else if val >= 10 {
        buf[0] = b'0' + val / 10;
        buf[1] = b'0' + val % 10;
        2
    } else {
        buf[0] = b'0' + val;
        1
    }
}

#[no_mangle]
pub unsafe extern "C" fn htons(hostshort: u16) -> u16 {
    hostshort.to_be()
}

#[no_mangle]
pub unsafe extern "C" fn ntohs(netshort: u16) -> u16 {
    u16::from_be(netshort)
}

#[no_mangle]
pub unsafe extern "C" fn htonl(hostlong: u32) -> u32 {
    hostlong.to_be()
}

#[no_mangle]
pub unsafe extern "C" fn ntohl(netlong: u32) -> u32 {
    u32::from_be(netlong)
}

#[no_mangle]
pub unsafe extern "C" fn inet_addr(cp: *const u8) -> u32 {
    if cp.is_null() {
        return u32::MAX; // INADDR_NONE
    }
    let len = super::string::strlen(cp);
    let s = core::slice::from_raw_parts(cp, len);
    match parse_ipv4(s) {
        Some(ip) => u32::from_be_bytes(ip),
        None => u32::MAX,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_ipv4(s: &[u8]) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut octet_idx = 0;
    let mut val: u16 = 0;
    let mut has_digit = false;

    for &c in s {
        if c >= b'0' && c <= b'9' {
            val = val * 10 + (c - b'0') as u16;
            if val > 255 {
                return None;
            }
            has_digit = true;
        } else if c == b'.' {
            if !has_digit || octet_idx >= 3 {
                return None;
            }
            octets[octet_idx] = val as u8;
            octet_idx += 1;
            val = 0;
            has_digit = false;
        } else {
            return None;
        }
    }
    if !has_digit || octet_idx != 3 {
        return None;
    }
    octets[3] = val as u8;
    Some(octets)
}

fn make_sockaddr_in(ip: [u8; 4], port: u16) -> SockaddrIn {
    SockaddrIn {
        sin_family: AF_INET as u16,
        sin_port: port.to_be(),
        sin_addr: InAddr { s_addr: u32::from_be_bytes(ip) },
        sin_zero: [0; 8],
    }
}

//! ToyOS userland networking library.
//!
//! Owns the netd IPC protocol and provides client functions for TCP, UDP, and DNS.
//! All networking in ToyOS goes through the `netd` daemon via message passing
//! and kernel pipes.

#![no_std]

use core::sync::atomic::{AtomicI32, Ordering};
use toyos_abi::ipc::{self, IpcHeader};
use toyos_abi::Fd;
use toyos_abi::syscall;

// ---------------------------------------------------------------------------
// netd IPC protocol — message types
// ---------------------------------------------------------------------------

// Client -> netd
pub const MSG_TCP_CLOSE: u32 = 4;
pub const MSG_TCP_SHUTDOWN: u32 = 7;
pub const MSG_UDP_BIND: u32 = 8;
pub const MSG_UDP_SEND_TO: u32 = 9;
pub const MSG_UDP_RECV_FROM: u32 = 10;
pub const MSG_UDP_CLOSE: u32 = 11;
pub const MSG_DNS_LOOKUP: u32 = 12;
pub const MSG_TCP_SET_OPTION: u32 = 13;
pub const MSG_TCP_GET_OPTION: u32 = 14;
pub const MSG_TCP_CONNECT_PIPED: u32 = 20;
pub const MSG_TCP_BIND_PIPED: u32 = 21;
pub const MSG_TCP_ACCEPT_PIPED: u32 = 22;

// netd -> client
pub const MSG_RESULT: u32 = 128;
pub const MSG_ERROR: u32 = 129;

// ---------------------------------------------------------------------------
// Error codes
// ---------------------------------------------------------------------------

pub const ERR_CONNECTION_REFUSED: u32 = 1;
pub const ERR_CONNECTION_RESET: u32 = 2;
pub const ERR_TIMED_OUT: u32 = 3;
pub const ERR_ADDR_IN_USE: u32 = 5;
pub const ERR_NOT_CONNECTED: u32 = 6;
pub const ERR_INVALID_INPUT: u32 = 7;
pub const ERR_OTHER: u32 = 255;

// ---------------------------------------------------------------------------
// TCP option types
// ---------------------------------------------------------------------------

pub const OPT_NODELAY: u32 = 1;

// ---------------------------------------------------------------------------
// Protocol request/response structs
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TcpConnectPipedRequest {
    pub addr: [u8; 4],
    pub port: u16,
    pub _pad: u16,
    pub timeout_ms: u32,
    pub _pad2: u32,
    pub rx_pipe_id: u64,
    pub tx_pipe_id: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TcpConnectResponse {
    pub socket_id: u32,
    pub local_port: u16,
    pub _pad: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SocketCloseRequest {
    pub socket_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TcpBindPipedRequest {
    pub addr: [u8; 4],
    pub port: u16,
    pub _pad: u16,
    pub notify_pipe_id: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TcpBindResponse {
    pub socket_id: u32,
    pub bound_port: u16,
    pub _pad: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TcpShutdownRequest {
    pub socket_id: u32,
    pub how: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TcpAcceptPipedRequest {
    pub socket_id: u32,
    pub _pad: u32,
    pub rx_pipe_id: u64,
    pub tx_pipe_id: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TcpAcceptPipedResponse {
    pub socket_id: u32,
    pub remote_addr: [u8; 4],
    pub remote_port: u16,
    pub local_port: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UdpBindRequest {
    pub addr: [u8; 4],
    pub port: u16,
    pub _pad: u16,
    pub tx_pipe_id: u64,
    pub rx_pipe_id: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UdpBindResponse {
    pub socket_id: u32,
    pub bound_port: u16,
    pub _pad: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UdpSendToRequest {
    pub socket_id: u32,
    pub addr: [u8; 4],
    pub port: u16,
    pub len: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UdpRecvFromRequest {
    pub socket_id: u32,
    pub max_len: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UdpRecvResponse {
    pub addr: [u8; 4],
    pub port: u16,
    pub len: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SocketOptionRequest {
    pub socket_id: u32,
    pub option: u32,
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SocketOptionResponse {
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ErrorResponse {
    pub code: u32,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    ConnectionRefused,
    ConnectionReset,
    TimedOut,
    AddrInUse,
    NotConnected,
    InvalidInput,
    NetdNotFound,
    Syscall,
    UnexpectedResponse,
    Other,
}

impl NetError {
    pub fn from_error_code(code: u32) -> Self {
        match code {
            ERR_CONNECTION_REFUSED => NetError::ConnectionRefused,
            ERR_CONNECTION_RESET => NetError::ConnectionReset,
            ERR_TIMED_OUT => NetError::TimedOut,
            ERR_ADDR_IN_USE => NetError::AddrInUse,
            ERR_NOT_CONNECTED => NetError::NotConnected,
            ERR_INVALID_INPUT => NetError::InvalidInput,
            _ => NetError::Other,
        }
    }
}

// ---------------------------------------------------------------------------
// Return types
// ---------------------------------------------------------------------------

pub struct TcpConnection {
    pub rx_fd: Fd,
    pub tx_fd: Fd,
    pub socket_id: u32,
    pub local_port: u16,
}

pub struct TcpBound {
    pub notify_fd: Fd,
    pub socket_id: u32,
    pub bound_port: u16,
}

pub struct TcpAccepted {
    pub rx_fd: Fd,
    pub tx_fd: Fd,
    pub socket_id: u32,
    pub remote_addr: [u8; 4],
    pub remote_port: u16,
    pub local_port: u16,
}

pub struct UdpBound {
    pub socket_id: u32,
    pub bound_port: u16,
    pub tx_fd: Fd,
    pub rx_fd: Fd,
}

// ---------------------------------------------------------------------------
// netd connection (cached)
// ---------------------------------------------------------------------------

static NETD_FD: AtomicI32 = AtomicI32::new(-1);

fn netd_fd() -> Result<Fd, NetError> {
    let cached = NETD_FD.load(Ordering::Relaxed);
    if cached >= 0 {
        return Ok(Fd(cached));
    }
    for _ in 0..100 {
        if let Ok(fd) = syscall::connect("netd") {
            NETD_FD.store(fd.0, Ordering::Relaxed);
            return Ok(fd);
        }
        syscall::nanosleep(10_000_000);
    }
    Err(NetError::NetdNotFound)
}

// ---------------------------------------------------------------------------
// Low-level netd IPC
// ---------------------------------------------------------------------------

pub fn send_to_netd<T: Copy>(msg_type: u32, payload: &T) -> Result<(), NetError> {
    let fd = netd_fd()?;
    ipc::send(fd, msg_type, payload).map_err(|_| NetError::Other)
}

pub fn send_bytes_to_netd(msg_type: u32, data: &[u8]) -> Result<(), NetError> {
    let fd = netd_fd()?;
    ipc::send_bytes(fd, msg_type, data).map_err(|_| NetError::Other)
}

/// Receive a response header from netd, check for errors.
pub fn recv_response_header() -> Result<IpcHeader, NetError> {
    let fd = netd_fd()?;
    let header = ipc::recv_header(fd).map_err(|_| NetError::Other)?;
    check_response(&header)?;
    Ok(header)
}

/// Receive a typed payload from the last response header.
pub fn recv_payload<T: Copy>(header: &IpcHeader) -> T {
    let fd = netd_fd().unwrap();
    ipc::recv_payload(fd, header).unwrap()
}

/// Receive raw bytes from the last response header.
pub fn recv_bytes(header: &IpcHeader, buf: &mut [u8]) -> usize {
    let fd = netd_fd().unwrap();
    ipc::recv_bytes(fd, header, buf).unwrap()
}

fn recv_header_from_netd() -> Result<IpcHeader, NetError> {
    let fd = netd_fd()?;
    ipc::recv_header(fd).map_err(|_| NetError::Other)
}

fn check_response(header: &IpcHeader) -> Result<(), NetError> {
    if header.msg_type == MSG_ERROR {
        let fd = netd_fd().unwrap();
        let err: ErrorResponse = ipc::recv_payload(fd, header).unwrap();
        return Err(NetError::from_error_code(err.code));
    }
    if header.msg_type != MSG_RESULT {
        return Err(NetError::UnexpectedResponse);
    }
    Ok(())
}

/// Send a typed request to netd and receive a typed response.
fn request<Req: Copy, Resp: Copy>(msg_type: u32, payload: &Req) -> Result<Resp, NetError> {
    send_to_netd(msg_type, payload)?;
    let header = recv_header_from_netd()?;
    check_response(&header)?;
    let fd = netd_fd().unwrap();
    ipc::recv_payload(fd, &header).map_err(|_| NetError::Other)
}

// ---------------------------------------------------------------------------
// TCP client functions
// ---------------------------------------------------------------------------

pub fn tcp_connect(
    addr: [u8; 4],
    port: u16,
    timeout_ms: u32,
) -> Result<TcpConnection, NetError> {
    let rx_pipe = syscall::pipe();
    let tx_pipe = syscall::pipe();

    let rx_pipe_id = syscall::pipe_id(rx_pipe.write).map_err(|_| NetError::Syscall)?;
    let tx_pipe_id = syscall::pipe_id(tx_pipe.read).map_err(|_| NetError::Syscall)?;

    send_to_netd(MSG_TCP_CONNECT_PIPED, &TcpConnectPipedRequest {
        addr,
        port,
        _pad: 0,
        timeout_ms,
        _pad2: 0,
        rx_pipe_id,
        tx_pipe_id,
    })?;
    let header = recv_header_from_netd()?;

    if let Err(e) = check_response(&header) {
        syscall::close(rx_pipe.read);
        syscall::close(rx_pipe.write);
        syscall::close(tx_pipe.read);
        syscall::close(tx_pipe.write);
        return Err(e);
    }

    let fd = netd_fd().unwrap();
    let resp: TcpConnectResponse = ipc::recv_payload(fd, &header).map_err(|_| NetError::Other)?;

    // Close the ends netd opened via pipe_open
    syscall::close(rx_pipe.write);
    syscall::close(tx_pipe.read);

    Ok(TcpConnection {
        rx_fd: rx_pipe.read,
        tx_fd: tx_pipe.write,
        socket_id: resp.socket_id,
        local_port: resp.local_port,
    })
}

pub fn tcp_bind(addr: [u8; 4], port: u16) -> Result<TcpBound, NetError> {
    let notify_pipe = syscall::pipe();
    let notify_pipe_id = syscall::pipe_id(notify_pipe.write).map_err(|_| NetError::Syscall)?;

    send_to_netd(MSG_TCP_BIND_PIPED, &TcpBindPipedRequest {
        addr,
        port,
        _pad: 0,
        notify_pipe_id,
    })?;
    let header = recv_header_from_netd()?;

    if let Err(e) = check_response(&header) {
        syscall::close(notify_pipe.read);
        syscall::close(notify_pipe.write);
        return Err(e);
    }

    let fd = netd_fd().unwrap();
    let resp: TcpBindResponse = ipc::recv_payload(fd, &header).map_err(|_| NetError::Other)?;

    // Close our write end — netd opened its own via pipe_open
    syscall::close(notify_pipe.write);

    Ok(TcpBound {
        notify_fd: notify_pipe.read,
        socket_id: resp.socket_id,
        bound_port: resp.bound_port,
    })
}

pub fn tcp_accept(socket_id: u32) -> Result<TcpAccepted, NetError> {
    let rx_pipe = syscall::pipe();
    let tx_pipe = syscall::pipe();

    let rx_pipe_id = syscall::pipe_id(rx_pipe.write).map_err(|_| NetError::Syscall)?;
    let tx_pipe_id = syscall::pipe_id(tx_pipe.read).map_err(|_| NetError::Syscall)?;

    send_to_netd(MSG_TCP_ACCEPT_PIPED, &TcpAcceptPipedRequest {
        socket_id,
        _pad: 0,
        rx_pipe_id,
        tx_pipe_id,
    })?;
    let header = recv_header_from_netd()?;

    if let Err(e) = check_response(&header) {
        syscall::close(rx_pipe.read);
        syscall::close(rx_pipe.write);
        syscall::close(tx_pipe.read);
        syscall::close(tx_pipe.write);
        return Err(e);
    }

    let fd = netd_fd().unwrap();
    let resp: TcpAcceptPipedResponse = ipc::recv_payload(fd, &header).map_err(|_| NetError::Other)?;

    // Close the ends netd opened via pipe_open
    syscall::close(rx_pipe.write);
    syscall::close(tx_pipe.read);

    Ok(TcpAccepted {
        rx_fd: rx_pipe.read,
        tx_fd: tx_pipe.write,
        socket_id: resp.socket_id,
        remote_addr: resp.remote_addr,
        remote_port: resp.remote_port,
        local_port: resp.local_port,
    })
}

pub fn tcp_shutdown(socket_id: u32, how: u32) -> Result<(), NetError> {
    request::<_, [u8; 0]>(MSG_TCP_SHUTDOWN, &TcpShutdownRequest { socket_id, how })?;
    Ok(())
}

pub fn tcp_close(socket_id: u32) {
    let _ = request::<_, [u8; 0]>(MSG_TCP_CLOSE, &SocketCloseRequest { socket_id });
}

pub fn tcp_set_option(socket_id: u32, option: u32, value: u32) -> Result<(), NetError> {
    request::<_, [u8; 0]>(
        MSG_TCP_SET_OPTION,
        &SocketOptionRequest { socket_id, option, value },
    )?;
    Ok(())
}

pub fn tcp_get_option(socket_id: u32, option: u32) -> Result<u32, NetError> {
    let resp: SocketOptionResponse = request(
        MSG_TCP_GET_OPTION,
        &SocketOptionRequest { socket_id, option, value: 0 },
    )?;
    Ok(resp.value)
}

// ---------------------------------------------------------------------------
// UDP client functions
// ---------------------------------------------------------------------------

pub fn udp_bind(addr: [u8; 4], port: u16) -> Result<UdpBound, NetError> {
    let tx_pipe = syscall::pipe();
    let rx_pipe = syscall::pipe();

    let tx_pipe_id = syscall::pipe_id(tx_pipe.read).map_err(|_| NetError::Syscall)?;
    let rx_pipe_id = syscall::pipe_id(rx_pipe.write).map_err(|_| NetError::Syscall)?;

    send_to_netd(MSG_UDP_BIND, &UdpBindRequest {
        addr,
        port,
        _pad: 0,
        tx_pipe_id,
        rx_pipe_id,
    })?;
    let header = recv_header_from_netd()?;

    if let Err(e) = check_response(&header) {
        syscall::close(tx_pipe.read);
        syscall::close(tx_pipe.write);
        syscall::close(rx_pipe.read);
        syscall::close(rx_pipe.write);
        return Err(e);
    }

    let fd = netd_fd().unwrap();
    let resp: UdpBindResponse = ipc::recv_payload(fd, &header).map_err(|_| NetError::Other)?;

    // Close the ends netd opened via pipe_open
    syscall::close(tx_pipe.read);
    syscall::close(rx_pipe.write);

    Ok(UdpBound {
        socket_id: resp.socket_id,
        bound_port: resp.bound_port,
        tx_fd: tx_pipe.write,
        rx_fd: rx_pipe.read,
    })
}

pub fn udp_close(socket_id: u32) {
    let _ = request::<_, [u8; 0]>(MSG_UDP_CLOSE, &SocketCloseRequest { socket_id });
}

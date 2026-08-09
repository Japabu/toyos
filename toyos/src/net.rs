//! ToyOS userland networking library.
//!
//! Owns the netd IPC protocol and provides client functions for TCP, UDP, and DNS.
//! All networking in ToyOS goes through the `netd` daemon via message passing
//! and kernel pipes.

use toyos_abi::syscall;
use crate::ipc::{IpcHeader, IpcPayload};
use crate::ipc_payload;
use crate::{Connection, Pipe, OwnedHandle};

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgType {
    TcpClose = 4,
    TcpShutdown = 7,
    UdpBind = 8,
    UdpSendTo = 9,
    UdpRecvFrom = 10,
    UdpClose = 11,
    DnsLookup = 12,
    TcpSetOption = 13,
    TcpGetOption = 14,
    TcpConnectPiped = 20,
    TcpBindPiped = 21,
    TcpAcceptPiped = 22,
}

impl MsgType {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            4 => Some(Self::TcpClose),
            7 => Some(Self::TcpShutdown),
            8 => Some(Self::UdpBind),
            9 => Some(Self::UdpSendTo),
            10 => Some(Self::UdpRecvFrom),
            11 => Some(Self::UdpClose),
            12 => Some(Self::DnsLookup),
            13 => Some(Self::TcpSetOption),
            14 => Some(Self::TcpGetOption),
            20 => Some(Self::TcpConnectPiped),
            21 => Some(Self::TcpBindPiped),
            22 => Some(Self::TcpAcceptPiped),
            _ => None,
        }
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RespType {
    Result = 128,
    Error = 129,
}

// Error codes (on the wire)

pub const ERR_CONNECTION_REFUSED: u32 = 1;
pub const ERR_CONNECTION_RESET: u32 = 2;
pub const ERR_TIMED_OUT: u32 = 3;
pub const ERR_ADDR_IN_USE: u32 = 4;
pub const ERR_NOT_CONNECTED: u32 = 5;
pub const ERR_INVALID_INPUT: u32 = 6;
/// netd will not hold another connection of this kind right now.
///
/// Distinct from [`ERR_CONNECTION_REFUSED`] on purpose, and the distinction is
/// not cosmetic: the two ask the client for opposite responses. A peer that
/// refused the SYN will keep refusing it, so the right move is to give up on
/// that peer; netd being full is a condition of this machine that clears when
/// something closes, so the right move is to back off and retry the same peer.
/// A client that cannot tell them apart cannot do either correctly.
///
/// The conflation was real, not hypothetical: `netd`'s own pending-connect
/// path answers a socket that reached `Closed` with `ERR_CONNECTION_REFUSED`,
/// so a capacity refusal on that code is indistinguishable from an ordinary
/// failed connection — including to a test trying to find where the cap is.
pub const ERR_RESOURCE_EXHAUSTED: u32 = 7;
pub const ERR_OTHER: u32 = 255;

pub const OPT_NODELAY: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    NetdNotFound,
    ConnectionRefused,
    ConnectionReset,
    TimedOut,
    AddrInUse,
    NotConnected,
    InvalidInput,
    /// netd is at its own limit. Retryable against the same peer, unlike
    /// [`NetError::ConnectionRefused`] — see [`ERR_RESOURCE_EXHAUSTED`].
    ResourceExhausted,
    Protocol(u32),
    Io,
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
            ERR_RESOURCE_EXHAUSTED => NetError::ResourceExhausted,
            ERR_OTHER => NetError::Io,
            // An older client meets a newer netd here rather than at a panic:
            // an unknown code is still an error, and still says which one.
            code => NetError::Protocol(code),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpSocketId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpSocketId(pub u32);

// Protocol request/response structs

ipc_payload! {
    pub struct TcpConnectPipedRequest {
        pub addr: [u8; 4],
        pub port: u16,
        pub _pad: u16,
        pub timeout_ms: u32,
        pub _pad2: u32,
        pub rx_pipe_id: u64,
        pub tx_pipe_id: u64,
    }

    pub struct TcpConnectResponse {
        pub socket_id: u32,
        pub local_port: u16,
        pub _pad: u16,
    }

    pub struct SocketCloseRequest {
        pub socket_id: u32,
    }

    pub struct TcpBindPipedRequest {
        pub addr: [u8; 4],
        pub port: u16,
        pub _pad: u16,
        pub notify_pipe_id: u64,
    }

    pub struct TcpBindResponse {
        pub socket_id: u32,
        pub bound_port: u16,
        pub _pad: u16,
    }

    pub struct TcpShutdownRequest {
        pub socket_id: u32,
        pub how: u32,
    }

    pub struct TcpAcceptPipedRequest {
        pub socket_id: u32,
        pub _pad: u32,
        pub rx_pipe_id: u64,
        pub tx_pipe_id: u64,
    }

    pub struct TcpAcceptPipedResponse {
        pub socket_id: u32,
        pub remote_addr: [u8; 4],
        pub remote_port: u16,
        pub local_port: u16,
    }

    pub struct UdpBindRequest {
        pub addr: [u8; 4],
        pub port: u16,
        pub _pad: u16,
        pub tx_pipe_id: u64,
        pub rx_pipe_id: u64,
    }

    pub struct UdpBindResponse {
        pub socket_id: u32,
        pub bound_port: u16,
        pub _pad: u16,
    }

    pub struct UdpSendToRequest {
        pub socket_id: u32,
        pub addr: [u8; 4],
        pub port: u16,
        pub len: u16,
    }

    pub struct UdpRecvFromRequest {
        pub socket_id: u32,
        pub max_len: u32,
    }

    pub struct UdpRecvResponse {
        pub addr: [u8; 4],
        pub port: u16,
        pub len: u16,
    }

    pub struct SocketOptionRequest {
        pub socket_id: u32,
        pub option: u32,
        pub value: u32,
    }

    pub struct SocketOptionResponse {
        pub value: u32,
    }

    pub struct ErrorResponse {
        pub code: u32,
    }

    struct SentBytes {
        value: u32,
    }
}

// Return types

pub struct TcpConnection {
    pub rx: Pipe,
    pub tx: Pipe,
    pub socket_id: TcpSocketId,
    pub local_port: u16,
}

pub struct TcpBound {
    pub notify: Pipe,
    pub socket_id: TcpSocketId,
    pub bound_port: u16,
}

pub struct TcpAccepted {
    pub rx: Pipe,
    pub tx: Pipe,
    pub socket_id: TcpSocketId,
    pub remote_addr: [u8; 4],
    pub remote_port: u16,
    pub local_port: u16,
}

pub struct UdpBound {
    pub socket_id: UdpSocketId,
    pub bound_port: u16,
    pub tx: Pipe,
    pub rx: Pipe,
}

// NetdConn — per-operation IPC connection (typestate protocol)

pub struct NetdConn(Connection);

impl NetdConn {
    /// One connection to netd, through this process's own namespace.
    ///
    /// **There was a retry loop here and it is gone.** It spun a hundred times
    /// at ten milliseconds waiting for a name to appear in a global registry;
    /// a `netd` connector is live from this process's first instruction, so
    /// there is nothing to wait for.
    ///
    /// [`NetError::ResourceExhausted`] is a separate answer and a retryable
    /// one: it is the *kernel's* port queue full of connections netd has not
    /// accepted yet, which is backpressure and not a limit netd chose. The
    /// retry loop used to hide it — it retried every error alike — and
    /// collapsing it into `NetdNotFound` would leave a caller told the machine
    /// has no network because a burst outran one accept loop.
    pub fn connect() -> Result<Self, NetError> {
        crate::endow::service("netd").map(Self).map_err(|e| match e {
            // Both are "there is no netd to reach from here": one because the
            // manifest gave this program none, one because it has exited.
            crate::endow::EndowError::NotEndowed
            | crate::endow::EndowError::ServerGone => NetError::NetdNotFound,
            crate::endow::EndowError::Refused(
                toyos_abi::syscall::SyscallError::ResourceExhausted,
            ) => NetError::ResourceExhausted,
            crate::endow::EndowError::Refused(_) => NetError::Io,
        })
    }

    pub fn request<Req: IpcPayload>(self, msg_type: MsgType, payload: &Req) -> Result<PendingResponse, NetError> {
        self.0.send(msg_type as u32, payload).map_err(|_| NetError::Io)?;
        Ok(PendingResponse(self))
    }

    pub fn request_bytes(self, msg_type: MsgType, data: &[u8]) -> Result<PendingResponse, NetError> {
        self.0.send_bytes(msg_type as u32, data).map_err(|_| NetError::Io)?;
        Ok(PendingResponse(self))
    }
}

pub struct PendingResponse(NetdConn);

impl PendingResponse {
    fn conn(&self) -> &Connection { &(self.0).0 }

    fn recv_checked_header(&self) -> Result<IpcHeader, NetError> {
        let header = self.conn().recv_header().map_err(|_| NetError::Io)?;
        if header.msg_type == RespType::Error as u32 {
            let err: ErrorResponse = self.conn().recv_payload(&header).map_err(|_| NetError::Io)?;
            return Err(NetError::from_error_code(err.code));
        }
        if header.msg_type != RespType::Result as u32 {
            return Err(NetError::Protocol(header.msg_type));
        }
        Ok(header)
    }

    pub fn response<Resp: IpcPayload>(self) -> Result<Resp, NetError> {
        let header = self.recv_checked_header()?;
        self.conn().recv_payload(&header).map_err(|_| NetError::Io)
    }

    pub fn response_bytes(self, buf: &mut [u8]) -> Result<usize, NetError> {
        let header = self.recv_checked_header()?;
        self.conn().recv_bytes(&header, buf).map_err(|_| NetError::Io)
    }

    pub fn status(self) -> Result<(), NetError> {
        let header = self.recv_checked_header()?;
        if header.len() > 0 {
            let mut skip = [0u8; 128];
            let _ = self.conn().recv_bytes(&header, &mut skip);
        }
        Ok(())
    }
}

fn pipe_pair() -> Result<(syscall::PipeFds, syscall::PipeFds), NetError> {
    let first = syscall::pipe().map_err(|_| NetError::Io)?;
    match syscall::pipe() {
        Ok(second) => Ok((first, second)),
        Err(_) => {
            syscall::close(first.read);
            syscall::close(first.write);
            Err(NetError::Io)
        }
    }
}

// TCP client functions

pub fn tcp_connect(
    addr: [u8; 4],
    port: u16,
    timeout_ms: u32,
) -> Result<TcpConnection, NetError> {
    let (rx_pipe, tx_pipe) = pipe_pair()?;

    let rx_pipe_id = syscall::pipe_id(rx_pipe.write).map_err(|_| NetError::Io)?;
    let tx_pipe_id = syscall::pipe_id(tx_pipe.read).map_err(|_| NetError::Io)?;

    let result = NetdConn::connect()?
        .request(MsgType::TcpConnectPiped, &TcpConnectPipedRequest {
            addr,
            port,
            _pad: 0,
            timeout_ms,
            _pad2: 0,
            rx_pipe_id,
            tx_pipe_id,
        })?
        .response::<TcpConnectResponse>();

    match result {
        Ok(resp) => {
            syscall::close(rx_pipe.write);
            syscall::close(tx_pipe.read);
            Ok(TcpConnection {
                rx: Pipe(OwnedHandle(rx_pipe.read)),
                tx: Pipe(OwnedHandle(tx_pipe.write)),
                socket_id: TcpSocketId(resp.socket_id),
                local_port: resp.local_port,
            })
        }
        Err(e) => {
            syscall::close(rx_pipe.read);
            syscall::close(rx_pipe.write);
            syscall::close(tx_pipe.read);
            syscall::close(tx_pipe.write);
            Err(e)
        }
    }
}

pub fn tcp_bind(addr: [u8; 4], port: u16) -> Result<TcpBound, NetError> {
    let notify_pipe = syscall::pipe().map_err(|_| NetError::Io)?;
    let notify_pipe_id = syscall::pipe_id(notify_pipe.write).map_err(|_| NetError::Io)?;

    let result = NetdConn::connect()?
        .request(MsgType::TcpBindPiped, &TcpBindPipedRequest {
            addr,
            port,
            _pad: 0,
            notify_pipe_id,
        })?
        .response::<TcpBindResponse>();

    match result {
        Ok(resp) => {
            syscall::close(notify_pipe.write);
            Ok(TcpBound {
                notify: Pipe(OwnedHandle(notify_pipe.read)),
                socket_id: TcpSocketId(resp.socket_id),
                bound_port: resp.bound_port,
            })
        }
        Err(e) => {
            syscall::close(notify_pipe.read);
            syscall::close(notify_pipe.write);
            Err(e)
        }
    }
}

pub fn tcp_accept(socket_id: TcpSocketId) -> Result<TcpAccepted, NetError> {
    let (rx_pipe, tx_pipe) = pipe_pair()?;

    let rx_pipe_id = syscall::pipe_id(rx_pipe.write).map_err(|_| NetError::Io)?;
    let tx_pipe_id = syscall::pipe_id(tx_pipe.read).map_err(|_| NetError::Io)?;

    let result = NetdConn::connect()?
        .request(MsgType::TcpAcceptPiped, &TcpAcceptPipedRequest {
            socket_id: socket_id.0,
            _pad: 0,
            rx_pipe_id,
            tx_pipe_id,
        })?
        .response::<TcpAcceptPipedResponse>();

    match result {
        Ok(resp) => {
            syscall::close(rx_pipe.write);
            syscall::close(tx_pipe.read);
            Ok(TcpAccepted {
                rx: Pipe(OwnedHandle(rx_pipe.read)),
                tx: Pipe(OwnedHandle(tx_pipe.write)),
                socket_id: TcpSocketId(resp.socket_id),
                remote_addr: resp.remote_addr,
                remote_port: resp.remote_port,
                local_port: resp.local_port,
            })
        }
        Err(e) => {
            syscall::close(rx_pipe.read);
            syscall::close(rx_pipe.write);
            syscall::close(tx_pipe.read);
            syscall::close(tx_pipe.write);
            Err(e)
        }
    }
}

pub fn tcp_shutdown(socket_id: TcpSocketId, how: u32) -> Result<(), NetError> {
    NetdConn::connect()?
        .request(MsgType::TcpShutdown, &TcpShutdownRequest { socket_id: socket_id.0, how })?
        .status()
}

pub fn tcp_close(socket_id: TcpSocketId) -> Result<(), NetError> {
    NetdConn::connect()?
        .request(MsgType::TcpClose, &SocketCloseRequest { socket_id: socket_id.0 })?
        .status()
}

pub fn tcp_set_option(socket_id: TcpSocketId, option: u32, value: u32) -> Result<(), NetError> {
    NetdConn::connect()?
        .request(MsgType::TcpSetOption, &SocketOptionRequest { socket_id: socket_id.0, option, value })?
        .status()
}

pub fn tcp_get_option(socket_id: TcpSocketId, option: u32) -> Result<u32, NetError> {
    let resp: SocketOptionResponse = NetdConn::connect()?
        .request(MsgType::TcpGetOption, &SocketOptionRequest { socket_id: socket_id.0, option, value: 0 })?
        .response()?;
    Ok(resp.value)
}

// UDP client functions

pub fn udp_bind(addr: [u8; 4], port: u16) -> Result<UdpBound, NetError> {
    let (tx_pipe, rx_pipe) = pipe_pair()?;

    let tx_pipe_id = syscall::pipe_id(tx_pipe.read).map_err(|_| NetError::Io)?;
    let rx_pipe_id = syscall::pipe_id(rx_pipe.write).map_err(|_| NetError::Io)?;

    let result = NetdConn::connect()?
        .request(MsgType::UdpBind, &UdpBindRequest {
            addr,
            port,
            _pad: 0,
            tx_pipe_id,
            rx_pipe_id,
        })?
        .response::<UdpBindResponse>();

    match result {
        Ok(resp) => {
            syscall::close(tx_pipe.read);
            syscall::close(rx_pipe.write);
            Ok(UdpBound {
                socket_id: UdpSocketId(resp.socket_id),
                bound_port: resp.bound_port,
                tx: Pipe(OwnedHandle(tx_pipe.write)),
                rx: Pipe(OwnedHandle(rx_pipe.read)),
            })
        }
        Err(e) => {
            syscall::close(tx_pipe.read);
            syscall::close(tx_pipe.write);
            syscall::close(rx_pipe.read);
            syscall::close(rx_pipe.write);
            Err(e)
        }
    }
}

pub fn udp_send_to(socket_id: UdpSocketId, addr: [u8; 4], port: u16, len: u16) -> Result<u32, NetError> {
    let resp: SentBytes = NetdConn::connect()?
        .request(MsgType::UdpSendTo, &UdpSendToRequest {
            socket_id: socket_id.0,
            addr,
            port,
            len,
        })?
        .response()?;
    Ok(resp.value)
}

pub fn udp_recv_from(socket_id: UdpSocketId, max_len: u32) -> Result<UdpRecvResponse, NetError> {
    NetdConn::connect()?
        .request(MsgType::UdpRecvFrom, &UdpRecvFromRequest {
            socket_id: socket_id.0,
            max_len,
        })?
        .response()
}

pub fn udp_close(socket_id: UdpSocketId) -> Result<(), NetError> {
    NetdConn::connect()?
        .request(MsgType::UdpClose, &SocketCloseRequest { socket_id: socket_id.0 })?
        .status()
}

pub fn dns_lookup(hostname: &str, results: &mut [[u8; 4]]) -> Result<usize, NetError> {
    let mut buf = [0u8; 256];
    let n = NetdConn::connect()?
        .request_bytes(MsgType::DnsLookup, hostname.as_bytes())?
        .response_bytes(&mut buf)?;

    if n == 0 {
        return Ok(0);
    }

    let count = buf[0] as usize;
    let mut written = 0;
    let mut offset = 1;
    for _ in 0..count {
        if written >= results.len() || offset >= n {
            break;
        }
        if buf[offset] == 4 && offset + 5 <= n {
            results[written] = [buf[offset + 1], buf[offset + 2], buf[offset + 3], buf[offset + 4]];
            written += 1;
            offset += 5;
        } else {
            break;
        }
    }
    Ok(written)
}

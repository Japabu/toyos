// Client -> netd
pub const MSG_TCP_CONNECT: u32 = 1;
pub const MSG_TCP_SEND: u32 = 2;
pub const MSG_TCP_RECV: u32 = 3;
pub const MSG_TCP_CLOSE: u32 = 4;
pub const MSG_TCP_BIND: u32 = 5;
pub const MSG_TCP_ACCEPT: u32 = 6;
pub const MSG_TCP_SHUTDOWN: u32 = 7;
pub const MSG_UDP_BIND: u32 = 8;
pub const MSG_UDP_SEND_TO: u32 = 9;
pub const MSG_UDP_RECV_FROM: u32 = 10;
pub const MSG_UDP_CLOSE: u32 = 11;
pub const MSG_DNS_LOOKUP: u32 = 12;
pub const MSG_TCP_SET_OPTION: u32 = 13;
pub const MSG_TCP_GET_OPTION: u32 = 14;

// netd -> client
pub const MSG_RESULT: u32 = 128;
pub const MSG_ERROR: u32 = 129;

// Error codes
pub const ERR_CONNECTION_REFUSED: u32 = 1;
pub const ERR_CONNECTION_RESET: u32 = 2;
pub const ERR_TIMED_OUT: u32 = 3;
pub const ERR_ADDR_IN_USE: u32 = 5;
pub const ERR_NOT_CONNECTED: u32 = 6;
pub const ERR_INVALID_INPUT: u32 = 7;
pub const ERR_OTHER: u32 = 255;

// TCP option types
pub const OPT_NODELAY: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TcpConnectRequest {
    pub addr: [u8; 4],
    pub port: u16,
    pub _pad: u16,
    pub timeout_ms: u32,
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
pub struct TcpRecvRequest {
    pub socket_id: u32,
    pub max_len: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TcpCloseRequest {
    pub socket_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TcpBindRequest {
    pub addr: [u8; 4],
    pub port: u16,
    pub _pad: u16,
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
pub struct TcpAcceptRequest {
    pub socket_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TcpAcceptResponse {
    pub socket_id: u32,
    pub remote_addr: [u8; 4],
    pub remote_port: u16,
    pub local_port: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TcpShutdownRequest {
    pub socket_id: u32,
    pub how: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UdpBindRequest {
    pub addr: [u8; 4],
    pub port: u16,
    pub _pad: u16,
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
pub struct UdpRecvFromRequest {
    pub socket_id: u32,
    pub max_len: u32,
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

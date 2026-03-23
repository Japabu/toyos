use std::collections::HashMap;
use std::time::{Duration, Instant};
use toyos_abi::Fd;
use toyos_abi::device;
use toyos_abi::ipc;
use toyos_abi::pipe;
use toyos_abi::io_uring::*;
use toyos_abi::raw_net as toyos_nic;
use toyos_abi::services;
use toyos_abi::shm;

use toyos_net::*;

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{dns, tcp, udp};
use smoltcp::time::Instant as SmoltcpInstant;
use smoltcp::wire::{DnsQueryType, EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint};

use std::net::Ipv4Addr;

// --- smoltcp Device wrapper ---

struct DmaNic {
    _dma_region: shm::SharedMemory,
    rx_base: *const u8,
    rx_buf_size: usize,
    tx_buf: *mut u8,
    net_hdr_size: usize,
    mac: [u8; 6],
    nic_fd: Fd,
}

impl DmaNic {
    fn new() -> Self {
        let nic_fd = device::open_nic().expect("netd: failed to claim NIC device");

        let mut info_bytes = [0u8; core::mem::size_of::<toyos_abi::net::NicInfo>()];
        let n = toyos_abi::syscall::read(nic_fd, &mut info_bytes).expect("netd: failed to read NicInfo");
        assert_eq!(n, info_bytes.len(), "netd: NicInfo size mismatch");
        let info: toyos_abi::net::NicInfo = unsafe { core::ptr::read(info_bytes.as_ptr() as *const _) };

        let rx_buf_size = info.rx_buf_size as usize;
        let dma_region = shm::SharedMemory::map(info.dma_token, 2 * 1024 * 1024);
        let dma_base = dma_region.as_ptr() as *const u8;
        let rx_base = unsafe { dma_base.add(info.rx_buf_offset as usize) };
        let tx_ptr = unsafe { dma_base.add(info.tx_buf_offset as usize) as *mut u8 };

        Self {
            _dma_region: dma_region,
            rx_base,
            rx_buf_size,
            tx_buf: tx_ptr,
            net_hdr_size: info.net_hdr_size as usize,
            mac: info.mac,
            nic_fd,
        }
    }

    fn rx_buf(&self, idx: usize) -> *const u8 {
        unsafe { self.rx_base.add(idx * self.rx_buf_size) }
    }
}

impl Device for DmaNic {
    type RxToken<'a> = DmaRxToken<'a>;
    type TxToken<'a> = DmaTxToken<'a>;

    fn receive(&mut self, _timestamp: SmoltcpInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let (buf_idx, frame_len) = toyos_nic::nic_rx_poll()?;
        // Safety: The data slice borrows from the DMA region via the device's lifetime 'a.
        // smoltcp's RxToken::consume takes self by value with FnOnce(&[u8]) -> R, so the
        // callback cannot store the reference. nic_rx_done is called after the callback
        // returns, ensuring the DMA buffer is only refilled after smoltcp is done with it.
        let data = unsafe {
            core::slice::from_raw_parts(
                self.rx_buf(buf_idx).add(self.net_hdr_size),
                frame_len,
            )
        };
        Some((
            DmaRxToken { data, buf_idx },
            DmaTxToken { tx_buf: self.tx_buf, net_hdr_size: self.net_hdr_size, _phantom: core::marker::PhantomData },
        ))
    }

    fn transmit(&mut self, _timestamp: SmoltcpInstant) -> Option<Self::TxToken<'_>> {
        Some(DmaTxToken { tx_buf: self.tx_buf, net_hdr_size: self.net_hdr_size, _phantom: core::marker::PhantomData })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        caps.medium = Medium::Ethernet;
        caps
    }
}

struct DmaRxToken<'a> {
    data: &'a [u8],
    buf_idx: usize,
}

impl<'a> phy::RxToken for DmaRxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let result = f(self.data);
        toyos_nic::nic_rx_done(self.buf_idx);
        result
    }
}

struct DmaTxToken<'a> {
    tx_buf: *mut u8,
    net_hdr_size: usize,
    _phantom: core::marker::PhantomData<&'a ()>,
}

impl<'a> phy::TxToken for DmaTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        unsafe {
            core::ptr::write_bytes(self.tx_buf, 0, self.net_hdr_size);
            let frame = core::slice::from_raw_parts_mut(
                self.tx_buf.add(self.net_hdr_size),
                len,
            );
            let result = f(frame);
            toyos_nic::nic_tx(self.net_hdr_size + len);
            result
        }
    }
}

// --- Socket tracking ---

enum SocketKind {
    TcpStream(SocketHandle),
    TcpListener(SocketHandle),
    Udp(SocketHandle),
}

struct UdpPipes {
    tx_read_fd: Fd,
    rx_write_fd: Fd,
}

struct PendingUdpRecv {
    client_fd: Fd,
    socket_id: u32,
    max_len: u32,
    deadline: Option<Instant>,
}

struct PendingDns {
    client_fd: Fd,
    query: dns::QueryHandle,
}

/// A piped TCP connection: data flows through kernel pipes instead of IPC messages.
struct PipedConnection {
    handle: SocketHandle,
    rx_write_fd: Option<Fd>,
    tx_read_fd: Option<Fd>,
    rx_ring: *const toyos_abi::ring::RingHeader,
    tx_ring: *const toyos_abi::ring::RingHeader,
}

impl PipedConnection {
    fn close_rx(&mut self) {
        if let Some(fd) = self.rx_write_fd.take() {
            toyos_abi::syscall::close(fd);
        }
    }

    fn close_tx(&mut self) {
        if let Some(fd) = self.tx_read_fd.take() {
            toyos_abi::syscall::close(fd);
        }
    }

    fn close_all(&mut self) {
        self.close_rx();
        self.close_tx();
    }

    fn is_fully_closed(&self) -> bool {
        self.rx_write_fd.is_none() && self.tx_read_fd.is_none()
    }
}

/// A piped TCP listener: netd writes 1 byte to notify pipe on new connection.
struct PipedListener {
    handle: SocketHandle,
    notify_write_fd: Fd,
    notify_ring: *const toyos_abi::ring::RingHeader,
    notified: bool,
}

struct PendingPipedConnect {
    client_fd: Fd,
    socket_id: u32,
    handle: SocketHandle,
    rx_pipe_id: u64,
    tx_pipe_id: u64,
    deadline: Option<Instant>,
}

fn map_pipe_ring(fd: Fd) -> *const toyos_abi::ring::RingHeader {
    toyos_abi::syscall::pipe_map(fd)
        .expect("pipe_map failed") as *const toyos_abi::ring::RingHeader
}

/// Open a pipe by ID and return the fd. `read_end=true` opens for reading, false for writing.
fn open_pipe_fd(pipe_id: u64, read_end: bool) -> Option<Fd> {
    pipe::open_by_id(pipe_id, read_end).ok()
}

/// Open rx (write) and tx (read) pipe fds and create a PipedConnection.
fn open_piped_connection(handle: SocketHandle, rx_pipe_id: u64, tx_pipe_id: u64) -> Option<PipedConnection> {
    let rx_write_fd = open_pipe_fd(rx_pipe_id, false)?;
    let tx_read_fd = match open_pipe_fd(tx_pipe_id, true) {
        Some(fd) => fd,
        None => {
            toyos_abi::syscall::close(rx_write_fd);
            return None;
        }
    };
    Some(PipedConnection {
        handle,
        rx_write_fd: Some(rx_write_fd),
        tx_read_fd: Some(tx_read_fd),
        rx_ring: map_pipe_ring(rx_write_fd),
        tx_ring: map_pipe_ring(tx_read_fd),
    })
}

// --- IPC response helpers ---
// Per-operation model: each handler closes the client fd after responding.
// Async handlers store the fd and close it when the operation completes.

const RESP_RESULT: u32 = RespType::Result as u32;
const RESP_ERROR: u32 = RespType::Error as u32;

fn send_error(fd: Fd, code: u32) {
    let _ = ipc::send(fd, RESP_ERROR, &ErrorResponse { code });
}

fn send_error_close(fd: Fd, code: u32) {
    send_error(fd, code);
    toyos_abi::syscall::close(fd);
}

fn send_result_close<T: Copy>(fd: Fd, payload: &T) {
    let _ = ipc::send(fd, RESP_RESULT, payload);
    toyos_abi::syscall::close(fd);
}

fn send_result_bytes_close(fd: Fd, data: &[u8]) {
    let _ = ipc::send_bytes(fd, RESP_RESULT, data);
    toyos_abi::syscall::close(fd);
}

fn signal_result_close(fd: Fd) {
    let _ = ipc::signal(fd, RESP_RESULT);
    toyos_abi::syscall::close(fd);
}

/// Try to read a typed request payload. Returns None if the client disconnected.
fn recv_request<T: IpcPayload>(fd: Fd, header: &ipc::IpcHeader) -> Option<T> {
    ipc::recv_payload(fd, header).ok()
}

struct NetDaemon {
    sockets: HashMap<u32, SocketKind>,
    next_id: u32,
    next_local_port: u16,
    pending_udp_recvs: Vec<PendingUdpRecv>,
    pending_dns: Vec<PendingDns>,
    dns_handle: SocketHandle,
    piped_connections: Vec<PipedConnection>,
    piped_listeners: HashMap<u32, PipedListener>,
    pending_piped_connects: Vec<PendingPipedConnect>,
    udp_pipes: HashMap<u32, UdpPipes>,
}

impl NetDaemon {
    fn new(dns_handle: SocketHandle) -> Self {
        Self {
            sockets: HashMap::new(),
            next_id: 1,
            next_local_port: 49152,
            pending_udp_recvs: Vec::new(),
            pending_dns: Vec::new(),
            dns_handle,
            piped_connections: Vec::new(),
            piped_listeners: HashMap::new(),
            pending_piped_connects: Vec::new(),
            udp_pipes: HashMap::new(),
        }
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn alloc_port(&mut self) -> u16 {
        let port = self.next_local_port;
        self.next_local_port = if self.next_local_port >= 65535 { 49152 } else { self.next_local_port + 1 };
        port
    }

    /// Dispatch an IPC message. Closes client_fd after responding for sync operations.
    /// Async operations store client_fd and close it when the operation completes.
    fn handle_message(
        &mut self,
        client_fd: Fd,
        header: &ipc::IpcHeader,
        socket_set: &mut SocketSet<'_>,
        iface: &mut Interface,
    ) {
        match MsgType::from_u32(header.msg_type) {
            Some(MsgType::TcpClose) => self.handle_tcp_close(client_fd, header, socket_set),
            Some(MsgType::TcpShutdown) => self.handle_tcp_shutdown(client_fd, header, socket_set),
            Some(MsgType::UdpBind) => self.handle_udp_bind(client_fd, header, socket_set),
            Some(MsgType::UdpSendTo) => self.handle_udp_send_to(client_fd, header, socket_set),
            Some(MsgType::UdpRecvFrom) => self.handle_udp_recv_from(client_fd, header, socket_set),
            Some(MsgType::UdpClose) => self.handle_udp_close(client_fd, header, socket_set),
            Some(MsgType::DnsLookup) => self.handle_dns_lookup(client_fd, header, socket_set, iface),
            Some(MsgType::TcpSetOption) => self.handle_tcp_set_option(client_fd, header, socket_set),
            Some(MsgType::TcpGetOption) => self.handle_tcp_get_option(client_fd, header, socket_set),
            Some(MsgType::TcpConnectPiped) => self.handle_tcp_connect_piped(client_fd, header, socket_set, iface),
            Some(MsgType::TcpBindPiped) => self.handle_tcp_bind_piped(client_fd, header, socket_set),
            Some(MsgType::TcpAcceptPiped) => self.handle_tcp_accept_piped(client_fd, header, socket_set),
            None => {
                eprintln!("netd: unknown message type {}", header.msg_type);
                send_error_close(client_fd, ERR_INVALID_INPUT);
            }
        }
    }

    fn handle_tcp_close(&mut self, client_fd: Fd, header: &ipc::IpcHeader, socket_set: &mut SocketSet<'_>) {
        let Some(req) = recv_request::<SocketCloseRequest>(client_fd, header) else {
            toyos_abi::syscall::close(client_fd);
            return;
        };
        if let Some(kind) = self.sockets.remove(&req.socket_id) {
            match kind {
                SocketKind::TcpStream(handle) => {
                    socket_set.get_mut::<tcp::Socket>(handle).close();
                    socket_set.remove(handle);
                    if let Some(pos) = self.piped_connections.iter().position(|c| c.handle == handle) {
                        self.piped_connections.swap_remove(pos).close_all();
                    }
                }
                SocketKind::TcpListener(handle) => {
                    socket_set.get_mut::<tcp::Socket>(handle).abort();
                    socket_set.remove(handle);
                    if let Some(listener) = self.piped_listeners.remove(&req.socket_id) {
                        toyos_abi::syscall::close(listener.notify_write_fd);
                    }
                }
                SocketKind::Udp(handle) => {
                    socket_set.get_mut::<udp::Socket>(handle).close();
                    socket_set.remove(handle);
                    if let Some(pipes) = self.udp_pipes.remove(&req.socket_id) {
                        toyos_abi::syscall::close(pipes.tx_read_fd);
                        toyos_abi::syscall::close(pipes.rx_write_fd);
                    }
                }
            }
        }
        signal_result_close(client_fd);
    }

    fn handle_tcp_shutdown(
        &mut self,
        client_fd: Fd,
        header: &ipc::IpcHeader,
        socket_set: &mut SocketSet<'_>,
    ) {
        let Some(req) = recv_request::<TcpShutdownRequest>(client_fd, header) else {
            toyos_abi::syscall::close(client_fd);
            return;
        };
        let Some(SocketKind::TcpStream(handle)) = self.sockets.get(&req.socket_id) else {
            send_error_close(client_fd, ERR_NOT_CONNECTED);
            return;
        };
        let socket = socket_set.get_mut::<tcp::Socket>(*handle);
        if req.how == 1 || req.how == 2 {
            socket.close();
        }
        signal_result_close(client_fd);
    }

    fn handle_udp_bind(
        &mut self,
        client_fd: Fd,
        header: &ipc::IpcHeader,
        socket_set: &mut SocketSet<'_>,
    ) {
        let Some(req) = recv_request::<UdpBindRequest>(client_fd, header) else {
            toyos_abi::syscall::close(client_fd);
            return;
        };
        let port = if req.port == 0 { self.alloc_port() } else { req.port };

        let Some(tx_read_fd) = open_pipe_fd(req.tx_pipe_id, true) else {
            send_error_close(client_fd, ERR_INVALID_INPUT);
            return;
        };
        let Some(rx_write_fd) = open_pipe_fd(req.rx_pipe_id, false) else {
            toyos_abi::syscall::close(tx_read_fd);
            send_error_close(client_fd, ERR_INVALID_INPUT);
            return;
        };

        let rx_buf = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; 16],
            vec![0u8; 65536],
        );
        let tx_buf = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; 16],
            vec![0u8; 65536],
        );
        let mut socket = udp::Socket::new(rx_buf, tx_buf);
        let endpoint = IpEndpoint::new(IpAddress::Ipv4(Ipv4Addr::from(req.addr)), port);
        if socket.bind(endpoint).is_err() {
            toyos_abi::syscall::close(tx_read_fd);
            toyos_abi::syscall::close(rx_write_fd);
            send_error_close(client_fd, ERR_ADDR_IN_USE);
            return;
        }

        let handle = socket_set.add(socket);
        let socket_id = self.alloc_id();
        self.sockets.insert(socket_id, SocketKind::Udp(handle));
        self.udp_pipes.insert(socket_id, UdpPipes { tx_read_fd, rx_write_fd });

        send_result_close(client_fd, &UdpBindResponse {
            socket_id,
            bound_port: port,
            _pad: 0,
        });
    }

    fn handle_udp_send_to(
        &mut self,
        client_fd: Fd,
        header: &ipc::IpcHeader,
        socket_set: &mut SocketSet<'_>,
    ) {
        let Some(req) = recv_request::<UdpSendToRequest>(client_fd, header) else {
            toyos_abi::syscall::close(client_fd);
            return;
        };

        let Some(SocketKind::Udp(handle)) = self.sockets.get(&req.socket_id) else {
            send_error_close(client_fd, ERR_NOT_CONNECTED);
            return;
        };
        let handle = *handle;

        let Some(pipes) = self.udp_pipes.get(&req.socket_id) else {
            send_error_close(client_fd, ERR_NOT_CONNECTED);
            return;
        };

        let mut buf = vec![0u8; req.len as usize];
        let n = match toyos_abi::syscall::read(pipes.tx_read_fd, &mut buf) {
            Ok(n) => n,
            Err(_) => {
                send_error_close(client_fd, ERR_OTHER);
                return;
            }
        };

        let addr = Ipv4Addr::from(req.addr);
        let endpoint = IpEndpoint::new(IpAddress::Ipv4(addr), req.port);
        let socket = socket_set.get_mut::<udp::Socket>(handle);
        match socket.send_slice(&buf[..n], endpoint) {
            Ok(()) => {
                let sent = n as u32;
                send_result_close(client_fd, &sent);
            }
            Err(_) => send_error_close(client_fd, ERR_OTHER),
        }
    }

    fn send_udp_recv_response(fd: Fd, socket: &mut udp::Socket, max_len: u32, rx_write_fd: Fd) -> bool {
        if !socket.can_recv() {
            return false;
        }
        let mut buf = vec![0u8; max_len as usize];
        match socket.recv_slice(&mut buf) {
            Ok((n, endpoint)) => {
                let addr = match endpoint.endpoint.addr {
                    IpAddress::Ipv4(a) => a.octets(),
                };
                let _ = toyos_abi::syscall::write(rx_write_fd, &buf[..n]);
                let _ = ipc::send(fd, RESP_RESULT, &UdpRecvResponse {
                    addr,
                    port: endpoint.endpoint.port,
                    len: n as u16,
                });
            }
            Err(_) => send_error(fd, ERR_OTHER),
        }
        true
    }

    fn handle_udp_recv_from(
        &mut self,
        client_fd: Fd,
        header: &ipc::IpcHeader,
        socket_set: &mut SocketSet<'_>,
    ) {
        let Some(req) = recv_request::<UdpRecvFromRequest>(client_fd, header) else {
            toyos_abi::syscall::close(client_fd);
            return;
        };
        let Some(SocketKind::Udp(handle)) = self.sockets.get(&req.socket_id) else {
            send_error_close(client_fd, ERR_NOT_CONNECTED);
            return;
        };
        let handle = *handle;
        let Some(pipes) = self.udp_pipes.get(&req.socket_id) else {
            send_error_close(client_fd, ERR_NOT_CONNECTED);
            return;
        };
        let rx_write_fd = pipes.rx_write_fd;
        let socket = socket_set.get_mut::<udp::Socket>(handle);

        if Self::send_udp_recv_response(client_fd, socket, req.max_len, rx_write_fd) {
            // Sync completion — close fd
            toyos_abi::syscall::close(client_fd);
            return;
        }

        // Async — hold fd until data arrives
        self.pending_udp_recvs.push(PendingUdpRecv {
            client_fd,
            socket_id: req.socket_id,
            max_len: req.max_len,
            deadline: None,
        });
    }

    fn handle_udp_close(&mut self, client_fd: Fd, header: &ipc::IpcHeader, socket_set: &mut SocketSet<'_>) {
        let Some(req) = recv_request::<SocketCloseRequest>(client_fd, header) else {
            toyos_abi::syscall::close(client_fd);
            return;
        };
        if let Some(SocketKind::Udp(handle)) = self.sockets.remove(&req.socket_id) {
            socket_set.get_mut::<udp::Socket>(handle).close();
            socket_set.remove(handle);
            if let Some(pipes) = self.udp_pipes.remove(&req.socket_id) {
                toyos_abi::syscall::close(pipes.tx_read_fd);
                toyos_abi::syscall::close(pipes.rx_write_fd);
            }
        }
        signal_result_close(client_fd);
    }

    fn handle_dns_lookup(
        &mut self,
        client_fd: Fd,
        header: &ipc::IpcHeader,
        socket_set: &mut SocketSet<'_>,
        iface: &mut Interface,
    ) {
        let mut raw = [0u8; 256];
        let Ok(n) = ipc::recv_bytes(client_fd, header, &mut raw) else {
            toyos_abi::syscall::close(client_fd);
            return;
        };
        let hostname = raw[..n].to_vec();
        let hostname = match std::str::from_utf8(&hostname) {
            Ok(s) => s,
            Err(_) => {
                send_error_close(client_fd, ERR_INVALID_INPUT);
                return;
            }
        };

        if let Ok(ip) = hostname.parse::<std::net::Ipv4Addr>() {
            let octets = ip.octets();
            let mut resp = vec![1u8];
            resp.push(4);
            resp.extend_from_slice(&octets);
            send_result_bytes_close(client_fd, &resp);
            return;
        }

        let dns = socket_set.get_mut::<dns::Socket>(self.dns_handle);
        match dns.start_query(iface.context(), hostname, DnsQueryType::A) {
            Ok(query) => {
                // Async — hold fd until DNS resolves
                self.pending_dns.push(PendingDns {
                    client_fd,
                    query,
                });
            }
            Err(_) => send_error_close(client_fd, ERR_OTHER),
        }
    }

    fn handle_tcp_set_option(
        &mut self,
        client_fd: Fd,
        header: &ipc::IpcHeader,
        socket_set: &mut SocketSet<'_>,
    ) {
        let Some(req) = recv_request::<SocketOptionRequest>(client_fd, header) else {
            toyos_abi::syscall::close(client_fd);
            return;
        };
        let Some(SocketKind::TcpStream(handle)) = self.sockets.get(&req.socket_id) else {
            send_error_close(client_fd, ERR_NOT_CONNECTED);
            return;
        };
        let socket = socket_set.get_mut::<tcp::Socket>(*handle);
        match req.option {
            OPT_NODELAY => {
                socket.set_nagle_enabled(req.value == 0);
                signal_result_close(client_fd);
            }
            _ => send_error_close(client_fd, ERR_INVALID_INPUT),
        }
    }

    fn handle_tcp_get_option(
        &mut self,
        client_fd: Fd,
        header: &ipc::IpcHeader,
        socket_set: &mut SocketSet<'_>,
    ) {
        let Some(req) = recv_request::<SocketOptionRequest>(client_fd, header) else {
            toyos_abi::syscall::close(client_fd);
            return;
        };
        let Some(SocketKind::TcpStream(handle)) = self.sockets.get(&req.socket_id) else {
            send_error_close(client_fd, ERR_NOT_CONNECTED);
            return;
        };
        let socket = socket_set.get_mut::<tcp::Socket>(*handle);
        match req.option {
            OPT_NODELAY => {
                let val = if socket.nagle_enabled() { 0u32 } else { 1u32 };
                send_result_close(client_fd, &SocketOptionResponse { value: val });
            }
            _ => send_error_close(client_fd, ERR_INVALID_INPUT),
        }
    }

    // --- Piped socket handlers ---

    fn handle_tcp_connect_piped(
        &mut self,
        client_fd: Fd,
        header: &ipc::IpcHeader,
        socket_set: &mut SocketSet<'_>,
        iface: &mut Interface,
    ) {
        let Some(req) = recv_request::<TcpConnectPipedRequest>(client_fd, header) else {
            toyos_abi::syscall::close(client_fd);
            return;
        };
        let remote = IpEndpoint::new(
            IpAddress::Ipv4(Ipv4Addr::from(req.addr)),
            req.port,
        );
        let local_port = self.alloc_port();

        let rx_buf = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let mut socket = tcp::Socket::new(rx_buf, tx_buf);
        if socket.connect(iface.context(), remote, local_port).is_err() {
            send_error_close(client_fd, ERR_CONNECTION_REFUSED);
            return;
        }

        let handle = socket_set.add(socket);
        let socket_id = self.alloc_id();
        self.sockets.insert(socket_id, SocketKind::TcpStream(handle));

        let deadline = if req.timeout_ms > 0 {
            Some(Instant::now() + Duration::from_millis(req.timeout_ms as u64))
        } else {
            None
        };

        // Async — hold fd until connection established
        self.pending_piped_connects.push(PendingPipedConnect {
            client_fd,
            socket_id,
            handle,
            rx_pipe_id: req.rx_pipe_id,
            tx_pipe_id: req.tx_pipe_id,
            deadline,
        });
    }

    fn handle_tcp_bind_piped(
        &mut self,
        client_fd: Fd,
        header: &ipc::IpcHeader,
        socket_set: &mut SocketSet<'_>,
    ) {
        let Some(req) = recv_request::<TcpBindPipedRequest>(client_fd, header) else {
            toyos_abi::syscall::close(client_fd);
            return;
        };
        let port = if req.port == 0 { self.alloc_port() } else { req.port };

        // BUG 4 fix: open pipe BEFORE adding socket to socket_set.
        // If pipe open fails, no socket to clean up.
        let Some(notify_write_fd) = open_pipe_fd(req.notify_pipe_id, false) else {
            send_error_close(client_fd, ERR_INVALID_INPUT);
            return;
        };

        let rx_buf = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let mut socket = tcp::Socket::new(rx_buf, tx_buf);
        if socket.listen(port).is_err() {
            toyos_abi::syscall::close(notify_write_fd);
            send_error_close(client_fd, ERR_ADDR_IN_USE);
            return;
        }

        let handle = socket_set.add(socket);
        let socket_id = self.alloc_id();
        self.sockets.insert(socket_id, SocketKind::TcpListener(handle));

        self.piped_listeners.insert(socket_id, PipedListener {
            handle,
            notify_write_fd,
            notify_ring: map_pipe_ring(notify_write_fd),
            notified: false,
        });

        send_result_close(client_fd, &TcpBindResponse {
            socket_id,
            bound_port: port,
            _pad: 0,
        });
    }

    fn handle_tcp_accept_piped(
        &mut self,
        client_fd: Fd,
        header: &ipc::IpcHeader,
        socket_set: &mut SocketSet<'_>,
    ) {
        let Some(req) = recv_request::<TcpAcceptPipedRequest>(client_fd, header) else {
            toyos_abi::syscall::close(client_fd);
            return;
        };
        let Some(listener) = self.piped_listeners.get(&req.socket_id) else {
            send_error_close(client_fd, ERR_NOT_CONNECTED);
            return;
        };

        let socket = socket_set.get_mut::<tcp::Socket>(listener.handle);
        // BUG 5 fix: check Established, not is_active.
        // is_active is true in SynReceived, but remote_endpoint() is None until Established.
        if socket.state() != tcp::State::Established {
            send_error_close(client_fd, ERR_NOT_CONNECTED);
            return;
        }

        let remote = socket.remote_endpoint().unwrap();
        let local_port = socket.local_endpoint().unwrap().port;
        let remote_addr = match remote.addr {
            IpAddress::Ipv4(a) => a.octets(),
        };

        let old_handle = listener.handle;
        let stream_id = self.alloc_id();
        self.sockets.insert(stream_id, SocketKind::TcpStream(old_handle));

        let Some(conn) = open_piped_connection(old_handle, req.rx_pipe_id, req.tx_pipe_id) else {
            send_error_close(client_fd, ERR_INVALID_INPUT);
            return;
        };
        self.piped_connections.push(conn);

        // Create replacement listener
        let rx_buf = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let mut new_listener = tcp::Socket::new(rx_buf, tx_buf);
        new_listener.listen(local_port).ok();
        let new_handle = socket_set.add(new_listener);
        self.sockets.insert(req.socket_id, SocketKind::TcpListener(new_handle));

        if let Some(pl) = self.piped_listeners.get_mut(&req.socket_id) {
            pl.handle = new_handle;
            pl.notified = false;
        }

        send_result_close(client_fd, &TcpAcceptPipedResponse {
            socket_id: stream_id,
            remote_addr,
            remote_port: remote.port,
            local_port,
        });
    }

    /// Bridge data between smoltcp sockets and kernel pipes for piped connections.
    /// Drains fully in both directions — when the ring is full, data stays in smoltcp's
    /// buffer and TCP window shrinks (correct backpressure).
    fn bridge_piped(&mut self, socket_set: &mut SocketSet<'_>) {
        let mut closed = Vec::new();
        for i in 0..self.piped_connections.len() {
            let conn = &mut self.piped_connections[i];
            let socket = socket_set.get_mut::<tcp::Socket>(conn.handle);
            let rx_ring = unsafe { &*conn.rx_ring };
            let tx_ring = unsafe { &*conn.tx_ring };

            // smoltcp rx → pipe write (drain fully)
            while socket.can_recv() && rx_ring.space() > 0 {
                let mut buf = [0u8; 4096];
                match socket.recv_slice(&mut buf) {
                    Ok(n) if n > 0 => rx_ring.write(&buf[..n]),
                    _ => break,
                };
            }

            // pipe read → smoltcp tx (drain fully)
            while socket.can_send() && tx_ring.available() > 0 {
                let mut buf = [0u8; 4096];
                let n = tx_ring.read(&mut buf);
                if n > 0 {
                    let _ = socket.send_slice(&buf[..n]);
                } else {
                    break;
                }
            }

            // Signal EOF to client when remote has closed and all data is drained
            if !socket.may_recv() && !socket.can_recv() && conn.rx_write_fd.is_some() {
                conn.close_rx();
            }

            // Detect client death: client closed its read end of the RX pipe
            if conn.rx_write_fd.is_some() && rx_ring.is_reader_closed() {
                conn.close_rx();
            }

            // Detect client stopped writing (or died)
            if conn.tx_read_fd.is_some() && tx_ring.is_writer_closed() {
                socket.close();
                conn.close_tx();
            }

            // Fully clean up when both sides are done
            if conn.is_fully_closed() && !socket.is_open() {
                closed.push(i);
            }
        }

        for &i in closed.iter().rev() {
            self.piped_connections.swap_remove(i);
        }
    }

    /// Check piped listeners for new established connections and notify via pipe.
    fn check_piped_listeners(&mut self, socket_set: &mut SocketSet<'_>) {
        for (_, listener) in &mut self.piped_listeners {
            let socket = socket_set.get_mut::<tcp::Socket>(listener.handle);
            // BUG 6 fix: check Established, not is_active.
            // is_active is true in SynReceived before the 3-way handshake completes.
            if socket.state() == tcp::State::Established && !listener.notified {
                let _ = toyos_abi::syscall::write_nonblock(listener.notify_write_fd, &[1]);
                listener.notified = true;
            }
        }
    }

    /// Detect dead piped listeners (owning process died, notify pipe reader closed).
    fn cleanup_dead_listeners(&mut self, socket_set: &mut SocketSet<'_>) {
        let mut dead = Vec::new();
        for (&socket_id, listener) in &self.piped_listeners {
            let ring = unsafe { &*listener.notify_ring };
            if ring.is_reader_closed() {
                dead.push(socket_id);
            }
        }
        for socket_id in dead {
            if let Some(listener) = self.piped_listeners.remove(&socket_id) {
                toyos_abi::syscall::close(listener.notify_write_fd);
                if let Some(kind) = self.sockets.remove(&socket_id) {
                    if let SocketKind::TcpListener(handle) = kind {
                        socket_set.get_mut::<tcp::Socket>(handle).abort();
                        socket_set.remove(handle);
                    }
                }
            }
        }
    }

    /// Process pending async operations (UDP recvs, DNS, piped connects).
    fn process_pending(&mut self, socket_set: &mut SocketSet<'_>) {
        let now = Instant::now();

        // Pending UDP recvs
        let mut i = 0;
        while i < self.pending_udp_recvs.len() {
            let pr = &self.pending_udp_recvs[i];
            let Some(SocketKind::Udp(handle)) = self.sockets.get(&pr.socket_id) else {
                send_error_close(pr.client_fd, ERR_NOT_CONNECTED);
                self.pending_udp_recvs.swap_remove(i);
                continue;
            };
            let handle = *handle;
            let Some(pipes) = self.udp_pipes.get(&pr.socket_id) else {
                send_error_close(pr.client_fd, ERR_NOT_CONNECTED);
                self.pending_udp_recvs.swap_remove(i);
                continue;
            };
            let rx_write_fd = pipes.rx_write_fd;
            let socket = socket_set.get_mut::<udp::Socket>(handle);
            let client = pr.client_fd;
            let max_len = pr.max_len;
            if Self::send_udp_recv_response(client, socket, max_len, rx_write_fd) {
                toyos_abi::syscall::close(client);
                self.pending_udp_recvs.swap_remove(i);
                continue;
            }
            if pr.deadline.is_some_and(|d| now >= d) {
                send_error_close(pr.client_fd, ERR_TIMED_OUT);
                self.pending_udp_recvs.swap_remove(i);
                continue;
            }
            i += 1;
        }

        // Pending DNS queries
        let mut i = 0;
        while i < self.pending_dns.len() {
            let pd = &self.pending_dns[i];
            let dns = socket_set.get_mut::<dns::Socket>(self.dns_handle);
            match dns.get_query_result(pd.query) {
                Ok(addrs) => {
                    let mut resp = Vec::new();
                    resp.push(addrs.len() as u8);
                    for addr in addrs.iter() {
                        match addr {
                            IpAddress::Ipv4(a) => {
                                resp.push(4);
                                resp.extend_from_slice(&a.octets());
                            }
                        }
                    }
                    send_result_bytes_close(pd.client_fd, &resp);
                    self.pending_dns.swap_remove(i);
                    continue;
                }
                Err(dns::GetQueryResultError::Pending) => {
                    i += 1;
                    continue;
                }
                Err(_) => {
                    send_error_close(pd.client_fd, ERR_OTHER);
                    self.pending_dns.swap_remove(i);
                    continue;
                }
            }
        }

        // Pending piped connects
        let mut i = 0;
        while i < self.pending_piped_connects.len() {
            let pc = &self.pending_piped_connects[i];
            let socket = socket_set.get_mut::<tcp::Socket>(pc.handle);
            if socket.may_send() {
                let local_port = socket.local_endpoint().map(|e| e.port).unwrap_or(0);
                let Some(conn) = open_piped_connection(pc.handle, pc.rx_pipe_id, pc.tx_pipe_id) else {
                    // Client died or pipes invalid — clean up the smoltcp socket
                    send_error_close(pc.client_fd, ERR_OTHER);
                    socket.abort();
                    self.sockets.remove(&pc.socket_id);
                    socket_set.remove(pc.handle);
                    self.pending_piped_connects.swap_remove(i);
                    continue;
                };
                self.piped_connections.push(conn);

                let resp = TcpConnectResponse {
                    socket_id: pc.socket_id,
                    local_port,
                    _pad: 0,
                };
                send_result_close(pc.client_fd, &resp);
                self.pending_piped_connects.swap_remove(i);
                continue;
            }
            if socket.state() == tcp::State::Closed {
                send_error_close(pc.client_fd, ERR_CONNECTION_REFUSED);
                self.sockets.remove(&pc.socket_id);
                socket_set.remove(pc.handle);
                self.pending_piped_connects.swap_remove(i);
                continue;
            }
            if pc.deadline.is_some_and(|d| now >= d) {
                send_error_close(pc.client_fd, ERR_TIMED_OUT);
                socket.abort();
                self.sockets.remove(&pc.socket_id);
                socket_set.remove(pc.handle);
                self.pending_piped_connects.swap_remove(i);
                continue;
            }
            i += 1;
        }
    }
}

fn main() {
    let listener = services::listen("netd").expect("netd already running");

    let mut device = DmaNic::new();
    let mac = device.mac;

    eprintln!(
        "netd: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
    let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    let epoch = Instant::now();
    let now = SmoltcpInstant::from_millis(0);
    let mut iface = Interface::new(config, &mut device, now);

    iface.update_ip_addrs(|addrs| {
        addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)).ok();
    });
    iface.routes_mut()
        .add_default_ipv4_route(Ipv4Addr::new(10, 0, 2, 2))
        .ok();

    let mut socket_set = SocketSet::new(vec![]);

    let dns_servers = &[IpAddress::v4(10, 0, 2, 3)];
    let dns_socket = dns::Socket::new(dns_servers, vec![]);
    let dns_handle = socket_set.add(dns_socket);

    let mut daemon = NetDaemon::new(dns_handle);

    eprintln!("netd: ready");

    // Create io_uring for event multiplexing
    let (ring_fd, ring_shm_token) = toyos_abi::syscall::io_uring_setup(64).expect("netd: io_uring_setup failed");
    let ring_base = unsafe { toyos_abi::syscall::map_shared(ring_shm_token) };
    let ring_params = unsafe { &*(ring_base as *const IoUringParams) };
    let ring_sq_size = ring_params.sq_ring_size;
    let ring_cq_size = ring_params.cq_ring_size;
    const TOKEN_LISTENER: u64 = 0;
    const TOKEN_NIC: u64 = 1;

    loop {
        // 1. Drive smoltcp until quiescent
        let now = SmoltcpInstant::from_millis(epoch.elapsed().as_millis() as i64);
        while iface.poll(now, &mut device, &mut socket_set) != PollResult::None {}

        // 2. Bridge piped connections (drain fully)
        daemon.bridge_piped(&mut socket_set);

        // 3. Check piped listeners for Established connections
        daemon.check_piped_listeners(&mut socket_set);

        // 4. Detect dead piped listeners (process crashed)
        daemon.cleanup_dead_listeners(&mut socket_set);

        // 5. Process pending async operations
        daemon.process_pending(&mut socket_set);

        // 6. Compute poll timeout
        let delay = iface.poll_delay(now, &socket_set);

        let has_pending_async = !daemon.pending_udp_recvs.is_empty()
            || !daemon.pending_dns.is_empty()
            || !daemon.pending_piped_connects.is_empty();

        let timeout_nanos = if has_pending_async {
            Some(Duration::from_millis(1).as_nanos() as u64)
        } else {
            match delay {
                Some(d) if d.total_millis() > 0 => Some(Duration::from_millis(d.total_millis() as u64).as_nanos() as u64),
                Some(_) => Some(Duration::from_millis(1).as_nanos() as u64),
                None => None,
            }
        };

        // 7. Poll: listener fd + NIC fd via io_uring
        {
            let sq_hdr = unsafe { &*(ring_base.add(SQ_RING_OFF as usize) as *const IoUringRingHeader) };
            // Submit POLL_ADD for listener
            let tail = sq_hdr.tail.load(std::sync::atomic::Ordering::Acquire);
            let sqe = unsafe {
                &mut *(ring_base.add(SQES_OFF as usize + (tail & (ring_sq_size - 1)) as usize * core::mem::size_of::<IoUringSqe>()) as *mut IoUringSqe)
            };
            *sqe = IoUringSqe::default();
            sqe.op = IORING_OP_POLL_ADD;
            sqe.fd = listener.0;
            sqe.op_flags = IORING_POLL_IN;
            sqe.user_data = TOKEN_LISTENER;
            sq_hdr.tail.store(tail.wrapping_add(1), std::sync::atomic::Ordering::Release);

            // Submit POLL_ADD for NIC
            let tail = sq_hdr.tail.load(std::sync::atomic::Ordering::Acquire);
            let sqe = unsafe {
                &mut *(ring_base.add(SQES_OFF as usize + (tail & (ring_sq_size - 1)) as usize * core::mem::size_of::<IoUringSqe>()) as *mut IoUringSqe)
            };
            *sqe = IoUringSqe::default();
            sqe.op = IORING_OP_POLL_ADD;
            sqe.fd = device.nic_fd.0;
            sqe.op_flags = IORING_POLL_IN;
            sqe.user_data = TOKEN_NIC;
            sq_hdr.tail.store(tail.wrapping_add(1), std::sync::atomic::Ordering::Release);
        }

        let timeout = match timeout_nanos {
            None => u64::MAX,
            Some(n) => n,
        };
        let _ = toyos_abi::syscall::io_uring_enter(ring_fd, 2, 1, timeout);

        // Drain CQEs
        let mut listener_ready = false;
        {
            let cq_hdr = unsafe { &*(ring_base.add(CQ_RING_OFF as usize) as *const IoUringRingHeader) };
            loop {
                let head = cq_hdr.head.load(std::sync::atomic::Ordering::Acquire);
                let tail = cq_hdr.tail.load(std::sync::atomic::Ordering::Acquire);
                if head == tail { break; }
                let idx = head & (ring_cq_size - 1);
                let cqe = unsafe {
                    &*(ring_base.add(CQ_RING_OFF as usize + 16 + idx as usize * core::mem::size_of::<IoUringCqe>()) as *const IoUringCqe)
                };
                if cqe.result > 0 && cqe.user_data == TOKEN_LISTENER {
                    listener_ready = true;
                }
                cq_hdr.head.store(head.wrapping_add(1), std::sync::atomic::Ordering::Release);
            }
        }

        // 8. Accept and handle new IPC request (per-operation: accept → handle → close)
        if listener_ready {
            let conn = services::accept(listener).expect("accept failed");
            if let Ok(header) = ipc::recv_header(conn.fd) {
                daemon.handle_message(conn.fd, &header, &mut socket_set, &mut iface);
            } else {
                toyos_abi::syscall::close(conn.fd);
            }
        }
    }
}

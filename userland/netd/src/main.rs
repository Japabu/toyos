use std::collections::HashMap;
use std::os::fd::AsRawFd;
use toyos_abi::Fd;
use std::os::toyos::message::{self, Message};
use std::os::toyos::device as toyos_device;
use std::os::toyos::pipe as toyos_pipe;
use std::time::{Duration, Instant};
use toyos_abi::poll as toyos_poll;
use toyos_abi::raw_net as toyos_nic;
use toyos_abi::services;
use toyos_abi::shm;
use toyos_net::*;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{dns, tcp, udp};
use smoltcp::time::Instant as SmoltcpInstant;
use smoltcp::wire::{DnsQueryType, EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint};

use std::net::Ipv4Addr;

// --- smoltcp Device wrapper ---

struct DmaNic {
    rx_bufs: [*const u8; 3],
    tx_buf: *mut u8,
    net_hdr_size: usize,
    mac: [u8; 6],
    pending_rx: Option<(usize, usize)>,
    _nic_file: std::fs::File,
}

impl DmaNic {
    fn new() -> Self {
        use std::io::Read;

        let mut nic_file = toyos_device::open_nic().expect("netd: failed to claim NIC device");

        let mut info_bytes = [0u8; core::mem::size_of::<toyos_abi::net::NicInfo>()];
        nic_file.read_exact(&mut info_bytes).expect("netd: failed to read NicInfo");
        let info: toyos_abi::net::NicInfo = unsafe { core::ptr::read(info_bytes.as_ptr() as *const _) };

        let mut rx_bufs = [core::ptr::null::<u8>(); 3];
        for i in 0..info.rx_buf_count as usize {
            let region = shm::SharedMemory::map(info.rx_buf_tokens[i], 4096);
            rx_bufs[i] = region.as_ptr() as *const u8;
            std::mem::forget(region);
        }
        let tx_region = shm::SharedMemory::map(info.tx_buf_token, 4096);
        let tx_ptr = tx_region.as_ptr();
        std::mem::forget(tx_region);

        Self {
            rx_bufs,
            tx_buf: tx_ptr,
            net_hdr_size: info.net_hdr_size as usize,
            mac: info.mac,
            pending_rx: None,
            _nic_file: nic_file,
        }
    }

    fn try_recv(&mut self) {
        if self.pending_rx.is_some() {
            return;
        }
        self.pending_rx = toyos_nic::nic_rx_poll();
    }
}

impl Device for DmaNic {
    type RxToken<'a> = DmaRxToken<'a>;
    type TxToken<'a> = DmaTxToken<'a>;

    fn receive(&mut self, _timestamp: SmoltcpInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let (buf_idx, frame_len) = self.pending_rx.take()?;
        let data = unsafe {
            core::slice::from_raw_parts(
                self.rx_bufs[buf_idx].add(self.net_hdr_size),
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

struct PendingUdpRecv {
    client_pid: u32,
    socket_id: u32,
    max_len: u32,
    deadline: Option<Instant>,
}

struct PendingDns {
    client_pid: u32,
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
    notified: bool,
}

struct PendingPipedConnect {
    client_pid: u32,
    socket_id: u32,
    handle: SocketHandle,
    rx_pipe_id: u64,
    tx_pipe_id: u64,
    deadline: Option<Instant>,
}

/// Convert a std::fs::File into an Fd, consuming ownership without running the destructor.
fn file_into_fd(file: std::fs::File) -> Fd {
    let fd = Fd(file.as_raw_fd());
    std::mem::forget(file);
    fd
}

fn map_pipe_ring(fd: Fd) -> *const toyos_abi::ring::RingHeader {
    toyos_abi::syscall::pipe_map(fd)
        .expect("pipe_map failed") as *const toyos_abi::ring::RingHeader
}

/// Open a pipe by ID and return the fd. `read_end=true` opens for reading, false for writing.
fn open_pipe_fd(pipe_id: u64, read_end: bool) -> Option<Fd> {
    toyos_pipe::open_by_id(pipe_id, read_end).ok().map(file_into_fd)
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

/// Build a UDP recv response: [addr:4][port:2][pad:2][data:n]
fn build_udp_recv_response(addr: [u8; 4], port: u16, data: &[u8]) -> Vec<u8> {
    let mut resp = Vec::with_capacity(8 + data.len());
    resp.extend_from_slice(&addr);
    resp.extend_from_slice(&port.to_le_bytes());
    resp.extend_from_slice(&[0, 0]);
    resp.extend_from_slice(data);
    resp
}

struct NetDaemon {
    sockets: HashMap<u32, SocketKind>,
    owners: HashMap<u32, u32>, // socket_id -> owner pid
    next_id: u32,
    next_local_port: u16,
    pending_udp_recvs: Vec<PendingUdpRecv>,
    pending_dns: Vec<PendingDns>,
    dns_handle: SocketHandle,
    piped_connections: Vec<PipedConnection>,
    piped_listeners: HashMap<u32, PipedListener>,
    pending_piped_connects: Vec<PendingPipedConnect>,
}

impl NetDaemon {
    fn new(dns_handle: SocketHandle) -> Self {
        Self {
            sockets: HashMap::new(),
            owners: HashMap::new(),
            next_id: 1,
            next_local_port: 49152,
            pending_udp_recvs: Vec::new(),
            pending_dns: Vec::new(),
            dns_handle,
            piped_connections: Vec::new(),
            piped_listeners: HashMap::new(),
            pending_piped_connects: Vec::new(),
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

    fn send_error(pid: u32, code: u32) {
        message::send(pid, Message::new(MSG_ERROR, ErrorResponse { code })).ok();
    }

    fn handle_message(
        &mut self,
        msg: Message,
        socket_set: &mut SocketSet<'_>,
        iface: &mut Interface,
    ) {
        let sender = msg.sender();
        match msg.msg_type() {
            MSG_TCP_CLOSE => self.handle_tcp_close(sender, msg, socket_set),
            MSG_TCP_SHUTDOWN => self.handle_tcp_shutdown(sender, msg, socket_set),
            MSG_UDP_BIND => self.handle_udp_bind(sender, msg, socket_set),
            MSG_UDP_SEND_TO => self.handle_udp_send_to(sender, msg, socket_set),
            MSG_UDP_RECV_FROM => self.handle_udp_recv_from(sender, msg, socket_set),
            MSG_UDP_CLOSE => self.handle_udp_close(sender, msg, socket_set),
            MSG_DNS_LOOKUP => self.handle_dns_lookup(sender, msg, socket_set, iface),
            MSG_TCP_SET_OPTION => self.handle_tcp_set_option(sender, msg, socket_set),
            MSG_TCP_GET_OPTION => self.handle_tcp_get_option(sender, msg, socket_set),
            MSG_TCP_CONNECT_PIPED => self.handle_tcp_connect_piped(sender, msg, socket_set, iface),
            MSG_TCP_BIND_PIPED => self.handle_tcp_bind_piped(sender, msg, socket_set),
            MSG_TCP_ACCEPT_PIPED => self.handle_tcp_accept_piped(sender, msg, socket_set),
            other => {
                eprintln!("netd: unknown message type {other} from pid {sender}");
            }
        }
    }

    fn handle_tcp_close(&mut self, sender: u32, msg: Message, socket_set: &mut SocketSet<'_>) {
        let req: SocketCloseRequest = msg.take_payload();
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
                }
            }
            self.owners.remove(&req.socket_id);
        }
        message::send(sender, Message::signal(MSG_RESULT)).ok();
    }

    fn handle_tcp_shutdown(
        &mut self,
        sender: u32,
        msg: Message,
        socket_set: &mut SocketSet<'_>,
    ) {
        let req: TcpShutdownRequest = msg.take_payload();
        let Some(SocketKind::TcpStream(handle)) = self.sockets.get(&req.socket_id) else {
            Self::send_error(sender, ERR_NOT_CONNECTED);
            return;
        };
        let socket = socket_set.get_mut::<tcp::Socket>(*handle);
        if req.how == 1 || req.how == 2 {
            socket.close();
        }
        message::send(sender, Message::signal(MSG_RESULT)).ok();
    }

    fn handle_udp_bind(
        &mut self,
        sender: u32,
        msg: Message,
        socket_set: &mut SocketSet<'_>,
    ) {
        let req: UdpBindRequest = msg.take_payload();
        let port = if req.port == 0 { self.alloc_port() } else { req.port };

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
            Self::send_error(sender, ERR_ADDR_IN_USE);
            return;
        }

        let handle = socket_set.add(socket);
        let socket_id = self.alloc_id();
        self.sockets.insert(socket_id, SocketKind::Udp(handle));
        self.owners.insert(socket_id, sender);

        message::send(sender, Message::new(MSG_RESULT, UdpBindResponse {
            socket_id,
            bound_port: port,
            _pad: 0,
        })).ok();
    }

    fn handle_udp_send_to(
        &mut self,
        sender: u32,
        msg: Message,
        socket_set: &mut SocketSet<'_>,
    ) {
        let bytes = msg.take_bytes();
        if bytes.len() < 12 {
            Self::send_error(sender, ERR_INVALID_INPUT);
            return;
        }
        let socket_id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let addr = Ipv4Addr::new(bytes[4], bytes[5], bytes[6], bytes[7]);
        let port = u16::from_le_bytes([bytes[8], bytes[9]]);
        let data = &bytes[12..];

        let Some(SocketKind::Udp(handle)) = self.sockets.get(&socket_id) else {
            Self::send_error(sender, ERR_NOT_CONNECTED);
            return;
        };
        let socket = socket_set.get_mut::<udp::Socket>(*handle);
        let endpoint = IpEndpoint::new(IpAddress::Ipv4(addr), port);
        match socket.send_slice(data, endpoint) {
            Ok(()) => {
                let sent = data.len() as u32;
                message::send(sender, Message::new(MSG_RESULT, sent)).ok();
            }
            Err(_) => Self::send_error(sender, ERR_OTHER),
        }
    }

    fn send_udp_recv_response(pid: u32, socket: &mut udp::Socket, max_len: u32) -> bool {
        if !socket.can_recv() {
            return false;
        }
        let mut buf = vec![0u8; max_len as usize];
        match socket.recv_slice(&mut buf) {
            Ok((n, endpoint)) => {
                let addr = match endpoint.endpoint.addr {
                    IpAddress::Ipv4(a) => a.octets(),
                };
                let resp = build_udp_recv_response(addr, endpoint.endpoint.port, &buf[..n]);
                message::send(pid, Message::from_bytes(MSG_RESULT, &resp)).ok();
            }
            Err(_) => Self::send_error(pid, ERR_OTHER),
        }
        true
    }

    fn handle_udp_recv_from(
        &mut self,
        sender: u32,
        msg: Message,
        socket_set: &mut SocketSet<'_>,
    ) {
        let req: UdpRecvFromRequest = msg.take_payload();
        let Some(SocketKind::Udp(handle)) = self.sockets.get(&req.socket_id) else {
            Self::send_error(sender, ERR_NOT_CONNECTED);
            return;
        };
        let socket = socket_set.get_mut::<udp::Socket>(*handle);

        if Self::send_udp_recv_response(sender, socket, req.max_len) {
            return;
        }

        self.pending_udp_recvs.push(PendingUdpRecv {
            client_pid: sender,
            socket_id: req.socket_id,
            max_len: req.max_len,
            deadline: None,
        });
    }

    fn handle_udp_close(&mut self, sender: u32, msg: Message, socket_set: &mut SocketSet<'_>) {
        let req: SocketCloseRequest = msg.take_payload();
        if let Some(SocketKind::Udp(handle)) = self.sockets.remove(&req.socket_id) {
            socket_set.get_mut::<udp::Socket>(handle).close();
            socket_set.remove(handle);
            self.owners.remove(&req.socket_id);
        }
        message::send(sender, Message::signal(MSG_RESULT)).ok();
    }

    fn handle_dns_lookup(
        &mut self,
        sender: u32,
        msg: Message,
        socket_set: &mut SocketSet<'_>,
        iface: &mut Interface,
    ) {
        let hostname = msg.take_bytes();
        let hostname = match std::str::from_utf8(&hostname) {
            Ok(s) => s,
            Err(_) => {
                Self::send_error(sender, ERR_INVALID_INPUT);
                return;
            }
        };

        if let Ok(ip) = hostname.parse::<std::net::Ipv4Addr>() {
            let octets = ip.octets();
            let mut resp = vec![1u8];
            resp.push(4);
            resp.extend_from_slice(&octets);
            message::send(sender, Message::from_bytes(MSG_RESULT, &resp)).ok();
            return;
        }

        let dns = socket_set.get_mut::<dns::Socket>(self.dns_handle);
        match dns.start_query(iface.context(), hostname, DnsQueryType::A) {
            Ok(query) => {
                self.pending_dns.push(PendingDns {
                    client_pid: sender,
                    query,
                });
            }
            Err(_) => Self::send_error(sender, ERR_OTHER),
        }
    }

    fn handle_tcp_set_option(
        &mut self,
        sender: u32,
        msg: Message,
        socket_set: &mut SocketSet<'_>,
    ) {
        let req: SocketOptionRequest = msg.take_payload();
        let Some(SocketKind::TcpStream(handle)) = self.sockets.get(&req.socket_id) else {
            Self::send_error(sender, ERR_NOT_CONNECTED);
            return;
        };
        let socket = socket_set.get_mut::<tcp::Socket>(*handle);
        match req.option {
            OPT_NODELAY => {
                socket.set_nagle_enabled(req.value == 0);
                message::send(sender, Message::signal(MSG_RESULT)).ok();
            }
            _ => Self::send_error(sender, ERR_INVALID_INPUT),
        }
    }

    fn handle_tcp_get_option(
        &mut self,
        sender: u32,
        msg: Message,
        socket_set: &mut SocketSet<'_>,
    ) {
        let req: SocketOptionRequest = msg.take_payload();
        let Some(SocketKind::TcpStream(handle)) = self.sockets.get(&req.socket_id) else {
            Self::send_error(sender, ERR_NOT_CONNECTED);
            return;
        };
        let socket = socket_set.get_mut::<tcp::Socket>(*handle);
        match req.option {
            OPT_NODELAY => {
                let val = if socket.nagle_enabled() { 0u32 } else { 1u32 };
                message::send(sender, Message::new(MSG_RESULT, SocketOptionResponse { value: val })).ok();
            }
            _ => Self::send_error(sender, ERR_INVALID_INPUT),
        }
    }

    // --- Piped socket handlers ---

    fn handle_tcp_connect_piped(
        &mut self,
        sender: u32,
        msg: Message,
        socket_set: &mut SocketSet<'_>,
        iface: &mut Interface,
    ) {
        let req: TcpConnectPipedRequest = msg.take_payload();
        let remote = IpEndpoint::new(
            IpAddress::Ipv4(Ipv4Addr::from(req.addr)),
            req.port,
        );
        let local_port = self.alloc_port();

        let rx_buf = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let mut socket = tcp::Socket::new(rx_buf, tx_buf);
        if socket.connect(iface.context(), remote, local_port).is_err() {
            Self::send_error(sender, ERR_CONNECTION_REFUSED);
            return;
        }

        let handle = socket_set.add(socket);
        let socket_id = self.alloc_id();
        self.sockets.insert(socket_id, SocketKind::TcpStream(handle));
        self.owners.insert(socket_id, sender);

        let deadline = if req.timeout_ms > 0 {
            Some(Instant::now() + Duration::from_millis(req.timeout_ms as u64))
        } else {
            None
        };

        self.pending_piped_connects.push(PendingPipedConnect {
            client_pid: sender,
            socket_id,
            handle,
            rx_pipe_id: req.rx_pipe_id,
            tx_pipe_id: req.tx_pipe_id,
            deadline,
        });
    }

    fn handle_tcp_bind_piped(
        &mut self,
        sender: u32,
        msg: Message,
        socket_set: &mut SocketSet<'_>,
    ) {
        let req: TcpBindPipedRequest = msg.take_payload();
        let port = if req.port == 0 { self.alloc_port() } else { req.port };

        let rx_buf = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let mut socket = tcp::Socket::new(rx_buf, tx_buf);
        if socket.listen(port).is_err() {
            Self::send_error(sender, ERR_ADDR_IN_USE);
            return;
        }

        let handle = socket_set.add(socket);
        let socket_id = self.alloc_id();
        self.sockets.insert(socket_id, SocketKind::TcpListener(handle));
        self.owners.insert(socket_id, sender);

        let Some(notify_write_fd) = open_pipe_fd(req.notify_pipe_id, false) else {
            Self::send_error(sender, ERR_INVALID_INPUT);
            return;
        };

        self.piped_listeners.insert(socket_id, PipedListener {
            handle,
            notify_write_fd,
            notified: false,
        });

        message::send(sender, Message::new(MSG_RESULT, TcpBindResponse {
            socket_id,
            bound_port: port,
            _pad: 0,
        })).ok();
    }

    fn handle_tcp_accept_piped(
        &mut self,
        sender: u32,
        msg: Message,
        socket_set: &mut SocketSet<'_>,
    ) {
        let req: TcpAcceptPipedRequest = msg.take_payload();
        let Some(listener) = self.piped_listeners.get(&req.socket_id) else {
            Self::send_error(sender, ERR_NOT_CONNECTED);
            return;
        };

        let socket = socket_set.get_mut::<tcp::Socket>(listener.handle);
        if !socket.is_active() {
            Self::send_error(sender, ERR_NOT_CONNECTED);
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
        self.owners.insert(stream_id, sender);

        let Some(conn) = open_piped_connection(old_handle, req.rx_pipe_id, req.tx_pipe_id) else {
            Self::send_error(sender, ERR_INVALID_INPUT);
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

        message::send(sender, Message::new(MSG_RESULT, TcpAcceptPipedResponse {
            socket_id: stream_id,
            remote_addr,
            remote_port: remote.port,
            local_port,
        })).ok();
    }

    /// Bridge data between smoltcp sockets and kernel pipes for piped connections.
    fn bridge_piped(&mut self, socket_set: &mut SocketSet<'_>) {
        let mut closed = Vec::new();
        for i in 0..self.piped_connections.len() {
            let conn = &mut self.piped_connections[i];
            let socket = socket_set.get_mut::<tcp::Socket>(conn.handle);
            let rx_ring = unsafe { &*conn.rx_ring };
            let tx_ring = unsafe { &*conn.tx_ring };

            // smoltcp rx → pipe write (netd pushes received data to client)
            if socket.can_recv() && rx_ring.space() > 0 {
                let mut buf = [0u8; 4096];
                if let Ok(n) = socket.recv_slice(&mut buf) {
                    if n > 0 {
                        rx_ring.write(&buf[..n]);
                    }
                }
            }

            // pipe read → smoltcp tx (netd reads client's outgoing data)
            if socket.can_send() && tx_ring.available() > 0 {
                let mut buf = [0u8; 4096];
                let n = tx_ring.read(&mut buf);
                if n > 0 {
                    let _ = socket.send_slice(&buf[..n]);
                }
            }

            // Signal EOF to client when remote has closed and all data is drained
            if !socket.may_recv() && !socket.can_recv() && conn.rx_write_fd.is_some() {
                conn.close_rx();
            }

            // Signal broken pipe when client has stopped reading
            if conn.tx_read_fd.is_some() && tx_ring.is_reader_closed() {
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

    /// Check piped listeners for new connections and notify via pipe.
    fn check_piped_listeners(&mut self, socket_set: &mut SocketSet<'_>) {
        for (_, listener) in &mut self.piped_listeners {
            let socket = socket_set.get_mut::<tcp::Socket>(listener.handle);
            if socket.is_active() && !listener.notified {
                let _ = toyos_abi::syscall::write_nonblock(listener.notify_write_fd, &[1]);
                listener.notified = true;
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
                Self::send_error(pr.client_pid, ERR_NOT_CONNECTED);
                self.pending_udp_recvs.swap_remove(i);
                continue;
            };
            let handle = *handle;
            let socket = socket_set.get_mut::<udp::Socket>(handle);
            let client = pr.client_pid;
            let max_len = pr.max_len;
            if Self::send_udp_recv_response(client, socket, max_len) {
                self.pending_udp_recvs.swap_remove(i);
                continue;
            }
            if pr.deadline.is_some_and(|d| now >= d) {
                Self::send_error(pr.client_pid, ERR_TIMED_OUT);
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
                    message::send(pd.client_pid, Message::from_bytes(MSG_RESULT, &resp)).ok();
                    self.pending_dns.swap_remove(i);
                    continue;
                }
                Err(dns::GetQueryResultError::Pending) => {
                    i += 1;
                    continue;
                }
                Err(_) => {
                    Self::send_error(pd.client_pid, ERR_OTHER);
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
                    Self::send_error(pc.client_pid, ERR_OTHER);
                    self.pending_piped_connects.swap_remove(i);
                    continue;
                };
                self.piped_connections.push(conn);

                let resp = TcpConnectResponse {
                    socket_id: pc.socket_id,
                    local_port,
                    _pad: 0,
                };
                message::send(pc.client_pid, Message::new(MSG_RESULT, resp)).ok();
                self.pending_piped_connects.swap_remove(i);
                continue;
            }
            if socket.state() == tcp::State::Closed {
                Self::send_error(pc.client_pid, ERR_CONNECTION_REFUSED);
                self.sockets.remove(&pc.socket_id);
                self.owners.remove(&pc.socket_id);
                socket_set.remove(pc.handle);
                self.pending_piped_connects.swap_remove(i);
                continue;
            }
            if pc.deadline.is_some_and(|d| now >= d) {
                Self::send_error(pc.client_pid, ERR_TIMED_OUT);
                socket.abort();
                self.sockets.remove(&pc.socket_id);
                self.owners.remove(&pc.socket_id);
                socket_set.remove(pc.handle);
                self.pending_piped_connects.swap_remove(i);
                continue;
            }
            i += 1;
        }
    }
}

fn main() {
    services::register("netd").expect("netd already running");

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

    loop {
        let now = SmoltcpInstant::from_millis(epoch.elapsed().as_millis() as i64);

        device.try_recv();
        iface.poll(now, &mut device, &mut socket_set);

        daemon.process_pending(&mut socket_set);
        daemon.bridge_piped(&mut socket_set);
        daemon.check_piped_listeners(&mut socket_set);

        let delay = iface.poll_delay(now, &socket_set);
        let mut timeout = match delay {
            Some(d) if d.total_millis() > 0 => Duration::from_millis(d.total_millis() as u64).min(Duration::from_millis(50)),
            Some(_) => Duration::from_millis(1),
            None => Duration::from_millis(50),
        };

        let has_pending = !daemon.pending_udp_recvs.is_empty()
            || !daemon.pending_dns.is_empty()
            || !daemon.pending_piped_connects.is_empty()
            || !daemon.piped_connections.is_empty();
        if has_pending {
            timeout = timeout.min(Duration::from_millis(1));
        }

        let result = toyos_poll::poll_timeout(&[], Some(timeout.as_nanos() as u64));

        if result.messages() {
            loop {
                let msg = message::recv();
                daemon.handle_message(msg, &mut socket_set, &mut iface);

                let check = toyos_poll::poll_timeout(&[], Some(0));
                if !check.messages() {
                    break;
                }
            }
        }
    }
}

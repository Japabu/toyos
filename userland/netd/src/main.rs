use std::collections::HashMap;
use std::time::{Duration, Instant};
use toyos_abi::RawHandle;
use toyos::poller::{IORING_POLL_IN, Poller};
use toyos::ipc;
use toyos::ipc::{Connection, IpcPayload, RxStep};
/// One line, one `write`.
///
/// **`eprintln!` is not one write.** Stderr is unbuffered by design, so
/// `write_fmt` issues a syscall per format fragment, and on this machine the
/// console and the kernel's log ring are one stream — so somebody else's whole
/// line lands inside this daemon's. `netd: ready, at most ` and
/// `init: started test-runner` arrived interleaved and the harness parsed a cap
/// out of the wrong number. `userland/soundd` has the same macro for the same
/// reason. **The class is closed at the kernel since L5** — a `ConsoleObject`
/// per holder buffers a line and emits it whole under one `BackendGuard`
/// (`specs/log-architecture-spec.md` §4.4) — so what this still buys is one
/// syscall per line instead of one per fragment.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let mut line = format!($($arg)*);
        line.push('\n');
        let _ = std::io::stderr().write_all(line.as_bytes());
    }};
}

use toyos::endow;
use toyos::shm;
use toyos_abi::syscall::DeviceType;
use toyos::{Nic as NicDev, Pipe};

use toyos::net::*;

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
    nic_fd: NicDev,
}

impl DmaNic {
    /// Bring up the DMA rings behind a claim `/bin/init` minted and endowed.
    ///
    /// Whether this machine *has* a NIC is answered before netd's first
    /// instruction — metal-sim has none, and neither does the target laptop
    /// until its own driver exists — so the absent case is a missing endowment
    /// label and not an error here. What is left is the kernel contradicting
    /// its own description, which is a bug rather than a machine.
    fn open(nic_dev: NicDev) -> Self {
        let info = nic_dev.info().expect("netd: failed to read NicInfo");

        let rx_buf_size = info.rx_buf_size as usize;
        let dma_region = shm::SharedMemory::adopt(info.dma, 2 * 1024 * 1024)
            .expect("the DMA region the NIC claim just handed over");
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
            nic_fd: nic_dev,
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
        // netd holds the NIC claim, so a refusal here is a kernel-side bug,
        // not a condition to swallow.
        let v = self.nic_fd.rx_poll().expect("netd holds the NIC claim");
        if v == 0 { return None; }
        let (buf_idx, frame_len) = ((v >> 16) as usize, (v & 0xFFFF) as usize);
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
            DmaRxToken { data, buf_idx, nic: &self.nic_fd },
            DmaTxToken { tx_buf: self.tx_buf, net_hdr_size: self.net_hdr_size, nic: &self.nic_fd },
        ))
    }

    fn transmit(&mut self, _timestamp: SmoltcpInstant) -> Option<Self::TxToken<'_>> {
        Some(DmaTxToken { tx_buf: self.tx_buf, net_hdr_size: self.net_hdr_size, nic: &self.nic_fd })
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
    /// The claim the refill is made through — the authority, not a pid the
    /// kernel would have had to look up.
    nic: &'a NicDev,
}

impl<'a> phy::RxToken for DmaRxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let result = f(self.data);
        self.nic.rx_done(self.buf_idx as u64).expect("netd holds the NIC claim");
        result
    }
}

struct DmaTxToken<'a> {
    tx_buf: *mut u8,
    net_hdr_size: usize,
    nic: &'a NicDev,
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
            self.nic.tx((self.net_hdr_size + len) as u64).expect("netd holds the NIC claim");
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
    tx_read_fd: Pipe,
    rx_write_fd: Pipe,
}

struct PendingUdpRecv {
    client: Client,
    socket_id: u32,
    max_len: u32,
    deadline: Option<Instant>,
}

struct PendingDns {
    client: Client,
    query: dns::QueryHandle,
}

/// A piped TCP connection: data flows through kernel pipes instead of IPC messages.
struct PipedConnection {
    handle: SocketHandle,
    rx_write_fd: Option<Pipe>,
    tx_read_fd: Option<Pipe>,
    rx_ring: *const toyos_abi::ring::RingHeader,
    tx_ring: *const toyos_abi::ring::RingHeader,
}

impl PipedConnection {
    fn close_rx(&mut self) {
        self.rx_write_fd.take();
    }

    fn close_tx(&mut self) {
        self.tx_read_fd.take();
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
    notify_write_fd: Pipe,
    notify_ring: *const toyos_abi::ring::RingHeader,
    notified: bool,
}

struct PendingPipedConnect {
    client: Client,
    socket_id: u32,
    handle: SocketHandle,
    /// Held from the moment the request arrived. The ends came *with* it, so
    /// there is nothing left to open when the handshake completes and nothing
    /// to fail there — where a pipe id could still be refused after netd had
    /// already told smoltcp to connect.
    pipes: DataPipes,
    deadline: Option<Instant>,
}

fn map_pipe_ring(pipe: &Pipe) -> *const toyos_abi::ring::RingHeader {
    pipe.pipe_map()
        .expect("pipe_map failed") as *const toyos_abi::ring::RingHeader
}

/// The two ends of a client's data path, as the client's request handed them
/// over.
///
/// A pipe end travels as itself now: the client makes both pipes, keeps the
/// ends facing itself, and moves these two. They used to be ids in the request
/// payload, which netd reopened by number — and any peer of the pipe's creator
/// could have named the same one.
struct DataPipes {
    to_client: Pipe,
    from_client: Pipe,
}

impl DataPipes {
    /// Take the pair the frame just read off `client` promised.
    fn take(client: &Client) -> Option<Self> {
        let [to_client, from_client] = client.conn.recv_handles_exact::<{ DATA_HANDLES }>()?;
        Some(Self {
            to_client: unsafe { Pipe::from_raw(to_client) },
            from_client: unsafe { Pipe::from_raw(from_client) },
        })
    }
}

fn piped_connection(handle: SocketHandle, pipes: DataPipes) -> PipedConnection {
    let rx_ring = map_pipe_ring(&pipes.to_client);
    let tx_ring = map_pipe_ring(&pipes.from_client);
    PipedConnection {
        handle,
        rx_write_fd: Some(pipes.to_client),
        tx_read_fd: Some(pipes.from_client),
        rx_ring,
        tx_ring,
    }
}

// --- One request, and the client waiting for its answer ---

const RESP_RESULT: u32 = RespType::Result as u32;
const RESP_ERROR: u32 = RespType::Error as u32;

/// One client's connection, which is also how netd names it.
///
/// netd answers a connection exactly once and then lets it close, so a handler
/// owns this for as long as its operation lasts: the synchronous ones drop it
/// where they answer, and the three asynchronous ones keep it across passes
/// until what they started finishes. The fd closes with it — which is what
/// replaced a `mem::forget` on the accepted connection and eight hand-written
/// `close` calls that had to agree with each other on every path.
struct Client {
    conn: Connection,
}

impl Client {
    fn result<T: IpcPayload>(&self, payload: &T) {
        self.answered(self.conn.try_send(RESP_RESULT, payload));
    }

    fn result_bytes(&self, data: &[u8]) {
        self.answered(self.conn.try_send_bytes(RESP_RESULT, data));
    }

    fn done(&self) {
        self.answered(self.conn.try_signal(RESP_RESULT));
    }

    fn error(&self, code: u32) {
        self.answered(self.conn.try_send(RESP_ERROR, &ErrorResponse { code }));
    }

    /// **The answer goes out in one non-blocking write, and a refusal is not
    /// retried.** `ipc::send` parks in `sys_write` until the client drains,
    /// which is a client deciding when the network stack runs again; and
    /// `TrySendError::Full` can have left part of the frame in the pipe, so
    /// there is nothing here to retry either. The connection closes either way.
    /// The log is the only place the machine this runs on gets told that a
    /// client asked something and was never answered.
    fn answered(&self, sent: Result<(), ipc::TrySendError>) {
        if let Err(e) = sent {
            let why = match e {
                ipc::TrySendError::Full => {
                    "its pipe will not take the answer and it is not reading"
                }
                ipc::TrySendError::TooLarge => "the answer netd built is larger than a frame",
                ipc::TrySendError::Syscall(_) => "its connection is gone",
            };
            say!("netd: dropping client {} — {why}", self.conn.fd().0);
        }
    }
}

/// Poll registrations that are not piped connections: the service listener and
/// the NIC fd.
const FIXED_POLL_FDS: u32 = 2;

/// Connections accepted and not yet carrying a whole request.
///
/// The kernel queues 32 unaccepted connections per listener
/// (`listener::MAX_PENDING_CONNECTIONS`); this is the same allowance one step
/// further along, for a client that has been accepted and has not yet said what
/// it wants. Past it netd refuses by name rather than growing, and
/// [`HANDSHAKE_TIMEOUT`] is what guarantees the table drains.
const MAX_PENDING_CONNS: u32 = 32;

/// How long an accepted connection may go without completing its request.
///
/// Policy, and generous: every client in the tree sends its request in the
/// statement after `connect` (`toyos::net`'s `NetdConn::request`). What this
/// bounds is the one that never sends it.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// The largest request payload netd keeps.
///
/// `MsgType::DnsLookup` is the only request carrying bytes rather than a struct,
/// and `toyos::net::dns_lookup` frames a hostname into a 256-byte buffer; every
/// typed request is far smaller, `TcpConnectPipedRequest` at 32 bytes being the
/// widest. A client may declare anything up to `ipc::MAX_FRAME_LEN` — the excess
/// is counted down and discarded, never waited for.
const MAX_KEPT_REQUEST: usize = 256;

/// Hard ceiling on live piped connections, from the poller rather than from
/// memory: netd registers every tx pipe in the same batch as the two fixed fds
/// and the pending connections, and `Poller::MAX_HANDLES` is the widest set one
/// poller can carry. The memory budget below is what binds on any machine with
/// less than 8 GiB.
const MAX_PIPED_SLOTS: u64 = (Poller::MAX_HANDLES - FIXED_POLL_FDS - MAX_PENDING_CONNS) as u64;

/// One client's inbound framing.
///
/// **netd never reads a client with a blocking read.** That is the whole point
/// of [`ipc::FrameRx`]: `ipc::recv_header` and `ipc::recv_payload` park the
/// caller until the peer sends the bytes it promised, and netd used to call
/// both — so one client that connected and wrote four bytes stopped the network
/// stack for everyone until it disconnected. Here a peer that stops halfway
/// through a frame costs a buffer and a deadline instead of the event loop.
type ClientRx = ipc::FrameRx<MAX_KEPT_REQUEST>;

/// A connection that has been accepted and has not yet said what it wants.
///
/// It exists because `accept` and the request frame are two events, and netd
/// used to fuse them with a blocking `recv_header` on the fresh fd.
struct PendingConn {
    conn: Connection,
    rx: ClientRx,
    since: Instant,
}

/// A whole request, off the fd and in memory.
///
/// The payload travels with the frame instead of being read off the fd during
/// dispatch: the read side is finished before anything acts on a message, so no
/// handler below can park on the client that sent it.
struct Request {
    client: Client,
    msg_type: u32,
    payload: [u8; MAX_KEPT_REQUEST],
    payload_len: usize,
}

impl Request {
    fn payload(&self) -> &[u8] {
        &self.payload[..self.payload_len]
    }
}

/// Payload bytes a UDP socket's receive buffer holds, and therefore the longest
/// datagram netd can ever hand back — which is what bounds the buffer
/// [`NetDaemon::deliver_datagram`] sizes from a client's `max_len`.
const UDP_SOCKET_BUFFER: usize = 65536;

/// Payload bytes each direction of a TCP socket buffers inside netd, before the
/// window closes and the peer is asked to wait.
const TCP_SOCKET_BUFFER: usize = 65536;

/// Physical memory one piped connection costs. A kernel pipe is exactly one
/// 2 MiB page (`kernel/src/pipe.rs`: `PIPE_SIZE = PAGE_2M`) and a piped socket
/// is two of them, one per direction. The client allocates them, but netd
/// holding the far ends is what keeps them alive, so this is netd's to bound.
const PIPED_CONNECTION_BYTES: u64 = 2 * 2 * 1024 * 1024;

/// Share of physical memory netd will keep tied up in client pipes.
///
/// Policy, not derivation, and the same eighth the compositor takes for the
/// same reason: nothing in the kernel says what a process may use — no
/// per-process limit, no pressure signal, no OOM killer — so the quantity that
/// would make this derivable does not exist yet.
const PIPE_BUDGET_SHARE: u64 = 8;

/// How many piped connections netd will hold, given total physical memory.
///
/// An eighth of memory divided by the two pipes a connection costs, floored at
/// one and capped at what one poller can watch.
///
/// **A mitigation, not a policy anyone chose.** A piped connection's 4 MiB is
/// charged to nobody — no per-process limit, no pressure signal, no OOM killer
/// (`specs/issues/isolation/`) — so without a cap a client that opens sockets
/// in a loop walks the machine into exhaustion, and netd has no way to tell
/// that from ordinary use. Delete this in favour of a kernel memory limit, not
/// in favour of a bigger number.
fn max_piped_connections(total_mem: u64) -> usize {
    let budget = total_mem / PIPE_BUDGET_SHARE;
    (budget / PIPED_CONNECTION_BYTES).clamp(1, MAX_PIPED_SLOTS) as usize
}

/// Total physical memory, as the kernel reports it.
fn total_memory() -> u64 {
    let mut buf = [0u8; toyos::system::SYSINFO_HEADER_SIZE];
    let n = toyos::system::sysinfo(&mut buf);
    assert!(n >= toyos::system::SYSINFO_HEADER_SIZE, "sysinfo returned {n} bytes");
    u64::from_le_bytes(buf[0..8].try_into().unwrap())
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
    max_piped_connections: usize,
}

impl NetDaemon {
    fn new(dns_handle: SocketHandle, max_piped_connections: usize) -> Self {
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
            max_piped_connections,
        }
    }

    /// Is there room for one more piped connection?
    ///
    /// Counts the connects still waiting for their SYN-ACK: they each already
    /// name a pair of pipes, so leaving them out would let a burst of
    /// `TCP_CONNECT_PIPED` overshoot the cap by the whole burst.
    fn piped_room(&self) -> bool {
        self.piped_live() < self.max_piped_connections
    }

    /// Connections the cap is counting. Reported by both refusals, because
    /// `piped_connections.len()` alone reads as "0 already, max 126" when a
    /// burst of connects fills the pending list — a refusal that looks like a
    /// bug in the check rather than the check working.
    fn piped_live(&self) -> usize {
        self.piped_connections.len() + self.pending_piped_connects.len()
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

    /// Dispatch one whole request.
    ///
    /// A synchronous handler answers and lets the connection close where it
    /// stands; an asynchronous one moves the [`Client`] into its pending list
    /// and answers when what it started finishes.
    fn handle_message(
        &mut self,
        req: Request,
        socket_set: &mut SocketSet<'_>,
        iface: &mut Interface,
    ) {
        match MsgType::from_u32(req.msg_type) {
            Some(MsgType::TcpClose) => self.handle_tcp_close(&req, socket_set),
            Some(MsgType::TcpShutdown) => self.handle_tcp_shutdown(&req, socket_set),
            Some(MsgType::UdpBind) => self.handle_udp_bind(&req, socket_set),
            Some(MsgType::UdpSendTo) => self.handle_udp_send_to(&req, socket_set),
            Some(MsgType::UdpRecvFrom) => self.handle_udp_recv_from(req, socket_set),
            Some(MsgType::UdpClose) => self.handle_udp_close(&req, socket_set),
            Some(MsgType::DnsLookup) => self.handle_dns_lookup(req, socket_set, iface),
            Some(MsgType::TcpSetOption) => self.handle_tcp_set_option(&req, socket_set),
            Some(MsgType::TcpGetOption) => self.handle_tcp_get_option(&req, socket_set),
            Some(MsgType::TcpConnectPiped) => self.handle_tcp_connect_piped(req, socket_set, iface),
            Some(MsgType::TcpBindPiped) => self.handle_tcp_bind_piped(&req, socket_set),
            Some(MsgType::TcpAcceptPiped) => self.handle_tcp_accept_piped(&req, socket_set),
            None => {
                say!("netd: unknown message type {}", req.msg_type);
                req.client.error(ERR_INVALID_INPUT);
            }
        }
    }

    fn handle_tcp_close(&mut self, msg: &Request, socket_set: &mut SocketSet<'_>) {
        let Ok(req) = ipc::decode_payload::<SocketCloseRequest>(msg.payload()) else {
            msg.client.error(ERR_INVALID_INPUT);
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
                    self.piped_listeners.remove(&req.socket_id);
                }
                SocketKind::Udp(handle) => {
                    socket_set.get_mut::<udp::Socket>(handle).close();
                    socket_set.remove(handle);
                    self.udp_pipes.remove(&req.socket_id);
                }
            }
        }
        msg.client.done();
    }

    fn handle_tcp_shutdown(&mut self, msg: &Request, socket_set: &mut SocketSet<'_>) {
        let Ok(req) = ipc::decode_payload::<TcpShutdownRequest>(msg.payload()) else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };
        let Some(SocketKind::TcpStream(handle)) = self.sockets.get(&req.socket_id) else {
            msg.client.error(ERR_NOT_CONNECTED);
            return;
        };
        let socket = socket_set.get_mut::<tcp::Socket>(*handle);
        if req.how == 1 || req.how == 2 {
            socket.close();
        }
        msg.client.done();
    }

    fn handle_udp_bind(&mut self, msg: &Request, socket_set: &mut SocketSet<'_>) {
        let Ok(req) = ipc::decode_payload::<UdpBindRequest>(msg.payload()) else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };
        let port = if req.port == 0 { self.alloc_port() } else { req.port };

        let Some(pipes) = DataPipes::take(&msg.client) else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };
        let (rx_write_fd, tx_read_fd) = (pipes.to_client, pipes.from_client);

        let rx_buf = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; 16],
            vec![0u8; UDP_SOCKET_BUFFER],
        );
        let tx_buf = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; 16],
            vec![0u8; UDP_SOCKET_BUFFER],
        );
        let mut socket = udp::Socket::new(rx_buf, tx_buf);
        let endpoint = IpEndpoint::new(IpAddress::Ipv4(Ipv4Addr::from(req.addr)), port);
        if socket.bind(endpoint).is_err() {
            msg.client.error(ERR_ADDR_IN_USE);
            return;
        }

        let handle = socket_set.add(socket);
        let socket_id = self.alloc_id();
        self.sockets.insert(socket_id, SocketKind::Udp(handle));
        self.udp_pipes.insert(socket_id, UdpPipes { tx_read_fd, rx_write_fd });

        msg.client.result(&UdpBindResponse {
            socket_id,
            bound_port: port,
            _pad: 0,
        });
    }

    fn handle_udp_send_to(&mut self, msg: &Request, socket_set: &mut SocketSet<'_>) {
        let Ok(req) = ipc::decode_payload::<UdpSendToRequest>(msg.payload()) else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };

        let Some(SocketKind::Udp(handle)) = self.sockets.get(&req.socket_id) else {
            msg.client.error(ERR_NOT_CONNECTED);
            return;
        };
        let handle = *handle;

        let Some(pipes) = self.udp_pipes.get(&req.socket_id) else {
            msg.client.error(ERR_NOT_CONNECTED);
            return;
        };

        let mut buf = vec![0u8; req.len as usize];
        let n = match toyos_abi::syscall::read_nonblock(pipes.tx_read_fd.fd(), &mut buf) {
            Ok(n) => n,
            // The client writes the datagram into the pipe and *then* sends this
            // request, so an empty pipe is a client naming bytes it never put
            // there. A blocking read here waits for a second write that a
            // conforming client never makes.
            Err(toyos_abi::syscall::SyscallError::WouldBlock) => {
                msg.client.error(ERR_INVALID_INPUT);
                return;
            }
            Err(_) => {
                msg.client.error(ERR_OTHER);
                return;
            }
        };

        let addr = Ipv4Addr::from(req.addr);
        let endpoint = IpEndpoint::new(IpAddress::Ipv4(addr), req.port);
        let socket = socket_set.get_mut::<udp::Socket>(handle);
        match socket.send_slice(&buf[..n], endpoint) {
            Ok(()) => msg.client.result(&(n as u32)),
            Err(_) => msg.client.error(ERR_OTHER),
        }
    }

    /// Hand one waiting datagram to a client, or report that none has arrived.
    ///
    /// The write into the client's own receive pipe is non-blocking, and a short
    /// one is refused rather than reported: the response names a length, and a
    /// client reading that many bytes out of a pipe holding fewer would splice
    /// the next datagram onto this one. A client that is not draining its
    /// receive pipe loses the datagram — which is what UDP does under pressure —
    /// and is told so rather than being lied to.
    fn deliver_datagram(
        client: &Client,
        socket: &mut udp::Socket,
        max_len: u32,
        rx_write_fd: RawHandle,
    ) -> bool {
        if !socket.can_recv() {
            return false;
        }
        // `max_len` is the client's number. Clamped rather than trusted: the
        // socket's own receive buffer is 65536 bytes, so no datagram it can hand
        // back is longer, and an unclamped `vec!` here is a 4 GiB allocation any
        // client can ask netd to make.
        let mut buf = vec![0u8; (max_len as usize).min(UDP_SOCKET_BUFFER)];
        match socket.recv_slice(&mut buf) {
            Ok((n, endpoint)) => {
                let addr = match endpoint.endpoint.addr {
                    IpAddress::Ipv4(a) => a.octets(),
                };
                match toyos_abi::syscall::write_nonblock(rx_write_fd, &buf[..n]) {
                    Ok(written) if written == n => client.result(&UdpRecvResponse {
                        addr,
                        port: endpoint.endpoint.port,
                        len: n as u16,
                    }),
                    _ => client.error(ERR_RESOURCE_EXHAUSTED),
                }
            }
            Err(_) => client.error(ERR_OTHER),
        }
        true
    }

    fn handle_udp_recv_from(&mut self, msg: Request, socket_set: &mut SocketSet<'_>) {
        let Ok(req) = ipc::decode_payload::<UdpRecvFromRequest>(msg.payload()) else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };
        let Some(SocketKind::Udp(handle)) = self.sockets.get(&req.socket_id) else {
            msg.client.error(ERR_NOT_CONNECTED);
            return;
        };
        let handle = *handle;
        let Some(pipes) = self.udp_pipes.get(&req.socket_id) else {
            msg.client.error(ERR_NOT_CONNECTED);
            return;
        };
        let rx_write_fd = pipes.rx_write_fd.fd();
        let socket = socket_set.get_mut::<udp::Socket>(handle);

        if Self::deliver_datagram(&msg.client, socket, req.max_len, rx_write_fd) {
            return;
        }

        // Nothing has arrived yet: keep the connection open until one does.
        self.pending_udp_recvs.push(PendingUdpRecv {
            client: msg.client,
            socket_id: req.socket_id,
            max_len: req.max_len,
            deadline: None,
        });
    }

    fn handle_udp_close(&mut self, msg: &Request, socket_set: &mut SocketSet<'_>) {
        let Ok(req) = ipc::decode_payload::<SocketCloseRequest>(msg.payload()) else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };
        if let Some(SocketKind::Udp(handle)) = self.sockets.remove(&req.socket_id) {
            socket_set.get_mut::<udp::Socket>(handle).close();
            socket_set.remove(handle);
            self.udp_pipes.remove(&req.socket_id);
        }
        msg.client.done();
    }

    fn handle_dns_lookup(
        &mut self,
        msg: Request,
        socket_set: &mut SocketSet<'_>,
        iface: &mut Interface,
    ) {
        let Ok(hostname) = std::str::from_utf8(msg.payload()) else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };

        if let Ok(ip) = hostname.parse::<std::net::Ipv4Addr>() {
            let octets = ip.octets();
            let mut resp = vec![1u8];
            resp.push(4);
            resp.extend_from_slice(&octets);
            msg.client.result_bytes(&resp);
            return;
        }

        let dns = socket_set.get_mut::<dns::Socket>(self.dns_handle);
        match dns.start_query(iface.context(), hostname, DnsQueryType::A) {
            // Async — hold the connection until the query resolves.
            Ok(query) => self.pending_dns.push(PendingDns { client: msg.client, query }),
            Err(_) => msg.client.error(ERR_OTHER),
        }
    }

    fn handle_tcp_set_option(&mut self, msg: &Request, socket_set: &mut SocketSet<'_>) {
        let Ok(req) = ipc::decode_payload::<SocketOptionRequest>(msg.payload()) else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };
        let Some(SocketKind::TcpStream(handle)) = self.sockets.get(&req.socket_id) else {
            msg.client.error(ERR_NOT_CONNECTED);
            return;
        };
        let socket = socket_set.get_mut::<tcp::Socket>(*handle);
        match req.option {
            OPT_NODELAY => {
                socket.set_nagle_enabled(req.value == 0);
                msg.client.done();
            }
            _ => msg.client.error(ERR_INVALID_INPUT),
        }
    }

    fn handle_tcp_get_option(&mut self, msg: &Request, socket_set: &mut SocketSet<'_>) {
        let Ok(req) = ipc::decode_payload::<SocketOptionRequest>(msg.payload()) else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };
        let Some(SocketKind::TcpStream(handle)) = self.sockets.get(&req.socket_id) else {
            msg.client.error(ERR_NOT_CONNECTED);
            return;
        };
        let socket = socket_set.get_mut::<tcp::Socket>(*handle);
        match req.option {
            OPT_NODELAY => {
                let val = if socket.nagle_enabled() { 0u32 } else { 1u32 };
                msg.client.result(&SocketOptionResponse { value: val });
            }
            _ => msg.client.error(ERR_INVALID_INPUT),
        }
    }

    // --- Piped socket handlers ---

    fn handle_tcp_connect_piped(
        &mut self,
        msg: Request,
        socket_set: &mut SocketSet<'_>,
        iface: &mut Interface,
    ) {
        let Ok(req) = ipc::decode_payload::<TcpConnectPipedRequest>(msg.payload()) else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };
        // Refused before the socket exists, so a refusal leaves nothing to
        // unwind and no SYN on the wire. An error return, never a panic: the
        // request is a client's and asking for one connection too many is not
        // a bug in netd.
        //
        // Not `ERR_CONNECTION_REFUSED`, which this file already uses below for
        // a pending connect whose socket reached `Closed` — the peer's answer.
        // On one code a client cannot tell "this machine is full, back off"
        // from "that peer says no, give up".
        if !self.piped_room() {
            say!(
                "netd: refusing connect, {} piped connections already (max {})",
                self.piped_live(),
                self.max_piped_connections,
            );
            msg.client.error(ERR_RESOURCE_EXHAUSTED);
            return;
        }
        // Taken before the socket exists, for the same reason the capacity
        // check is: a missing pair leaves nothing to unwind and no SYN on the
        // wire.
        let Some(pipes) = DataPipes::take(&msg.client) else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };
        let remote = IpEndpoint::new(
            IpAddress::Ipv4(Ipv4Addr::from(req.addr)),
            req.port,
        );
        let local_port = self.alloc_port();

        let rx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_SOCKET_BUFFER]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_SOCKET_BUFFER]);
        let mut socket = tcp::Socket::new(rx_buf, tx_buf);
        if socket.connect(iface.context(), remote, local_port).is_err() {
            msg.client.error(ERR_CONNECTION_REFUSED);
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

        // Async — hold the connection until the handshake completes.
        self.pending_piped_connects.push(PendingPipedConnect {
            client: msg.client,
            socket_id,
            handle,
            pipes,
            deadline,
        });
    }

    fn handle_tcp_bind_piped(&mut self, msg: &Request, socket_set: &mut SocketSet<'_>) {
        let Ok(req) = ipc::decode_payload::<TcpBindPipedRequest>(msg.payload()) else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };
        let port = if req.port == 0 { self.alloc_port() } else { req.port };

        // Take the pipe before the socket goes into socket_set: a missing one
        // then has no half-built socket to unwind.
        let Some([notify]) = msg.client.conn.recv_handles_exact::<{ NOTIFY_HANDLES }>() else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };
        let notify_write_fd = unsafe { Pipe::from_raw(notify) };

        let rx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_SOCKET_BUFFER]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_SOCKET_BUFFER]);
        let mut socket = tcp::Socket::new(rx_buf, tx_buf);
        if socket.listen(port).is_err() {
            msg.client.error(ERR_ADDR_IN_USE);
            return;
        }

        let handle = socket_set.add(socket);
        let socket_id = self.alloc_id();
        self.sockets.insert(socket_id, SocketKind::TcpListener(handle));

        let notify_ring = map_pipe_ring(&notify_write_fd);
        self.piped_listeners.insert(socket_id, PipedListener {
            handle,
            notify_write_fd,
            notify_ring,
            notified: false,
        });

        msg.client.result(&TcpBindResponse {
            socket_id,
            bound_port: port,
            _pad: 0,
        });
    }

    fn handle_tcp_accept_piped(&mut self, msg: &Request, socket_set: &mut SocketSet<'_>) {
        let Ok(req) = ipc::decode_payload::<TcpAcceptPipedRequest>(msg.payload()) else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };
        if !self.piped_room() {
            say!(
                "netd: refusing accept, {} piped connections already (max {})",
                self.piped_live(),
                self.max_piped_connections,
            );
            msg.client.error(ERR_RESOURCE_EXHAUSTED);
            return;
        }
        let Some(pipes) = DataPipes::take(&msg.client) else {
            msg.client.error(ERR_INVALID_INPUT);
            return;
        };
        let Some(listener) = self.piped_listeners.get(&req.socket_id) else {
            msg.client.error(ERR_NOT_CONNECTED);
            return;
        };

        let socket = socket_set.get_mut::<tcp::Socket>(listener.handle);
        // Not `is_active`: that is already true in SynReceived, where
        // `remote_endpoint()` is still None.
        if socket.state() != tcp::State::Established {
            msg.client.error(ERR_NOT_CONNECTED);
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

        self.piped_connections.push(piped_connection(old_handle, pipes));

        // Create replacement listener
        let rx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_SOCKET_BUFFER]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_SOCKET_BUFFER]);
        let mut new_listener = tcp::Socket::new(rx_buf, tx_buf);
        new_listener.listen(local_port).ok();
        let new_handle = socket_set.add(new_listener);
        self.sockets.insert(req.socket_id, SocketKind::TcpListener(new_handle));

        if let Some(pl) = self.piped_listeners.get_mut(&req.socket_id) {
            pl.handle = new_handle;
            pl.notified = false;
        }

        msg.client.result(&TcpAcceptPipedResponse {
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

            // smoltcp rx → pipe write via kernel (ensures reader notification)
            while socket.can_recv() {
                if let Some(ref fd) = conn.rx_write_fd {
                    let mut buf = [0u8; 4096];
                    match socket.recv_slice(&mut buf) {
                        Ok(n) if n > 0 => {
                            let _ = toyos_abi::syscall::write_nonblock(fd.fd(), &buf[..n]);
                        }
                        _ => break,
                    };
                } else {
                    break;
                }
            }

            // pipe read → smoltcp tx (drain fully via kernel for proper notification)
            while socket.can_send() {
                if let Some(ref fd) = conn.tx_read_fd {
                    let mut buf = [0u8; 4096];
                    match toyos_abi::syscall::read_nonblock(fd.fd(), &mut buf) {
                        Ok(n) if n > 0 => { let _ = socket.send_slice(&buf[..n]); }
                        _ => break,
                    }
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
            // Not `is_active`: that is already true in SynReceived, before the
            // three-way handshake completes.
            if socket.state() == tcp::State::Established && !listener.notified {
                let _ = toyos_abi::syscall::write_nonblock(listener.notify_write_fd.fd(), &[1]);
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
            if let Some(_listener) = self.piped_listeners.remove(&socket_id) {
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
                pr.client.error(ERR_NOT_CONNECTED);
                self.pending_udp_recvs.swap_remove(i);
                continue;
            };
            let handle = *handle;
            let Some(pipes) = self.udp_pipes.get(&pr.socket_id) else {
                pr.client.error(ERR_NOT_CONNECTED);
                self.pending_udp_recvs.swap_remove(i);
                continue;
            };
            let rx_write_fd = pipes.rx_write_fd.fd();
            let max_len = pr.max_len;
            let socket = socket_set.get_mut::<udp::Socket>(handle);
            if Self::deliver_datagram(&self.pending_udp_recvs[i].client, socket, max_len, rx_write_fd)
            {
                self.pending_udp_recvs.swap_remove(i);
                continue;
            }
            if self.pending_udp_recvs[i].deadline.is_some_and(|d| now >= d) {
                self.pending_udp_recvs[i].client.error(ERR_TIMED_OUT);
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
                    pd.client.result_bytes(&resp);
                    self.pending_dns.swap_remove(i);
                    continue;
                }
                Err(dns::GetQueryResultError::Pending) => {
                    i += 1;
                    continue;
                }
                Err(_) => {
                    pd.client.error(ERR_OTHER);
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
                let resp = TcpConnectResponse {
                    socket_id: pc.socket_id,
                    local_port,
                    _pad: 0,
                };
                pc.client.result(&resp);
                let pc = self.pending_piped_connects.swap_remove(i);
                self.piped_connections.push(piped_connection(pc.handle, pc.pipes));
                continue;
            }
            if socket.state() == tcp::State::Closed {
                pc.client.error(ERR_CONNECTION_REFUSED);
                let (socket_id, handle) = (pc.socket_id, pc.handle);
                self.sockets.remove(&socket_id);
                socket_set.remove(handle);
                self.pending_piped_connects.swap_remove(i);
                continue;
            }
            if pc.deadline.is_some_and(|d| now >= d) {
                pc.client.error(ERR_TIMED_OUT);
                socket.abort();
                let (socket_id, handle) = (pc.socket_id, pc.handle);
                self.sockets.remove(&socket_id);
                socket_set.remove(handle);
                self.pending_piped_connects.swap_remove(i);
                continue;
            }
            i += 1;
        }
    }
}

fn main() {
    // **The order this used to have was load-bearing and is now moot.** The
    // device was claimed before the name was published, because a client that
    // connected while netd was still in `DmaNic::open` reached a listener owned
    // by a process about to return and got its request answered by nobody —
    // sshd found it, took its `panic!` arm and put a tokio backtrace across the
    // boot. There is no window left to order around: the `netd` port exists
    // before either process does, a client's connection is queued on it whether
    // or not this program ever reaches `accept`, and if netd exits the queued
    // client sees `Gone` rather than silence.
    let Some(nic) = endow::device::<NicDev>(DeviceType::Nic) else {
        say!("netd: no NIC on this machine, exiting");
        return;
    };
    let acceptor = endow::acceptor("netd")
        .expect("the manifest declares this program serves `netd`");
    let mut device = DmaNic::open(nic);
    let mac = device.mac;

    say!(
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

    let total_mem = total_memory();
    let max_piped = max_piped_connections(total_mem);
    let mut daemon = NetDaemon::new(dns_handle, max_piped);

    say!(
        "netd: ready, at most {max_piped} piped connections \
         ({} MiB each of {} MiB total)",
        PIPED_CONNECTION_BYTES / (1024 * 1024),
        total_mem / (1024 * 1024),
    );

    // Sized for the slot ceiling rather than for `max_piped`: the batch
    // between two `wait` calls is the two fixed fds, one per live piped
    // connection and one per pending connection, and the ceiling is what that
    // can never exceed.
    let poller = Poller::new(FIXED_POLL_FDS + MAX_PIPED_SLOTS as u32 + MAX_PENDING_CONNS);
    const TOKEN_LISTENER: u64 = 0;
    const TOKEN_NIC: u64 = 1;
    const TOKEN_TX_PIPE_BASE: u64 = 0x1000;
    // Clear of the tx-pipe range by more than `MAX_PIPED_SLOTS`, and of a
    // connection's own fd number by more than `MAX_FDS` (1024, `kernel/src/fd.rs`).
    const TOKEN_PENDING_BASE: u64 = 0x1_0000;

    let mut pending: Vec<PendingConn> = Vec::new();

    loop {
        let now = SmoltcpInstant::from_millis(epoch.elapsed().as_millis() as i64);
        while iface.poll(now, &mut device, &mut socket_set) != PollResult::None {}

        daemon.bridge_piped(&mut socket_set);

        daemon.check_piped_listeners(&mut socket_set);

        daemon.cleanup_dead_listeners(&mut socket_set);

        daemon.process_pending(&mut socket_set);

        let delay = iface.poll_delay(now, &socket_set);

        let has_pending_async = !daemon.pending_udp_recvs.is_empty()
            || !daemon.pending_dns.is_empty()
            || !daemon.pending_piped_connects.is_empty();
        let has_piped = !daemon.piped_connections.is_empty();

        // Use 1ms polling when piped connections are active. This is the network
        // equivalent of NAPI polling — during active I/O, poll frequently to
        // bridge data between smoltcp and kernel pipes without relying solely on
        // interrupt-driven wakeups.
        let timeout_nanos = if has_pending_async || has_piped {
            Some(Duration::from_millis(1).as_nanos() as u64)
        } else {
            match delay {
                Some(d) if d.total_millis() > 0 => Some(Duration::from_millis(d.total_millis() as u64).as_nanos() as u64),
                Some(_) => Some(Duration::from_millis(1).as_nanos() as u64),
                None => None,
            }
        };

        poller.poll_add(&acceptor, IORING_POLL_IN, TOKEN_LISTENER);
        poller.poll_add(&device.nic_fd, IORING_POLL_IN, TOKEN_NIC);

        // Submit POLL_ADD for each active tx pipe (client → netd direction)
        for (i, conn) in daemon.piped_connections.iter().enumerate() {
            if let Some(ref pipe) = conn.tx_read_fd {
                poller.poll_add(pipe, IORING_POLL_IN, TOKEN_TX_PIPE_BASE + i as u64);
            }
        }

        for p in pending.iter() {
            poller.poll_add(&p.conn, IORING_POLL_IN, TOKEN_PENDING_BASE + p.conn.fd().0 as u64);
        }

        let timeout = match timeout_nanos {
            None => u64::MAX,
            Some(n) => n,
        };
        // A client that connects and then says nothing wakes nothing, so the
        // deadline that removes it has to be a wake in its own right: without
        // this netd can sit in `wait` forever with `pending` full of clients
        // whose handshake is already over its time.
        let timeout = if pending.is_empty() {
            timeout
        } else {
            timeout.min(HANDSHAKE_TIMEOUT.as_nanos() as u64)
        };

        let mut ready: Vec<u64> = Vec::new();
        poller.wait(1, timeout, |token| ready.push(token));

        // A handshake that never completes is why this deadline exists, and the
        // sweep has to happen on a pass that found nothing ready too —
        // otherwise a silent client is only ever timed out by some *other*
        // client's traffic.
        let now_wall = Instant::now();
        for p in pending.iter().filter(|p| now_wall.duration_since(p.since) >= HANDSHAKE_TIMEOUT) {
            say!(
                "netd: dropping client {} — it never finished its request",
                p.conn.fd().0
            );
        }
        pending.retain(|p| now_wall.duration_since(p.since) < HANDSHAKE_TIMEOUT);

        // Accept and the request are two events. Nothing is read here: a client
        // that connects and then says nothing costs a slot and a deadline, not
        // the network stack.
        if ready.contains(&TOKEN_LISTENER) {
            let conn = acceptor.accept().expect("accept failed");
            if pending.len() >= MAX_PENDING_CONNS as usize {
                say!(
                    "netd: refusing client {} — {MAX_PENDING_CONNS} connections are already \
                     waiting to say what they want",
                    conn.fd().0
                );
            } else {
                pending.push(PendingConn { conn, rx: ClientRx::new(), since: Instant::now() });
            }
        }

        // `remove` rather than `swap_remove`: the entries after `i` shift down,
        // so leaving `i` alone visits each connection exactly once. At
        // `MAX_PENDING_CONNS` entries the shift is not worth a subtler loop.
        let mut requests: Vec<Request> = Vec::new();
        let mut i = 0;
        while i < pending.len() {
            let fd = pending[i].conn.fd();
            if !ready.contains(&(TOKEN_PENDING_BASE + fd.0 as u64)) {
                i += 1;
                continue;
            }
            let step = {
                let p = &mut pending[i];
                p.rx.pump(&p.conn)
            };
            match step {
                RxStep::Idle => i += 1,
                // Unlogged, and the only removal here that is: a client may
                // connect to find out whether netd exists and hang up, which is
                // its business. The two below are the client getting something
                // wrong, and those netd names.
                RxStep::Eof => {
                    pending.remove(i);
                }
                RxStep::Malformed => {
                    say!(
                        "netd: dropping client {} — it sent a frame this protocol cannot \
                         describe",
                        pending[i].conn.fd().0
                    );
                    pending.remove(i);
                }
                RxStep::Frame { msg_type, payload_len } => {
                    let mut payload = [0u8; MAX_KEPT_REQUEST];
                    payload[..payload_len].copy_from_slice(pending[i].rx.payload(payload_len));
                    let p = pending.remove(i);
                    requests.push(Request {
                        client: Client { conn: p.conn },
                        msg_type,
                        payload,
                        payload_len,
                    });
                }
            }
        }

        for request in requests {
            daemon.handle_message(request, &mut socket_set, &mut iface);
        }
    }
}

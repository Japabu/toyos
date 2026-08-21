//! Everything between a client's pipe and a decision.
//!
//! **The compositor never reads a client with a blocking read**, and never
//! writes one with a blocking write. `ipc::recv_header` and `ipc::recv_payload`
//! park the caller until the peer sends the bytes it promised, which hands a
//! client the decision of when the desktop runs again; a blocking `send` hands
//! it the same decision by not reading. Here a peer that stops halfway through
//! a frame costs a buffer and a deadline, and one that will not take a message
//! costs itself.

use std::time::{Duration, Instant};

use toyos::shm::SharedMemory;
use toyos::AsHandle;
use toyos::{ipc, Connection};
use toyos_abi::RawHandle;
use toyos_desktop::Window;

/// Connections accepted but not yet identified by a first frame.
///
/// The kernel queues 32 unaccepted connections per listener
/// (`listener::MAX_PENDING_CONNECTIONS`); this is the same allowance one step
/// further along, for a client that has been accepted and has not yet said
/// what it wants. Past it the compositor refuses by name rather than growing,
/// and [`HANDSHAKE_TIMEOUT`] is what guarantees the table drains.
pub const MAX_PENDING_CONNS: u32 = 32;

/// How long a connection may go without completing its first frame.
///
/// Policy, and generous: every client in the tree sends its first frame in the
/// statement after `connect`. What this bounds is the one that never sends it.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// The largest client payload the compositor keeps.
///
/// `MSG_CLIPBOARD_SET`'s 116 bytes is the widest of them; every typed payload
/// it decodes is smaller (`CreateWindowRequest`, 40). A client may declare
/// anything up to `ipc::MAX_FRAME_LEN` — the excess is counted down and
/// discarded, never waited for.
pub const MAX_KEPT_PAYLOAD: usize = 116;

/// The largest clipboard the compositor will hold for a client.
///
/// Two things meet here. `MSG_CLIPBOARD_SET_SHM` carries a length the client
/// chooses and the compositor reads that many bytes out of a region the client
/// sent, so an unbounded length is a read past the mapping — and the kernel
/// rounds every shared region up to one 2 MiB page
/// (`object::shm::SharedMemObject::create`), which makes a page the largest
/// length that cannot leave the smallest region anybody can send. It is also policy: a clipboard is text somebody selected,
/// and a megabyte of it is already generous.
pub const MAX_CLIPBOARD_BYTES: usize = 2 * 1024 * 1024;

/// One client's inbound framing.
pub type ClientRx = ipc::FrameRx<MAX_KEPT_PAYLOAD>;

/// What the compositor needs to reach one window's client.
///
/// This is the `C` of [`Window`]: geometry and order are `toyos-desktop`'s and
/// never look at any of it.
pub struct Client {
    pub conn: Connection,
    pub shm: SharedMemory,
    pub rx: ClientRx,
}

/// A window, with the connection behind it.
pub type Win = Window<Client>;

/// A whole client message, off the connection and in memory.
///
/// `conn` is `Some` only for the first frame on a freshly accepted connection:
/// `MSG_CREATE_WINDOW` keeps it, and every other message type answers on it
/// and lets it close.
pub struct ClientFrame {
    pub handle: RawHandle,
    pub msg_type: u32,
    payload: [u8; MAX_KEPT_PAYLOAD],
    payload_len: usize,
    pub conn: Option<Connection>,
}

impl ClientFrame {
    pub fn new(handle: RawHandle, msg_type: u32) -> Self {
        Self { handle, msg_type, payload: [0; MAX_KEPT_PAYLOAD], payload_len: 0, conn: None }
    }

    pub fn set_payload(&mut self, bytes: &[u8]) {
        self.payload[..bytes.len()].copy_from_slice(bytes);
        self.payload_len = bytes.len();
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload[..self.payload_len]
    }
}

/// A connection that has been accepted and has not yet said what it is for.
///
/// It exists because `accept` and the first frame are two events, and the
/// compositor used to fuse them with a blocking `recv_header` — so a client
/// that connected and sent four bytes owned the desktop until it disconnected.
pub struct PendingConn {
    pub conn: Connection,
    pub rx: ClientRx,
    pub since: Instant,
}

/// Why a client is going.
///
/// **Every one of these is printed, named by the connection's handle.** A
/// client is not entitled to end the compositor, but the compositor is not
/// entitled to make one vanish without saying so either: the log is the only
/// place the machine this runs on can be asked what happened. The handle is
/// this process's own slot for that connection, carrying a generation that a
/// reissued slot does not repeat — so it names one client and designates
/// nothing anywhere else. It used to be the pid the kernel reported at accept,
/// which was a designation any process could present.
#[derive(Clone, Copy)]
pub enum DropReason {
    /// A frame no protocol here can produce. The next message boundary is
    /// unlocatable, so there is nothing to resynchronise to.
    OutOfProtocol,
    /// Its pipe would not take a whole frame — an entire pipe of messages it
    /// has not read.
    NotReading,
    /// The connection is gone.
    Gone,
    /// Accepted, and never completed a first frame.
    HandshakeTimeout,
}

impl DropReason {
    pub fn why(self) -> &'static str {
        match self {
            Self::OutOfProtocol => "it sent a frame this protocol cannot describe",
            Self::NotReading => "its pipe will not take another message and it is not reading",
            Self::Gone => "its connection is gone",
            Self::HandshakeTimeout => "it never finished its first message",
        }
    }
}

impl From<ipc::TrySendError> for DropReason {
    fn from(e: ipc::TrySendError) -> Self {
        match e {
            ipc::TrySendError::Full => Self::NotReading,
            _ => Self::Gone,
        }
    }
}

/// A client the next removal pass will take out.
pub type Dead = (RawHandle, DropReason);

pub fn mark_dead(dead: &mut Vec<Dead>, handle: RawHandle, reason: DropReason) {
    if !dead.iter().any(|(f, _)| *f == handle) {
        dead.push((handle, reason));
    }
}

pub fn announce(dead: &[Dead]) {
    for (handle, reason) in dead {
        eprintln!("compositor: dropping client {} — {}", handle.0, reason.why());
    }
}

/// Say that a window was deliberately closed, and by what.
///
/// A client that *dies* is already announced — `dropping client N — why` — and
/// a window somebody closed was not, so the desktop's only record of one was the
/// `windows=N` count in the statistics line. That is a sample of a level taken
/// every two seconds: two closes inside one interval are indistinguishable
/// from one, and from none if a window opened in between. Every report the
/// owner has made about this desktop begins "I closed a window and then", so
/// which window went, why, and how many are left is the first thing anyone
/// asks of the log — and a caller that re-sends a close because it could not
/// tell whether the first one landed closes the next window down.
pub fn note_closed(by: &str, client: RawHandle, remaining: usize) {
    eprintln!("compositor: window closed client={} by {by}, {remaining} left", client.0);
}

/// Hand a window a typed frame, or mark it for removal.
///
/// A failure is never retried and never ignored: `TrySendError::Full` can have
/// left part of the frame in the pipe, so the peer's stream is past saving —
/// which is the price of never blocking on it.
pub fn deliver<T: ipc::IpcPayload>(dead: &mut Vec<Dead>, win: &Win, msg_type: u32, payload: &T) {
    if let Err(e) = win.client.conn.try_send(msg_type, payload) {
        mark_dead(dead, win.client.conn.as_handle(), e.into());
    }
}

/// [`deliver`] for a message whose payload names buffers that travel with it.
///
/// The handles are moved whether or not the frame lands, so the caller has
/// already given them up — and a client dropped here drops the queue holding
/// them, which is what releases the region.
pub fn deliver_with_handles<T: ipc::IpcPayload>(
    dead: &mut Vec<Dead>,
    win: &Win,
    handles: &[RawHandle],
    msg_type: u32,
    payload: &T,
) {
    if let Err(e) = win.client.conn.try_send_with_handles(handles, msg_type, payload) {
        mark_dead(dead, win.client.conn.as_handle(), e.into());
    }
}

/// [`deliver`] for a message that is only its own header.
pub fn deliver_signal(dead: &mut Vec<Dead>, win: &Win, msg_type: u32) {
    if let Err(e) = win.client.conn.try_signal(msg_type) {
        mark_dead(dead, win.client.conn.as_handle(), e.into());
    }
}

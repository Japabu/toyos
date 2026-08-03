//! Key events down the surface tree.
//!
//! A **surface** is something with a screen and a keyboard: the compositor's
//! windows, `/bin/terminal`, `/bin/console`. Its owner reads key transitions
//! from whatever is above it and decides what they mean — which for a terminal
//! is bytes on its child's stdin, through a [`toyos_keymap::Translator`] it
//! owns alone.
//!
//! A child that wants the transitions themselves rather than the bytes asks
//! for them here. `locale detect` is the reason this exists: it identifies a
//! keyboard by which HID usage a *labelled* key reports, so the one thing it
//! must not be handed is the current layout's opinion of that key. The
//! keyboard device is claimed exclusively, and under a desktop or a console
//! the surface owner holds it, so before this the wizard refused to run at all
//! wherever anyone would actually run it.
//!
//! Two things travel this channel and nothing else:
//!
//! - **The grab.** A client asks for raw keys; the host answers granted or
//!   refused, and while a grab is held the surface's own translation stops.
//!   The answer is not optional — a client that assumed it had the keys would
//!   wait forever for events going somewhere else.
//! - **A layout change.** [`LAYOUT_CONFIG`] is the layout, and the message
//!   carries no name: it says only that the file changed, so no translator can
//!   hold an opinion that disagrees with it.
//!
//! The service name is per-host and lives in [`HOST_ENV`], which a child
//! inherits — so a wizard three processes below a terminal still finds it.

use toyos_abi::input::RawKeyEvent;
use toyos_abi::syscall::SyscallError;
use toyos_abi::Fd;

use crate::ipc::{self, Connection, FrameRx, RxStep, TrySendError};
use crate::poller::{Poller, IORING_POLL_IN};
use crate::services;
use crate::{AsHandle, Listener};

/// The environment variable carrying a surface's service name.
pub const HOST_ENV: &str = "TOYOS_SURFACE";

/// The file that says which keyboard layout this machine uses.
///
/// One path, read by every translator and written by `locale` alone. It is on
/// tmpfs until a writable store lands, so it survives a login and not a
/// reboot.
pub const LAYOUT_CONFIG: &str = "/home/root/.config/keyboard_layout";

/// SAFETY: `RawKeyEvent` is `#[repr(C)]` and two `u8`s, so it has no padding
/// and no pointers, and every bit pattern of both fields is a valid value —
/// which is what makes reinterpreting bytes a peer chose sound.
unsafe impl ipc::IpcPayload for RawKeyEvent {}

/// Client → host: send me key transitions instead of translating them.
pub const MSG_GRAB_KEYS: u32 = 1;
/// Either direction: [`LAYOUT_CONFIG`] changed, re-read it.
pub const MSG_LAYOUT_CHANGED: u32 = 2;
/// Host → client: the keys are yours until you disconnect.
pub const MSG_GRAB_GRANTED: u32 = 3;
/// Host → client: another client on this surface already has them.
pub const MSG_GRAB_REFUSED: u32 = 4;
/// Host → client: one transition, payload [`RawKeyEvent`].
pub const MSG_KEY: u32 = 5;

/// How many clients one surface will hold at once.
///
/// Policy, not physics. A surface's clients are the programs running under it
/// that want raw keys, and outside a test that is one wizard at a time; four
/// is room for a shell pipeline that spawned several. Past it a connection is
/// accepted and closed, which the client sees as [`GrabError::HostGone`] —
/// never a queue, because a client waiting behind three others for keys the
/// user is pressing now would be worse than being told no.
pub const MAX_CLIENTS: usize = 4;

/// The service name a surface owned by `pid` serves.
///
/// Derived rather than recorded: the host puts the same string in [`HOST_ENV`]
/// and there is nothing for the two to disagree about.
pub fn service_name(pid: u32, buf: &mut [u8; MAX_NAME]) -> &str {
    const PREFIX: &[u8] = b"surface.";
    buf[..PREFIX.len()].copy_from_slice(PREFIX);
    let mut n = PREFIX.len();
    let mut digits = [0u8; 10];
    let mut d = 0;
    let mut v = pid;
    loop {
        digits[d] = b'0' + (v % 10) as u8;
        d += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    while d > 0 {
        d -= 1;
        buf[n] = digits[d];
        n += 1;
    }
    core::str::from_utf8(&buf[..n]).expect("ASCII")
}

/// Widest [`service_name`] there is: the prefix plus a `u32`'s ten digits.
pub const MAX_NAME: usize = 8 + 10;

/// Something the surface owner should log or act on.
#[derive(Clone, Copy, Debug)]
pub enum Notice {
    /// A client says [`LAYOUT_CONFIG`] changed. Re-read it, and pass the same
    /// message on to whatever is above and below this surface.
    LayoutChanged,
    /// This client now takes the key transitions the surface would have
    /// translated.
    Grabbed { pid: u32 },
    /// The client holding the grab has gone; translation resumes.
    Released { pid: u32 },
    /// A peer was dropped, and why — for the log line that names it.
    Dropped { pid: u32, why: &'static str },
}

/// What [`Host::deliver`] did with a transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Nobody holds the grab. The surface translates it as usual.
    NotGrabbed,
    /// Handed to the client holding the grab.
    Sent,
}

struct Peer {
    conn: Connection,
    pid: u32,
    rx: FrameRx<0>,
}

/// The server side: a surface serving key transitions to its children.
pub struct Host {
    listener: Listener,
    peers: [Option<Peer>; MAX_CLIENTS],
    /// Index into `peers` of the client taking transitions.
    grab: Option<usize>,
    /// One slot per peer, so a drop decided inside `deliver` cannot be lost
    /// before the caller next asks for notices. A peer is dropped once, and
    /// its slot is written at that moment and read by the next [`Host::poll`].
    drops: [Option<(u32, &'static str)>; MAX_CLIENTS],
    pending: [Option<Notice>; MAX_CLIENTS],
}

impl Host {
    /// Serve `name`, which the caller must also put in [`HOST_ENV`] for the
    /// children it spawns.
    pub fn listen(name: &str) -> Result<Self, SyscallError> {
        Ok(Self {
            listener: services::listen(name)?,
            peers: [const { None }; MAX_CLIENTS],
            grab: None,
            drops: [const { None }; MAX_CLIENTS],
            pending: [const { None }; MAX_CLIENTS],
        })
    }

    /// The listener, for the caller's poller.
    pub fn listener_fd(&self) -> Fd {
        self.listener.fd()
    }

    /// Every connected client, for the caller's poller.
    pub fn client_fds(&self) -> impl Iterator<Item = Fd> + '_ {
        self.peers.iter().flatten().map(|p| p.conn.fd())
    }

    /// Widest handle set a host adds to its owner's poller.
    pub const POLL_HANDLES: u32 = 1 + MAX_CLIENTS as u32;

    pub fn grabbed(&self) -> bool {
        self.grab.is_some()
    }

    /// Accept one queued connection. Call when the listener reads ready.
    ///
    /// Accept and the first frame are two events: nothing is read here, and a
    /// client that connects and then says nothing costs a slot and no more.
    pub fn accept(&mut self) {
        let Ok(accepted) = services::accept(&self.listener) else {
            return;
        };
        let Some(slot) = self.peers.iter().position(|p| p.is_none()) else {
            // Dropping the connection is the refusal: the client's next read
            // sees the hang-up.
            self.note(Notice::Dropped {
                pid: accepted.client_pid,
                why: "this surface already holds as many input clients as it will",
            });
            return;
        };
        self.peers[slot] =
            Some(Peer { conn: accepted.conn, pid: accepted.client_pid, rx: FrameRx::new() });
    }

    /// Read what every client has sent, and hand back one notice.
    ///
    /// Call until it returns `None`. Grab requests are answered here and are
    /// not notices in themselves — [`Notice::Grabbed`] reports the one that
    /// succeeded.
    pub fn poll(&mut self) -> Option<Notice> {
        for i in 0..MAX_CLIENTS {
            if let Some((pid, why)) = self.drops[i].take() {
                return Some(Notice::Dropped { pid, why });
            }
            if let Some(notice) = self.pending[i].take() {
                return Some(notice);
            }
        }
        for i in 0..MAX_CLIENTS {
            let Some(peer) = &mut self.peers[i] else { continue };
            let (pid, step) = (peer.pid, peer.rx.pump(&peer.conn));
            match step {
                RxStep::Idle => {}
                RxStep::Eof => {
                    let held_the_grab = self.grab == Some(i);
                    self.close(i);
                    if held_the_grab {
                        return Some(Notice::Released { pid });
                    }
                }
                RxStep::Malformed => {
                    self.close(i);
                    return Some(Notice::Dropped {
                        pid,
                        why: "its frame is not one this protocol can produce",
                    });
                }
                RxStep::Frame { msg_type, .. } => {
                    if let Some(notice) = self.frame(i, pid, msg_type) {
                        return Some(notice);
                    }
                }
            }
        }
        None
    }

    fn frame(&mut self, i: usize, pid: u32, msg_type: u32) -> Option<Notice> {
        match msg_type {
            MSG_GRAB_KEYS => {
                let free = self.grab.is_none();
                let answer = if free { MSG_GRAB_GRANTED } else { MSG_GRAB_REFUSED };
                let taken = self.peers[i]
                    .as_ref()
                    .expect("the slot this frame came off")
                    .conn
                    .try_signal(answer)
                    .is_ok();
                if !taken {
                    self.close(i);
                    return Some(Notice::Dropped {
                        pid,
                        why: "it would not take the answer to its own request",
                    });
                }
                if free {
                    self.grab = Some(i);
                    return Some(Notice::Grabbed { pid });
                }
                None
            }
            MSG_LAYOUT_CHANGED => Some(Notice::LayoutChanged),
            _ => {
                self.close(i);
                Some(Notice::Dropped { pid, why: "it sent a message this surface does not serve" })
            }
        }
    }

    /// Hand one transition to the client holding the grab.
    ///
    /// [`Delivery::NotGrabbed`] is the caller's cue to translate it itself.
    pub fn deliver(&mut self, event: RawKeyEvent) -> Delivery {
        let Some(i) = self.grab else {
            return Delivery::NotGrabbed;
        };
        let peer = self.peers[i].as_ref().expect("the grab names a live peer");
        let (pid, sent) = (peer.pid, peer.conn.try_send(MSG_KEY, &event));
        match sent {
            Ok(()) => Delivery::Sent,
            other => {
                self.refused(i, pid, other, "it would not take a key event it asked for");
                Delivery::NotGrabbed
            }
        }
    }

    /// Tell every client that [`LAYOUT_CONFIG`] changed.
    pub fn notify_layout(&mut self) {
        for i in 0..MAX_CLIENTS {
            let Some(peer) = &self.peers[i] else { continue };
            let (pid, sent) = (peer.pid, peer.conn.try_signal(MSG_LAYOUT_CHANGED));
            self.refused(i, pid, sent, "it would not take a layout notification");
        }
    }

    /// Close a peer a write did not reach, and decide whether that is worth
    /// reporting.
    ///
    /// **A peer that has gone is not a peer at fault.** A client sends
    /// [`MSG_LAYOUT_CHANGED`] and exits, and the broadcast that follows lands
    /// on its closed pipe — the write fails with a syscall error, which is
    /// what a hang-up looks like from this side. Only a *short* write is a
    /// fault: it means the pipe is full, so the peer has a backlog of messages
    /// it asked for and never read, and there is no way to retract the half a
    /// frame the kernel took.
    fn refused(&mut self, i: usize, pid: u32, sent: Result<(), TrySendError>, why: &'static str) {
        match sent {
            Ok(()) => {}
            Err(TrySendError::Syscall(_)) => self.close(i),
            Err(TrySendError::Full) | Err(TrySendError::TooLarge) => {
                self.close(i);
                self.drops[i] = Some((pid, why));
            }
        }
    }

    fn close(&mut self, i: usize) {
        self.peers[i] = None;
        if self.grab == Some(i) {
            self.grab = None;
        }
    }

    fn note(&mut self, notice: Notice) {
        if let Some(slot) = self.pending.iter().position(|n| n.is_none()) {
            self.pending[slot] = Some(notice);
        }
    }
}

/// Why a client could not take the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrabError {
    /// Nothing is serving this surface's name — the host went away, or the
    /// name in [`HOST_ENV`] is stale.
    HostGone,
    /// Another client on the same surface already holds them.
    Busy,
    /// The host answered the request with something else.
    Protocol(u32),
}

impl core::fmt::Display for GrabError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::HostGone => write!(f, "the surface hosting this program is not answering"),
            Self::Busy => write!(f, "something else on this surface already has the keyboard"),
            Self::Protocol(t) => write!(f, "the surface answered with message type {t}"),
        }
    }
}

/// The client side: key transitions from this program's surface.
pub struct Keys {
    conn: Connection,
    poller: Poller,
}

impl Keys {
    /// Ask the surface serving `name` for raw key transitions.
    pub fn grab(name: &str) -> Result<Self, GrabError> {
        let conn = services::connect(name).map_err(|_| GrabError::HostGone)?;
        conn.signal(MSG_GRAB_KEYS).map_err(|_| GrabError::HostGone)?;
        let header = conn.recv_header().map_err(|_| GrabError::HostGone)?;
        match header.msg_type {
            MSG_GRAB_GRANTED => Ok(Self { conn, poller: Poller::new(1) }),
            MSG_GRAB_REFUSED => Err(GrabError::Busy),
            other => Err(GrabError::Protocol(other)),
        }
    }

    /// The next transition, or `None` when `timeout_nanos` passes.
    ///
    /// A layout change arriving on this channel is consumed and not reported:
    /// a client holding the keys is reading usages, which no layout moves.
    pub fn next(&mut self, timeout_nanos: u64) -> Option<RawKeyEvent> {
        loop {
            self.poller.poll_add(&self.conn, IORING_POLL_IN, 0);
            let mut ready = false;
            self.poller.wait(1, timeout_nanos, |_| ready = true);
            if !ready {
                return None;
            }
            let header = self.conn.recv_header().ok()?;
            match header.msg_type {
                MSG_KEY => return self.conn.recv_payload::<RawKeyEvent>(&header).ok(),
                MSG_LAYOUT_CHANGED => {}
                _ => return None,
            }
        }
    }
}

impl AsHandle for Keys {
    fn as_handle(&self) -> Fd {
        self.conn.fd()
    }
}

/// Tell the surface serving `name` that [`LAYOUT_CONFIG`] changed.
///
/// Best effort by construction: the config is already written, so a program
/// with no surface above it has still changed the layout — for the next
/// translator that starts. There is nothing here to report.
pub fn notify_layout_changed(name: &str) {
    if let Ok(conn) = services::connect(name) {
        let _: Result<(), ipc::IpcError> = conn.signal(MSG_LAYOUT_CHANGED);
    }
}

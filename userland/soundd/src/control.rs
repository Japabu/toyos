//! The connections, the framing, and what a client may ask for.
//!
//! One thread, one poller, one handle per client plus the acceptor. Everything
//! it decides is decided before anything is done: [`classify`] says whether a
//! framed message is one this client may send at all, and [`reject_open`] says
//! whether the stream it asks for is one the mixer can carry.
//!
//! **soundd never reads a client with a blocking read**, which is
//! `userland/CLAUDE.md`'s doctrine and this file's whole shape: one client
//! parking a partial header would wedge accept and volume/close/disconnect
//! handling for every other client.

use toyos::audio::{
    StreamOpenRequest, StreamSetVolume, FORMAT_S16LE, MSG_STREAM_CLOSE, MSG_STREAM_ERROR,
    MSG_STREAM_OPEN, MSG_STREAM_SET_VOLUME,
};
use toyos::ipc::{self, RxStep};
use toyos::poller::{Poller, READABLE};
use toyos::port::Acceptor;
use toyos::Connection;
use toyos_abi::RawHandle;
use toyos_mixer::{period_nanos, Gain, MAX_CLIENT_RATE, MIN_CLIENT_RATE};

use crate::client::{open_stream, Departure};
use crate::command::{remove, submit, CommandRing, MixCommand};

/// Control connections soundd will hold at once.
///
/// The control thread watches one handle per client plus the acceptor in a
/// single poller and io_uring rings are powers of two, so the limit is a ring
/// size minus one. 64 costs the same 2 MiB page as 32; 63 simultaneous streams
/// is already past what the mixer renders inside one 2.9 ms period, and costs
/// 189 of the kernel's 4096 handle slots (a control connection plus both signal
/// pipe ends per client).
pub(crate) const MAX_CONTROL_CLIENTS: usize = 63;

/// The widest control payload soundd decodes, **plus one**.
///
/// `StreamOpenRequest` is the only payload wider than a bare signal. The `+ 1`
/// is what keeps an over-long frame a refusal rather than a truncation, and it
/// is the one behaviour the hand-written buffer this replaced had that the
/// SDK's does not: `FrameRx` keeps `min(declared, N)` bytes, so with `N` equal
/// to the struct, a client declaring a hundred bytes of `MSG_STREAM_OPEN` would
/// report exactly the struct's width and open a stream from its first eight.
/// One byte of slack makes that client report one more than the struct instead,
/// which [`classify`]'s exact-width guard refuses by name. Nothing legitimate
/// reaches it — the SDK sends the struct — so the byte costs a byte of stack
/// and buys back a refusal.
const MAX_KEPT_PAYLOAD: usize = core::mem::size_of::<StreamOpenRequest>() + 1;

const _: () = assert!(
    core::mem::size_of::<StreamSetVolume>() < MAX_KEPT_PAYLOAD,
    "a control payload wider than the frame buffer would be refused as malformed",
);

/// One client's inbound framing.
///
/// **soundd never reads a client with a blocking read.** `ipc::recv_header` and
/// `ipc::recv_payload` park the caller until the peer sends the bytes it
/// promised, which hands a client the decision of when the control thread runs
/// again: one client parking a partial header would wedge accept and
/// volume/close/disconnect handling for every other client (the mix thread is
/// unaffected either way). This was soundd's own buffer until the SDK's grew a
/// non-blocking one; the doctrine is `userland/CLAUDE.md`'s, and the type is
/// the same one init, the compositor, netd and every surface host read with.
///
/// A client may declare anything up to `ipc::MAX_FRAME_LEN`; the excess past
/// [`MAX_KEPT_PAYLOAD`] is counted down and discarded rather than waited for, so
/// the connection stays framed whatever a peer declares — the SDK's rule, which
/// the compositor states too (`compositor::client::MAX_KEPT_PAYLOAD`). What such
/// a frame is *worth* is [`classify`]'s answer rather than this type's: it
/// arrives reporting exactly `MAX_KEPT_PAYLOAD` bytes, one more than any payload
/// here is wide, and is refused there.
type ControlRx = ipc::FrameRx<MAX_KEPT_PAYLOAD>;

/// What one framed control message asks for, decided before anything is done.
///
/// This is the whole of the framing soundd still owns: [`ControlRx`] decides
/// where a frame ends, and this decides whether what came out of it is a
/// message this client may send at all. A payload that is not exactly as long
/// as the struct its type names is refused here rather than decoded — the
/// `expect` on the open path rests on this guard and not on the peer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Control {
    Open,
    SetVolume { idx: usize },
    Close { idx: usize },
    /// An unknown message type, a payload the wrong width for its type, or a
    /// message this connection is not in the state to send. All three are the
    /// client getting it wrong, and all three end the connection.
    Violation,
}

fn classify(msg_type: u32, payload_len: usize, stream_idx: Option<usize>) -> Control {
    // **A frame wider than anything this server decodes is a violation whatever
    // its type**, and this is where the buffer this replaced refused it — at the
    // header, before the type was looked at. `FrameRx` reports an over-long
    // frame as `MAX_KEPT_PAYLOAD` exactly (see that constant's `+ 1`), so one
    // comparison is the whole of that refusal.
    if payload_len >= MAX_KEPT_PAYLOAD {
        return Control::Violation;
    }
    match (msg_type, stream_idx) {
        (MSG_STREAM_OPEN, None) if payload_len == core::mem::size_of::<StreamOpenRequest>() => {
            Control::Open
        }
        (MSG_STREAM_SET_VOLUME, Some(idx))
            if payload_len == core::mem::size_of::<StreamSetVolume>() =>
        {
            Control::SetVolume { idx }
        }
        (MSG_STREAM_CLOSE, Some(idx)) => Control::Close { idx },
        _ => Control::Violation,
    }
}

fn reject_open(req: &StreamOpenRequest) -> Option<&'static str> {
    if req.format != FORMAT_S16LE {
        return Some("unsupported sample format");
    }
    if req.channels != 1 && req.channels != 2 {
        return Some("unsupported channel count");
    }
    if !(MIN_CLIENT_RATE..=MAX_CLIENT_RATE).contains(&req.sample_rate) {
        return Some("unsupported sample rate");
    }
    None
}

pub(crate) fn control_thread(
    acceptor: Acceptor,
    cmd_ring: &CommandRing,
    cmd_pipe_write: RawHandle,
    device_sample_rate: u32,
    device_channels: u16,
    device_period_frames: u32,
    slot_count: u32,
    ramp_frames: u32,
) {
    // One handle per client plus the acceptor; `MAX_CONTROL_CLIENTS` is derived
    // from this ring, so the set always fits in one batch.
    let poller = Poller::new(MAX_CONTROL_CLIENTS as u32 + 1);
    let period_nanos = period_nanos(device_period_frames as u64, device_sample_rate as u64);

    struct ControlClient {
        conn: Connection,
        rx: ControlRx,
        // Set once MSG_STREAM_OPEN succeeds; accepted-but-silent connections
        // stay pending so they cannot stall the control plane.
        stream_idx: Option<usize>,
        /// Latest volume this client asked for, not yet handed to the mix
        /// thread. Volume is state, not an event: `set_target` overwrites the
        /// ramp outright, so applying N of them within one mix cycle leaves
        /// exactly what applying only the last would. Collapsing is therefore
        /// lossless, and it keeps a client's message rate off the command ring
        /// — the drain loop below reads everything a client has written before
        /// it yields.
        pending_volume: Option<Gain>,
    }

    let mut clients: Vec<ControlClient> = Vec::new();
    let mut next_idx: usize = 0;

    const TOKEN_ACCEPT: u64 = u64::MAX;

    loop {
        poller.watch(&acceptor, READABLE, TOKEN_ACCEPT);
        for (i, client) in clients.iter().enumerate() {
            poller.watch(&client.conn, READABLE, i as u64);
        }

        let mut ready: Vec<u64> = Vec::new();
        poller.wait(1, u64::MAX, |t| ready.push(t));

        if ready.contains(&TOKEN_ACCEPT) {
            match acceptor.accept() {
                // Refused rather than left queued: a connection past the
                // poller's watchable set would never be read from. It is still
                // accepted first — leaving it in the port's queue keeps the
                // acceptor readable and spins this loop.
                Ok(conn) if clients.len() >= MAX_CONTROL_CLIENTS => {
                    say!("soundd: refusing connection, {MAX_CONTROL_CLIENTS} clients already connected");
                    let _ = conn.signal(MSG_STREAM_ERROR);
                }
                Ok(conn) => {
                    clients.push(ControlClient {
                        conn,
                        rx: ControlRx::new(),
                        stream_idx: None,
                        pending_volume: None,
                    });
                }
                Err(e) => say!("soundd: accept failed: {e:?}"),
            }
        }

        let mut dead: Vec<usize> = Vec::new();
        for i in 0..clients.len() {
            if !ready.contains(&(i as u64)) {
                continue;
            }
            let mut disconnected = false;
            'msgs: loop {
                let step = {
                    let c = &mut clients[i];
                    c.rx.pump(&c.conn)
                };
                let (msg_type, payload_len) = match step {
                    RxStep::Idle => break 'msgs,
                    // The client's process is gone; whether it exited or
                    // crashed is not knowable here.
                    RxStep::Eof => {
                        if let Some(idx) = clients[i].stream_idx {
                            remove(cmd_ring, cmd_pipe_write, idx, Departure::Disconnected, period_nanos);
                        }
                        dead.push(i);
                        disconnected = true;
                        break 'msgs;
                    }
                    // A length no frame here can carry. Nothing after it can be
                    // located, so there is nothing to resynchronise to — and
                    // unlike the EOF above, this one soundd caused.
                    RxStep::Malformed => {
                        say!("soundd: frame this protocol cannot describe, disconnecting client");
                        if let Some(idx) = clients[i].stream_idx {
                            remove(cmd_ring, cmd_pipe_write, idx, Departure::Refused, period_nanos);
                        }
                        dead.push(i);
                        disconnected = true;
                        break 'msgs;
                    }
                    RxStep::Frame { msg_type, payload_len } => (msg_type, payload_len),
                };
                // The payload travels with the frame instead of being read off
                // the connection during dispatch: the read side is finished before
                // anything below acts on the message.
                let mut payload = [0u8; MAX_KEPT_PAYLOAD];
                payload[..payload_len].copy_from_slice(clients[i].rx.payload(payload_len));
                match classify(msg_type, payload_len, clients[i].stream_idx) {
                    Control::Open => {
                        let req: StreamOpenRequest = ipc::decode_payload(&payload[..payload_len])
                            .expect("a payload as long as the struct decodes");
                        if let Some(reason) = reject_open(&req) {
                            say!("soundd: rejecting stream ({reason}): {}Hz {}ch fmt={}",
                                req.sample_rate, req.channels, req.format);
                            let _ = clients[i].conn.signal(MSG_STREAM_ERROR);
                            dead.push(i);
                            disconnected = true;
                            break 'msgs;
                        }
                        say!("soundd: opening stream: {}Hz {}ch fmt={}",
                            req.sample_rate, req.channels, req.format);
                        let idx = next_idx;
                        next_idx += 1;
                        let Some(client) = open_stream(
                            idx,
                            &req,
                            &clients[i].conn,
                            device_sample_rate,
                            device_channels,
                            device_period_frames,
                            slot_count,
                            ramp_frames,
                        ) else {
                            let _ = clients[i].conn.signal(MSG_STREAM_ERROR);
                            dead.push(i);
                            disconnected = true;
                            break 'msgs;
                        };
                        submit(cmd_ring, cmd_pipe_write, MixCommand::AddClient(Box::new(client)), period_nanos);
                        clients[i].stream_idx = Some(idx);
                    }
                    Control::SetVolume { idx } => {
                        let raw = f32::from_le_bytes(payload[0..4].try_into().unwrap());
                        let Some(gain) = Gain::from_wire(raw) else {
                            say!("soundd: volume is not a number, disconnecting client");
                            remove(cmd_ring, cmd_pipe_write, idx, Departure::Refused, period_nanos);
                            dead.push(i);
                            disconnected = true;
                            break 'msgs;
                        };
                        clients[i].pending_volume = Some(gain);
                    }
                    Control::Close { idx } => {
                        remove(cmd_ring, cmd_pipe_write, idx, Departure::Closed, period_nanos);
                        dead.push(i);
                        disconnected = true;
                        break 'msgs;
                    }
                    Control::Violation => {
                        say!("soundd: protocol violation (msg {msg_type}), disconnecting client");
                        if let Some(idx) = clients[i].stream_idx {
                            remove(cmd_ring, cmd_pipe_write, idx, Departure::Refused, period_nanos);
                        }
                        dead.push(i);
                        disconnected = true;
                        break 'msgs;
                    }
                }
            }
            // One command for however many volume messages that drain carried.
            // A disconnecting client is skipped: its `RemoveClient` is already
            // queued, and a `SetVolume` behind it would aim the ramp at the
            // volume instead of at silence and strand the stream unremoved.
            if !disconnected {
                if let (Some(client_id), Some(target)) = (clients[i].stream_idx, clients[i].pending_volume.take()) {
                    submit(cmd_ring, cmd_pipe_write, MixCommand::SetVolume { client_id, target }, period_nanos);
                }
            }
        }
        for &i in dead.iter().rev() {
            clients.remove(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toyos::audio::MSG_STREAM_OPENED;

    /// Every framing decision soundd still makes for itself, with a lying peer
    /// on the other side.
    ///
    /// `ipc::FrameRx` decides where a frame ends and this decides whether what
    /// came out of it is a message the client may send: a payload that is not
    /// exactly as wide as the struct its type names never reaches
    /// `decode_payload`, and a message arriving in the wrong state is refused
    /// whatever it carries. Both used to be inline guards on a hand-rolled
    /// buffer that nothing tested.
    ///
    /// What this cannot reach is `FrameRx`'s own reassembly — a split header, a
    /// declared length past `ipc::MAX_FRAME_LEN` — because pumping one needs a
    /// `Connection`, and reading a `Connection` on the host would issue the
    /// ToyOS `syscall` instruction at a macOS kernel. That half is gated in
    /// QEMU, and soundd has no gate of its own there yet.
    #[test]
    fn a_control_message_is_refused_unless_its_width_and_its_state_both_fit() {
        const OPEN: usize = core::mem::size_of::<StreamOpenRequest>();
        const VOL: usize = core::mem::size_of::<StreamSetVolume>();

        assert_eq!(classify(MSG_STREAM_OPEN, OPEN, None), Control::Open);
        // Every width short of the struct, which is what a peer that declared
        // less than it owes produces — and what a truncated payload looks like
        // from here.
        for short in 0..OPEN {
            assert_eq!(
                classify(MSG_STREAM_OPEN, short, None),
                Control::Violation,
                "a {short}-byte open must be refused, not decoded from whatever follows",
            );
        }
        // A second open on a connection that already carries a stream: one
        // client is one stream, and the `None` in the guard is what says so.
        assert_eq!(classify(MSG_STREAM_OPEN, OPEN, Some(3)), Control::Violation);

        assert_eq!(classify(MSG_STREAM_SET_VOLUME, VOL, Some(3)), Control::SetVolume { idx: 3 });
        // Volume before there is a stream to aim it at, and volume of the wrong
        // width — the `f32` read on the other side is unchecked, so this guard
        // is the only thing standing between a short frame and stale bytes.
        assert_eq!(classify(MSG_STREAM_SET_VOLUME, VOL, None), Control::Violation);
        for wrong in [0, VOL - 1, VOL + 1, OPEN] {
            assert_eq!(classify(MSG_STREAM_SET_VOLUME, wrong, Some(3)), Control::Violation);
        }

        // Close is a bare signal: it needs a stream and it ignores whatever a
        // client chose to attach to it, up to the width the frame buffer keeps.
        assert_eq!(classify(MSG_STREAM_CLOSE, 0, Some(3)), Control::Close { idx: 3 });
        assert_eq!(classify(MSG_STREAM_CLOSE, OPEN, Some(3)), Control::Close { idx: 3 });
        assert_eq!(classify(MSG_STREAM_CLOSE, 0, None), Control::Violation);

        // **The over-long frame, which is the one refusal the SDK's reader does
        // not make for us.** A peer that declares more than any payload here is
        // wide is refused whatever type it names, because `FrameRx` reports the
        // excess as exactly `MAX_KEPT_PAYLOAD` — this is the header check the
        // hand-written buffer used to do, kept.
        for kind in [MSG_STREAM_OPEN, MSG_STREAM_SET_VOLUME, MSG_STREAM_CLOSE] {
            for stream_idx in [None, Some(3)] {
                assert_eq!(
                    classify(kind, MAX_KEPT_PAYLOAD, stream_idx),
                    Control::Violation,
                    "an over-long {kind} frame must be refused, not truncated into a message",
                );
            }
        }

        // Nothing else is a message — including the two soundd only ever sends.
        for other in [0, MSG_STREAM_OPENED, MSG_STREAM_ERROR, u32::MAX] {
            for stream_idx in [None, Some(3)] {
                for len in [0, VOL, OPEN] {
                    assert_eq!(classify(other, len, stream_idx), Control::Violation);
                }
            }
        }
    }
}

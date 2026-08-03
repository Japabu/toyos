//! The desktop must survive a client that stops talking, stops listening, or
//! never stops.
//!
//! Every one of these cases used to park the compositor's whole event loop in
//! a kernel wait with no deadline — no redraws, no input, nothing — because
//! the compositor read and wrote its clients with blocking calls. The one
//! written up in `specs/known-issues.md` §1 is the second case here: a client
//! that connects and sends four bytes, met by `ipc::recv_header` on a freshly
//! accepted fd.
//!
//! Each case sets its stall up and leaves it standing, then asks the
//! compositor a question **with a deadline**. That is the shape the assertion
//! has to have: a frozen compositor turns any unbounded call into a hung boot,
//! and a hung boot names no defect. The host side asserts the other half —
//! that the desktop is still *painting*, and that every client dropped along
//! the way was named in the log.

use std::process::exit;
use std::thread;
use std::time::{Duration, Instant};

use toyos::{ipc, services, Connection};
use toyos_abi::syscall::{self, SyscallError};
use window::Window;

/// `MSG_GET_RESOLUTION` is answered from the compositor's dispatch, so a reply
/// proves the event loop reached the end of a pass rather than merely that the
/// process exists.
const PROBE_POLLS: u32 = 500;
const PROBE_POLL_NS: u64 = 10_000_000;

/// Past the compositor's own `HANDSHAKE_TIMEOUT`, so the three connections
/// that never finish a first frame have been ruled on by the time the run
/// ends and the host can require the lines that say so.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(3);

/// A message type no protocol here defines: the compositor's dispatch ignores
/// it, so a stream of them is pure event-loop load with nothing to draw. That
/// is what makes it a starvation case rather than a redraw case.
const UNKNOWN_MSG: u32 = 0x7FFF_0001;

/// Long enough to contain the compositor's 2 s reporting interval whichever
/// side of one it starts on.
const STREAM: Duration = Duration::from_secs(5);

/// One `MSG_GET_RESOLUTION` costs the client 8 bytes and the compositor 16, so
/// filling a client's 2,097,088-byte receive ring from the far side takes
/// 131,068 answers. This is that with margin, and the requests themselves are
/// half the bytes and fit in the client's own ring — nothing here can block
/// the *client* instead, which would prove the wrong thing.
const REQUESTS: usize = 140_000;

/// How long the compositor gets to reach the end of that ring and say so.
///
/// Measured at roughly a second on the metal-sim boot; this is an order of
/// magnitude of slack, and it is a bound on the machine rather than on the
/// answer — the answer is the connection closing.
const REFUSAL_POLLS: u32 = 1_500;

fn main() {
    // Held to the end of the run: a dropped `Connection` closes the fd, and a
    // closed fd is a peer that hung up rather than one that went quiet.
    let mut held: Vec<Connection> = Vec::new();

    held.push(connect("connected and silent"));
    probe("connected and silent");

    let conn = connect("half a header");
    write_raw(&conn, &[0u8; 4], "half a header");
    held.push(conn);
    probe("half a header");

    let conn = connect("header without payload");
    let payload_len = std::mem::size_of::<window::CreateWindowRequest>() as u32;
    write_raw(&conn, &header(window::MSG_CREATE_WINDOW, payload_len), "header without payload");
    held.push(conn);
    probe("header without payload");

    // The three above are handshakes that never complete. Nothing the client
    // does ends them; the compositor's own deadline does.
    thread::sleep(HANDSHAKE_WAIT);
    probe("after the handshake deadline");

    // A window that stops in the middle of a message it already declared. The
    // stall is on an established connection rather than a fresh one, which is
    // the sibling of the accept-path defect and had the same cure.
    let stuck = Window::create(64, 64).expect("a window to stall mid-message with");
    write_raw_fd(stuck.fd(), &header(window::MSG_CLIPBOARD_SET, 116), "window mid-message");
    write_raw_fd(stuck.fd(), &[b'x'; 8], "window mid-message");
    probe("window stopped mid-message");

    // A window that asks faster than it reads. The compositor's answer has to
    // be a refusal, because the alternative is waiting for a client to read
    // its mail.
    let deaf = Window::create(64, 64).expect("a window to stop reading with");
    let mut requests = Vec::with_capacity(REQUESTS * 8);
    for _ in 0..REQUESTS {
        requests.extend_from_slice(&header(window::MSG_GET_RESOLUTION, 0));
    }
    write_raw_fd(deaf.fd(), &requests, "window that will not read");
    await_refusal(&deaf);
    probe("window that will not read");

    // A window with something to send on every pass. Nothing here is
    // unanswerable — the loop simply never runs out of work, and a drain that
    // ends only when nothing is ready never reaches the screen. The assertion
    // is the host's: frames, between these two markers.
    let noisy = Window::create(64, 64).expect("a window to stream from");
    let fd = noisy.fd();
    println!("compositor stall: stream start");
    let streamer = thread::spawn(move || {
        let frame = header(UNKNOWN_MSG, 0);
        let until = Instant::now() + STREAM;
        loop {
            // Fill the ring, not merely feed it. The compositor takes one
            // frame per client per pass, so a client that keeps up with only
            // that lets the drain run dry and the screen get painted — which
            // is the thing this case is supposed to prevent.
            //
            // Never a torn frame: both ends move this ring in multiples of
            // eight bytes and its capacity is one too, so a write of a header
            // either fits whole or finds no room at all.
            while matches!(syscall::write_nonblock(fd, &frame), Ok(8)) {}
            if Instant::now() >= until {
                break;
            }
            syscall::nanosleep(1_000_000);
        }
    });
    streamer.join().expect("the streaming thread");
    println!("compositor stall: stream end");
    probe("window that never stops sending");

    println!("compositor stall: 6 stalls survived, compositor still serving");
}

fn header(msg_type: u32, len: u32) -> [u8; 8] {
    let mut frame = [0u8; 8];
    frame[..4].copy_from_slice(&msg_type.to_ne_bytes());
    frame[4..].copy_from_slice(&len.to_ne_bytes());
    frame
}

fn connect(what: &str) -> Connection {
    services::connect("compositor")
        .unwrap_or_else(|e| fail(&format!("[{what}] the compositor is not serving: {e:?}")))
}

fn write_raw(conn: &Connection, bytes: &[u8], what: &str) {
    write_raw_fd(conn.fd(), bytes, what);
}

/// Every write here fits in the pipe it goes into, so a blocking `write` can
/// only be the compositor's problem, never this binary's.
fn write_raw_fd(fd: toyos_abi::Fd, bytes: &[u8], what: &str) {
    let mut offset = 0;
    while offset < bytes.len() {
        match syscall::write(fd, &bytes[offset..]) {
            Ok(n) => offset += n,
            Err(e) => fail(&format!("[{what}] write failed after {offset} bytes: {e:?}")),
        }
    }
}

/// Wait for the compositor to hang up on the window that stopped reading.
///
/// **Without draining a byte**, which is the whole difficulty: this client's
/// receive ring has to stay full for the compositor to reach the end of it,
/// so the answer cannot be read from the ring. An empty `write_nonblock`
/// writes nothing and still asks the one question that matters — is anything
/// still holding the read end — so the refusal is observed rather than slept
/// through. A compositor parked in `write` instead has its fd open and
/// answers `Ok` here forever.
fn await_refusal(deaf: &Window) {
    for _ in 0..REFUSAL_POLLS {
        if let Err(SyscallError::NotFound) = syscall::write_nonblock(deaf.fd(), &[]) {
            return;
        }
        syscall::nanosleep(PROBE_POLL_NS);
    }
    fail(&format!(
        "[window that will not read] {} bytes of unread answers and the compositor never \
         dropped the connection — it is waiting for this client to read its mail",
        REQUESTS * 16,
    ));
}

/// Ask the compositor something it always answers, and give it a deadline.
fn probe(what: &str) {
    let conn = connect(what);
    if let Err(e) = ipc::signal(conn.fd(), window::MSG_GET_RESOLUTION) {
        fail(&format!("[{what}] could not ask the compositor for its resolution: {e:?}"));
    }
    let mut buf = [0u8; 16];
    let mut got = 0;
    for _ in 0..PROBE_POLLS {
        match conn.read_nonblock(&mut buf[got..]) {
            Ok(0) => fail(&format!("[{what}] the compositor closed the probe unanswered")),
            Ok(n) => {
                got += n;
                if got == buf.len() {
                    return;
                }
            }
            Err(SyscallError::WouldBlock) => syscall::nanosleep(PROBE_POLL_NS),
            Err(e) => fail(&format!("[{what}] the probe could not be read: {e:?}")),
        }
    }
    fail(&format!(
        "[{what}] the compositor did not answer in {} ms — its event loop is parked on a client",
        PROBE_POLLS as u64 * PROBE_POLL_NS / 1_000_000,
    ));
}

fn fail(msg: &str) -> ! {
    eprintln!("compositor stall: {msg}");
    exit(1);
}

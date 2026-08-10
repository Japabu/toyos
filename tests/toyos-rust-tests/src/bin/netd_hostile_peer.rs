//! The network stack must survive a client that stops talking.
//!
//! netd used to `accept` and then call `ipc::recv_header` on the fresh fd, and
//! `read_exact` behind it is a *blocking* read — so one client that connected
//! and wrote four bytes stopped the network stack for everyone until it
//! disconnected. Its dispatch read every payload off the fd too, so a whole
//! header followed by silence did the same on a connection that had already
//! said what it wanted. This is the compositor's closed defect, line for line,
//! in the last daemon that still had it.
//!
//! Needs netd with a NIC in front of it, which only `tests/netcase` provides —
//! it is in `RUST_SKIP` and `netd_hostile_peer` runs it there.
//!
//! **Every wait here is bounded and every failure is named.** The obvious way
//! to ask netd a question is `toyos::net::dns_lookup`, and against a netd that
//! has stopped serving it blocks in `recv_header` forever — which is the exact
//! failure under test, so it cannot also be how the test waits.

use std::process::exit;
use std::time::{Duration, Instant};

use toyos::endow;
use toyos::ipc::{self, RxStep};
use toyos::net::{MsgType, RespType};
use toyos::Connection;
use toyos_abi::syscall;

/// How long netd may take to answer a question it answers from the request
/// itself. Four orders of magnitude over the real thing, and it still fails in
/// two seconds rather than hanging the boot.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(2);
/// How long netd may take to rule on a frame it cannot act on.
const RULING_TIMEOUT: Duration = Duration::from_secs(2);
/// netd's own `HANDSHAKE_TIMEOUT` is 2 s. This is the ceiling on observing it,
/// with enough slack that a busy host is not a failure.
const HANDSHAKE_CEILING: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(10);

/// A literal address, which netd parses out of the request and answers without
/// a packet — so this asks whether the daemon is *serving*, on a machine whose
/// NIC has nobody on the other end of it.
const LITERAL: &[u8] = b"192.0.2.7";
/// What netd sends back for [`LITERAL`]: one address, four bytes, the octets.
const LITERAL_REPLY: [u8; 6] = [1, 4, 192, 0, 2, 7];

/// Hostile connections opened in the run that fills netd's pending table. Past
/// its cap netd refuses by name, so this has to be comfortably over it.
const SILENT_BURST: usize = 48;
/// One netd pass per connection, so the table fills instead of the *kernel's*
/// 32-deep unaccepted queue refusing the connects before netd ever sees them.
const BURST_PACE: Duration = Duration::from_millis(1);
/// Long enough for netd to take one pass over a poller full of ready fds. Two
/// orders of magnitude over the pass itself, and far inside netd's own 2 s
/// handshake deadline, which is what would make the survivors miscount.
const SETTLE: Duration = Duration::from_millis(100);

/// A frame netd cannot act on, and what it must do about it.
struct Case {
    name: &'static str,
    /// Bytes written on a fresh connection. A prefix of a frame on purpose in
    /// the first three: that is what a blocking read parks on.
    bytes: Vec<u8>,
    /// Whether netd must have ruled on this connection — answered it or closed
    /// it — by the time it is asked. A partial frame is *not* a ruling: netd is
    /// entitled to hold it until its handshake deadline, and that it does so
    /// without stopping is the whole point.
    ruled: bool,
}

fn header(msg_type: u32, len: u32) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8);
    frame.extend_from_slice(&msg_type.to_ne_bytes());
    frame.extend_from_slice(&len.to_ne_bytes());
    frame
}

/// `TcpBindPipedRequest` is 16 bytes, so a frame declaring fewer is a payload
/// netd asked for and did not get.
fn cases() -> Vec<Case> {
    let bind = MsgType::TcpBindPiped as u32;
    vec![
        // The three that used to park netd. First, so a red run names the
        // stall rather than timing out on a later case behind it.
        Case { name: "connected and silent", bytes: Vec::new(), ruled: false },
        Case { name: "half a header", bytes: vec![0u8; 4], ruled: false },
        Case { name: "header, then silence", bytes: header(bind, 16), ruled: false },
        // Whole frames netd can locate and must rule on.
        Case { name: "short payload", bytes: header(bind, 0), ruled: true },
        Case { name: "oversized header", bytes: header(bind, u32::MAX), ruled: true },
        Case { name: "garbage frame", bytes: header(0xDEAD_BEEF, 0x7FFF_FFFF), ruled: true },
    ]
}

fn main() {
    let cases = cases();
    for case in &cases {
        let conn = endow::service("netd")
            .unwrap_or_else(|e| panic!("[{}] netd is not serving: {e:?}", case.name));
        if !case.bytes.is_empty() {
            let written = syscall::write(conn.fd(), &case.bytes)
                .unwrap_or_else(|e| panic!("[{}] could not write the frame: {e:?}", case.name));
            assert_eq!(written, case.bytes.len(), "[{}] partial frame write", case.name);
        }

        // Asked while the hostile connection is still open, which is the whole
        // question: a netd parked on it answers nobody.
        if let Err(e) = still_serving() {
            eprintln!("[{}] {e}", case.name);
            exit(1);
        }

        if case.ruled && !ruled_on(&conn, RULING_TIMEOUT) {
            eprintln!(
                "[{}] netd neither answered the frame nor closed the connection",
                case.name
            );
            exit(1);
        }
        drop(conn);
    }

    // A connection that never says anything must not be netd's to hold forever.
    let silent = endow::service("netd").expect("netd is not serving");
    let opened = Instant::now();
    if !closed_by_peer(&silent, HANDSHAKE_CEILING) {
        eprintln!("netd held a silent connection for {:?} without dropping it", opened.elapsed());
        exit(1);
    }
    let handshake = opened.elapsed();
    drop(silent);

    // And a burst of them must be bounded rather than accumulated. Liveness is
    // *not* asserted while the table is full: refusing a new connection there
    // is the bound doing its job, and a test that called that a stall would be
    // asserting the opposite of the design.
    let mut held = Vec::new();
    for _ in 0..SILENT_BURST {
        if let Ok(conn) = endow::service("netd") {
            held.push(conn);
        }
        syscall::nanosleep(BURST_PACE.as_nanos() as u64);
    }
    syscall::nanosleep(SETTLE.as_nanos() as u64);
    let opened = held.len();
    // netd drops what it will not hold, so the survivors are the ones it took.
    held.retain(|c| !closed_by_peer(c, Duration::ZERO));
    let kept = held.len();
    // Both sides of the boundary. "It bounded them" is also true of a netd that
    // refused every one, which would be a broken daemon reported as a working
    // bound — the same non-vacuity check `netd_caps` makes.
    assert!(kept < opened, "netd held all {opened} unidentified connections — nothing bounded them");
    assert!(kept >= 2, "netd held only {kept} unidentified connections; it is refusing, not bounding");

    // Released, netd must come back — a bound that is really a wedge would
    // stay refusing after the thing it was bounding is gone.
    drop(held);
    syscall::nanosleep(SETTLE.as_nanos() as u64);
    if let Err(e) = still_serving() {
        eprintln!("[after the burst] {e}");
        exit(1);
    }

    println!(
        "netd hostile peer: {} malformed frames refused, {kept} of {opened} unidentified \
         connections held, silent one dropped after {} ms, netd alive",
        cases.len(),
        handshake.as_millis(),
    );
}

/// Ask netd something it answers from the request itself, without blocking.
///
/// [`ipc::FrameRx`] is the SDK's non-blocking framing — the same type netd now
/// reads its clients with — so the deadline is this test's and not the peer's.
fn still_serving() -> Result<(), String> {
    let conn = endow::service("netd").map_err(|e| format!("netd refused a connection: {e:?}"))?;
    conn.try_send_bytes(MsgType::DnsLookup as u32, LITERAL)
        .map_err(|e| format!("netd would not take a request: {e:?}"))?;

    let mut rx = ipc::FrameRx::<16>::new();
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    while Instant::now() < deadline {
        match rx.pump(&conn) {
            RxStep::Idle => syscall::nanosleep(POLL.as_nanos() as u64),
            RxStep::Eof => return Err("netd closed a request without answering it".to_string()),
            RxStep::Malformed => return Err("netd sent a frame the SDK cannot read".to_string()),
            RxStep::Frame { msg_type, payload_len } => {
                if msg_type != RespType::Result as u32 {
                    return Err(format!("netd answered with message type {msg_type}"));
                }
                let payload = rx.payload(payload_len);
                if payload != LITERAL_REPLY.as_slice() {
                    return Err(format!("netd answered a literal address with {payload:?}"));
                }
                return Ok(());
            }
        }
    }
    Err(format!(
        "netd did not answer a request in {ANSWER_TIMEOUT:?} — it is parked on a client"
    ))
}

/// Did netd either answer this connection or close it?
///
/// Both are rulings. An answer is the better one — a client learns that its
/// frame was refused — and a close is what a frame with no locatable next
/// message boundary gets.
fn ruled_on(conn: &Connection, within: Duration) -> bool {
    let mut buf = [0u8; 8];
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        match conn.read_nonblock(&mut buf) {
            Ok(_) => return true,
            Err(syscall::SyscallError::WouldBlock) => syscall::nanosleep(POLL.as_nanos() as u64),
            // The fd itself is gone, which is the same hang-up seen from the
            // other end of the same race.
            Err(_) => return true,
        }
    }
    false
}

/// Did netd hang up? `read_nonblock` returning 0 is EOF; `WouldBlock` is "not
/// yet"; anything else it sent means the connection is alive, which is not what
/// was asked.
fn closed_by_peer(conn: &Connection, within: Duration) -> bool {
    let mut buf = [0u8; 8];
    let deadline = Instant::now() + within;
    loop {
        match conn.read_nonblock(&mut buf) {
            Ok(0) => return true,
            Ok(_) => return false,
            Err(syscall::SyscallError::WouldBlock) => {}
            Err(_) => return true,
        }
        if Instant::now() >= deadline {
            return false;
        }
        syscall::nanosleep(POLL.as_nanos() as u64);
    }
}

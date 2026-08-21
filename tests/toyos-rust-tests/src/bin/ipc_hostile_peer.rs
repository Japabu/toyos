//! A daemon must survive a peer that lies about its frames.
//!
//! `ipc::recv_payload` used to `assert!(header.len >= size_of::<T>())` on a
//! number the peer chose, and `header.len` had no upper bound at all, so one
//! 8-byte message from any client was either a compositor panic or a
//! compositor parked in `read_exact` waiting for bytes nobody would send.
//! Nothing in the SDK bounded a frame: `MAX_` matched exactly one constant
//! across both SDK crates.
//!
//! Each case opens its own connection, writes a header the compositor cannot
//! act on, and then requires two things:
//!
//! - the compositor **closed that connection** — proof it read the frame and
//!   ruled on it, rather than the frame never arriving. Without this the test
//!   passes on a compositor that ignores its listener entirely.
//! - the compositor **still serves a real window** afterwards, from a fresh
//!   connection.
//!
//! The order matters: the short-header case is first because it is the one
//! that used to panic outright, so a red run names the defect rather than
//! timing out on the parked case behind it.

use std::process::exit;

use toyos::endow;
use toyos::AsHandle;
use toyos::Connection;
use toyos_abi::syscall;
use window::Window;

/// `CreateWindowRequest` is 40 bytes, so every length below it is a payload
/// the compositor asked for and did not get.
const CASES: &[(&str, u32, u32)] = &[
    // A payload shorter than the type the message type names.
    ("short header", window::MSG_CREATE_WINDOW, 0),
    // A length no frame can have. The old code walked it 128 bytes at a time.
    ("oversized header", window::MSG_CREATE_WINDOW, u32::MAX),
    // Neither field means anything: an unknown type with a hostile length.
    ("garbage frame", 0xDEAD_BEEF, 0x7FFF_FFFF),
];

/// The compositor's answer is a close, which arrives on its own schedule.
/// 100 x 10 ms is two orders of magnitude over a loop iteration and still
/// fails in a second rather than hanging the boot.
const EOF_POLLS: u32 = 100;
const EOF_POLL_NS: u64 = 10_000_000;

fn main() {
    for (name, msg_type, len) in CASES {
        let conn = endow::service("compositor")
            .unwrap_or_else(|e| panic!("[{name}] the compositor is not serving: {e:?}"));

        let mut frame = [0u8; 8];
        frame[..4].copy_from_slice(&msg_type.to_ne_bytes());
        frame[4..].copy_from_slice(&len.to_ne_bytes());
        let written = syscall::write(conn.as_handle(), &frame)
            .unwrap_or_else(|e| panic!("[{name}] could not write the frame: {e:?}"));
        assert_eq!(written, frame.len(), "[{name}] partial frame write");

        if !closed_by_peer(&conn) {
            eprintln!("[{name}] the compositor neither closed the connection nor refused it");
            exit(1);
        }

        // A window from a fresh connection: the compositor is not merely
        // alive as a process, it is still serving the protocol.
        let w = Window::create(64, 64)
            .unwrap_or_else(|e| panic!("[{name}] the compositor stopped serving windows: {e}"));
        drop(w);
    }

    println!("ipc hostile peer: {} malformed frames refused, compositor alive", CASES.len());
}

/// Did the peer hang up? `read_nonblock` returning 0 is EOF; `WouldBlock` is
/// "not yet". A blocking read would turn a compositor that panicked into a
/// hung boot instead of a named failure.
fn closed_by_peer(conn: &Connection) -> bool {
    let mut buf = [0u8; 8];
    for _ in 0..EOF_POLLS {
        match conn.read_nonblock(&mut buf) {
            Ok(0) => return true,
            // Anything the compositor sends back is still an answer, and it
            // means the connection is alive — which is not what was asked.
            Ok(_) => return false,
            Err(syscall::SyscallError::WouldBlock) => syscall::nanosleep(EOF_POLL_NS),
            // The connection itself is gone, which is the same hang-up seen from the
            // other end of the same race.
            Err(_) => return true,
        }
    }
    false
}

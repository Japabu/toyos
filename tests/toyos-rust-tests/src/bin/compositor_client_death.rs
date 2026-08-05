//! The desktop must survive a client that dies, and one that asks for
//! something the kernel will refuse on its behalf.
//!
//! The owner's machine lost its whole desktop to this: doom aborted, and three
//! seconds later the compositor granted a resized window's buffer to it —
//! `grant_shared` answers `InvalidArgument` for a pid the process table no
//! longer has, `SharedMemory::grant` was infallible over that, and every other
//! window went with it. `exit: compositor code=101`.
//!
//! Four cases, and the first is that one. The rest are the same shape found by
//! reading for it: places where a message from any client reached a syscall
//! whose refusal the compositor was not prepared to hear.
//!
//! Each case leaves its damage standing and then asks the compositor a
//! question **with a deadline**, exactly as `compositor_stall` does — the host
//! asserts the other half, that the desktop is still painting and that every
//! client dropped on the way was named with its pid.

use std::io::Write;
use std::process::{exit, Command, Stdio};

use toyos::{ipc, services, Connection};
use toyos_abi::syscall::{self, SyscallError};
use window::Window;

const SELF_PATH: &str = "/bin/test_rs_compositor_client_death";

/// How many clients ask for a window and are gone before they can be given
/// one.
///
/// One is a race — the compositor may serve a connection before its creator
/// has finished exiting — and this is not: the compositor promotes at most one
/// pending connection per pass and composites between passes, so the last of
/// these is dispatched several frames after every one of them was reaped.
const VANISHERS: usize = 8;

/// `MSG_GET_RESOLUTION` is answered from the compositor's dispatch, so a reply
/// proves the event loop reached the end of a pass rather than merely that the
/// process still exists.
const PROBE_POLLS: u32 = 500;
const PROBE_POLL_NS: u64 = 10_000_000;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("vanish") => vanish(),
        Some(other) => panic!("unknown role {other:?}"),
        None => run(),
    }
}

fn run() {
    // A creator that is gone before its window is made. `accept` names the
    // process that connected and the frame it left in the pipe outlives it, so
    // what the compositor grants to here is a pid the kernel has forgotten.
    let mut kids = Vec::new();
    for _ in 0..VANISHERS {
        kids.push(
            Command::new(SELF_PATH)
                .arg("vanish")
                .stdin(Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| fail(&format!("[vanishing creators] spawn failed: {e}"))),
        );
    }
    // Released together, so the compositor meets them as a queue rather than
    // one at a time.
    for kid in &mut kids {
        let mut go = kid.stdin.take().expect("the child's stdin");
        writeln!(go, "go").expect("release the child");
    }
    for mut kid in kids {
        kid.wait().expect("wait for the child");
    }
    probe("creators that vanished");

    // A window is a connection promoted by its first frame, so a second
    // `MSG_CREATE_WINDOW` on one arrives with nothing to promote. The
    // compositor read that as its own bug.
    let doubled = Window::create(64, 64).expect("a window to send a second create on");
    write_raw_fd(doubled.fd(), &create_frame(), "a second create");
    probe("a second create on a live window");

    // A region this process owns and has granted to nobody: the compositor is
    // refused when it maps the token, which is the only answer the kernel can
    // give and one message from any client.
    let token = syscall::alloc_shared(4096).expect("a region of our own");
    clipboard_shm(token, 64, "an ungranted clipboard token");
    probe("an ungranted clipboard token");

    // The same token with a length no region can satisfy. The length decides
    // how much of somebody else's memory gets read as clipboard text, so it is
    // the compositor's to bound rather than the client's to choose.
    clipboard_shm(token, u32::MAX, "a clipboard longer than any region");
    probe("a clipboard longer than any region");

    println!("compositor client death: 4 deaths survived, compositor still serving");
}

/// The child: connect, ask for a window, and be gone before the answer.
fn vanish() {
    let conn = services::connect("compositor").expect("the compositor is not serving");
    // Held back until the parent has every one of these standing, and written
    // in one call so the compositor never sees half a frame — a partial frame
    // is `compositor_stall`'s case, not this one.
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).expect("wait for the parent");
    write_raw_fd(conn.fd(), &create_frame(), "vanish");
}

/// A whole `MSG_CREATE_WINDOW` for a 64x64 window, header and payload.
fn create_frame() -> Vec<u8> {
    let payload_len = core::mem::size_of::<window::CreateWindowRequest>();
    let mut frame = vec![0u8; 8 + payload_len];
    frame[..4].copy_from_slice(&window::MSG_CREATE_WINDOW.to_ne_bytes());
    frame[4..8].copy_from_slice(&(payload_len as u32).to_ne_bytes());
    frame[8..12].copy_from_slice(&64u32.to_ne_bytes());
    frame[12..16].copy_from_slice(&64u32.to_ne_bytes());
    frame
}

fn clipboard_shm(token: u32, len: u32, what: &str) {
    let conn = services::connect("compositor")
        .unwrap_or_else(|e| fail(&format!("[{what}] the compositor is not serving: {e:?}")));
    conn.send(window::MSG_CLIPBOARD_SET_SHM, &window::ClipboardShmMsg { token, len })
        .unwrap_or_else(|e| fail(&format!("[{what}] could not send: {e:?}")));
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

/// Ask the compositor something it always answers, and give it a deadline.
fn probe(what: &str) {
    let conn: Connection = services::connect("compositor")
        .unwrap_or_else(|e| fail(&format!("[{what}] the compositor is not serving: {e:?}")));
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
        "[{what}] the compositor did not answer in {} ms — it is gone or its loop is parked",
        PROBE_POLLS as u64 * PROBE_POLL_NS / 1_000_000,
    ));
}

fn fail(msg: &str) -> ! {
    eprintln!("compositor client death: {msg}");
    exit(1);
}

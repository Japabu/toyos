//! The desktop must survive a client that dies, and one that asks for
//! something the kernel will refuse on its behalf.
//!
//! The owner's machine lost its whole desktop to this: doom aborted, and three
//! seconds later the compositor granted a resized window's buffer to it —
//! `grant_shared` answered `InvalidArgument` for a pid the process table no
//! longer had, `SharedMemory::grant` was infallible over that, and every other
//! window went with it. `exit: compositor code=101`. There is no grant left to
//! be infallible over: a buffer travels as a handle and a client that has gone
//! is a refused send.
//!
//! Five cases. The first is that one; the next three are the same shape found
//! by reading for it — places where a message from any client reached a
//! syscall whose refusal the compositor was not prepared to hear.
//!
//! The fifth is the other side of the same event, and it is the client's:
//! **a window whose connection has gone must let its owner leave.** Nothing
//! else here is about the client's own fate, and that is why it belongs beside
//! them rather than in a test of its own — a window ending has two halves, and
//! each one used to take a process with it.
//!
//! Each case leaves its damage standing and then asks the compositor a
//! question **with a deadline**, exactly as `compositor_stall` does — the host
//! asserts the other half, that the desktop is still painting and that every
//! client dropped on the way was named with its pid.

use std::io::{BufRead, BufReader};
use std::os::toyos::process::CommandExt;
use std::process::{exit, Command, Stdio};

use toyos::endow;
use toyos::AsHandle;
use toyos::shm::SharedMemory;
use toyos::{ipc, Connection};
use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::RawHandle;
use window::Window;

const SELF_PATH: &str = "/bin/test_rs_compositor_client_death";

/// The compositor connection, in the process that finishes the request its
/// creator did not live to send.
const RELAY_SOCKET: RawHandle = RawHandle(3);
/// The other end of the root's pipe, which closes when the creator has been
/// reaped. Nothing is ever read off it but the hang-up.
const RELAY_GO: RawHandle = RawHandle(4);

/// `MSG_GET_RESOLUTION` is answered from the compositor's dispatch, so a reply
/// proves the event loop reached the end of a pass rather than merely that the
/// process still exists.
const PROBE_POLLS: u32 = 500;
const PROBE_POLL_NS: u64 = 10_000_000;

/// How many events a closed window is asked for.
///
/// Two would do — `Close`, then `None` — and this is a handful more so the
/// failure prints a stream rather than a single wrong answer. Each poll past
/// the close costs nothing: the handle is ready, so none of them waits.
const POLLS_AFTER_CLOSE: usize = 8;
/// Long enough that a compositor still on its way to closing the connection is
/// waited for rather than raced.
const POLL_TIMEOUT_NS: u64 = 2_000_000_000;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("connect") => connect_and_go(),
        Some("finish") => finish(),
        Some(other) => panic!("unknown role {other:?}"),
        None => run(),
    }
}

fn run() {
    // **A creator that is gone before its window is asked for, with no race in
    // it.** `accept` names the process that called `connect`, and a connection
    // outlives that process — so the pid the compositor grants to here is one
    // the kernel has already forgotten.
    //
    // Racing a dying creator against the compositor's own dispatch is what
    // this used to do, and under a loaded host the compositor won all eight
    // heats and the run proved nothing. Instead the request is *completed by a
    // third process*: the creator hands its socket to a grandchild and exits,
    // this process reaps it — which is what takes the pid out of the process
    // table — and only then closes the pipe that releases the grandchild to
    // send the frame. Every step waits on the one before it.
    let mut creator = Command::new(SELF_PATH)
        .arg("connect")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| fail(&format!("[a reaped creator] spawn failed: {e}")));
    let go = creator.stdin.take().expect("the creator's stdin");
    let mut said = String::new();
    BufReader::new(creator.stdout.take().expect("the creator's stdout"))
        .read_line(&mut said)
        .unwrap_or_else(|e| fail(&format!("[a reaped creator] it never connected: {e}")));
    if !said.starts_with("connected") {
        fail(&format!("[a reaped creator] the creator said {said:?}"));
    }
    creator.wait().expect("reap the creator");
    // The reap is what makes the pid unknown; this is what tells the grandchild
    // the reap has happened.
    drop(go);
    probe("a creator reaped before its window");

    // A window is a connection promoted by its first frame, so a second
    // `MSG_CREATE_WINDOW` on one arrives with nothing to promote. The
    // compositor read that as its own bug.
    let doubled = Window::create(64, 64).expect("a window to send a second create on");
    write_handle(doubled.handle(), &create_frame(), "a second create");
    probe("a second create on a live window");

    // A clipboard frame with no region sent ahead of it. The receive is not a
    // poll — a short batch is a peer that sent its frame first — so this must
    // cost the client its connection and nothing else.
    clipboard_shm(None, 64, "a clipboard frame with no region");
    probe("a clipboard frame with no region");

    // A region really sent, with a length no region can satisfy. The length
    // decides how much of the region is read as clipboard text, so it is the
    // compositor's to bound rather than the client's to choose.
    let region = SharedMemory::create(4096).expect("a region of our own");
    let shared = region.share().expect("a second handle to it");
    clipboard_shm(Some(shared), u32::MAX, "a clipboard longer than any region");
    probe("a clipboard longer than any region");

    // The other side of a window ending: the client has to be able to leave.
    // `MSG_DESTROY_WINDOW` makes the compositor drop the connection, after
    // which the handle is permanently read-ready at EOF — so a `poll_event` that
    // did not latch answered `Close` for as long as anybody kept asking, and a
    // client draining until `None` never got out. Two calls decide it.
    let mut ending = Window::create(64, 64).expect("a window to close from the inside");
    ipc::signal(ending.handle(), window::MSG_DESTROY_WINDOW)
        .expect("ask the compositor to destroy this window");
    // Named rather than kept, because `Event` is not `Debug` and a failure
    // here has to print the whole sequence it saw.
    let mut seen: Vec<&'static str> = Vec::new();
    for _ in 0..POLLS_AFTER_CLOSE {
        let name = match ending.poll_event(POLL_TIMEOUT_NS) {
            None => "none",
            Some(window::Event::Close) => "close",
            // A frame the compositor had already sent can arrive first. It is
            // not what this case is about, and skipping it is not a weakening:
            // what follows still has to be close and then nothing.
            Some(_) => "other",
        };
        seen.push(name);
        if name == "none" {
            break;
        }
    }
    let sequence = seen.join(",");
    let Some(closed_at) = seen.iter().position(|n| *n == "close") else {
        fail(&format!(
            "[a window closed from the inside] the connection went and the window never said \
             so: {sequence}"
        ));
    };
    if seen.get(closed_at + 1) != Some(&"none") {
        fail(&format!(
            "[a window closed from the inside] the poll after Close answered again: {sequence} \
             — a client that drains until None cannot leave"
        ));
    }
    probe("a window closed from the inside");

    println!("compositor client death: 5 deaths survived, compositor still serving");
}

/// The creator: connect, hand the connection to a process that will outlive
/// this one, and go.
///
/// Nothing is sent here. The compositor's record of who this connection
/// belongs to is made at `connect`, and that is the only thing this role has
/// to establish before dying.
fn connect_and_go() {
    let conn = endow::service("compositor").expect("the compositor is not serving");
    // The kernel clones the handle into the child's table
    // (`loader::build_child_handles`), so the socket — and the pipes under it —
    // outlive this process.
    Command::new(SELF_PATH)
        .arg("finish")
        .inherit_handle(RELAY_SOCKET.0, conn.as_handle().0)
        .inherit_handle(RELAY_GO.0, 0)
        .spawn()
        .expect("spawn the process that finishes the request");
    println!("connected");
}

/// The grandchild: send the request its creator never sent, once that creator
/// has been reaped.
fn finish() {
    let mut byte = [0u8; 1];
    // The hang-up is the signal and the only signal: the root closes its end
    // after `wait` returns, and `wait` returning is the pid leaving the
    // process table.
    while let Ok(1) = syscall::read(RELAY_GO, &mut byte) {}
    write_handle(RELAY_SOCKET, &create_frame(), "finish");

    // **The answer is the non-vacuity witness, and it changed sides.** The
    // compositor used to say "the process behind it has exited" here, because
    // it granted the buffer to the pid `accept` reported and the kernel had
    // forgotten that pid. There is no pid and no grant: the buffer is a handle
    // sent over this connection, which is alive because this process holds it.
    // So the request is *served*, and the line that proves the compositor met
    // it is the answer rather than a refusal.
    let header = ipc::recv_header(RELAY_SOCKET).expect("the compositor answered");
    let what = if header.msg_type == window::MSG_WINDOW_CREATED { "a window" } else { "nothing" };
    // Stderr, because stdout is the pipe the root read one line off and let go
    // of: this process outlives the reader of its own stdout, and stderr is the
    // console both it and the compositor already share.
    eprintln!("a reaped creator's connection still got {what}");
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

fn clipboard_shm(region: Option<toyos_abi::RawHandle>, len: u32, what: &str) {
    let conn = endow::service("compositor")
        .unwrap_or_else(|e| fail(&format!("[{what}] the compositor is not serving: {e:?}")));
    let msg = window::ClipboardShmMsg { len };
    let sent = match region {
        Some(h) => conn.send_with_handles(&[h], window::MSG_CLIPBOARD_SET_SHM, &msg),
        None => conn.send(window::MSG_CLIPBOARD_SET_SHM, &msg),
    };
    sent.unwrap_or_else(|e| fail(&format!("[{what}] could not send: {e:?}")));
}

/// Every write here fits in the pipe it goes into, so a blocking `write` can
/// only be the compositor's problem, never this binary's.
fn write_handle(handle: toyos_abi::RawHandle, bytes: &[u8], what: &str) {
    let mut offset = 0;
    while offset < bytes.len() {
        match syscall::write(handle, &bytes[offset..]) {
            Ok(n) => offset += n,
            Err(e) => fail(&format!("[{what}] write failed after {offset} bytes: {e:?}")),
        }
    }
}

/// Ask the compositor something it always answers, and give it a deadline.
fn probe(what: &str) {
    let conn: Connection = endow::service("compositor")
        .unwrap_or_else(|e| fail(&format!("[{what}] the compositor is not serving: {e:?}")));
    if let Err(e) = ipc::signal(conn.as_handle(), window::MSG_GET_RESOLUTION) {
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

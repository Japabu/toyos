//! A thread killed while blocked on a handle still gives the handle back.
//!
//! **This is the handle property that matters most for this architecture, and
//! nothing in the tree executed it.**
//!
//! `handle_count` is deliberately not the `Arc` count (`kernel/src/object/`).
//! The reason is exactly this shape: a blocking syscall clones an `Arc` out of
//! the table before it parks, this kernel does not unwind, and a thread another
//! CPU kills never runs a destructor — so that `Arc` is stranded on a kernel
//! stack that is simply freed. If EOF and dead-peer detection rode `Arc`
//! counts, killing a client blocked in its signal-pipe read would leak the read
//! end and the server would never learn. That is the steady state of every cpal
//! client, so "never learn" means soundd writing into a ring nobody reads for
//! the rest of the boot.
//!
//! Three arms, each a child killed at a point it cannot come back from:
//!
//! 1. **Blocked reading a pipe.** The parent holds the only write end and must
//!    see the read side gone.
//! 2. **Blocked reading an IPC connection**, which is the soundd shape one
//!    layer up: the parent's write must answer `Gone`.
//! 3. **Blocked in `accept`.** The child holds the port's only acceptor; the
//!    parent's next connect through the connector must be `Gone`.
//!
//! **No census assertion anywhere in this file, and that is the point rather
//! than an omission.** A live-object count is the one instrument that cannot
//! judge these three: a thread parked in `accept` has cloned the `Arc` out of
//! its table onto its own kernel stack, so killing it strands that `Arc` on
//! memory that is freed without unwinding and the object stays *alive* — which
//! this design accepts and says so (`kernel/src/object/`, §1.1). What must not
//! survive is the **handle count**, because that is what every peer-visible
//! event rides. So each arm asks a peer, and none of them counts objects.
//!
//! **The marker is what gives every arm teeth.** Without it a child killed
//! before it reached its `read` would pass while asserting nothing — the handle
//! would have been released by an ordinary table drain with no `Arc` stranded
//! anywhere, which is the case that is *not* under test.

use std::io::{Read, Write};
use std::os::toyos::process::CommandExt;
use std::process::{Command, Stdio};

use toyos::{endow, namespace, port, AsHandle};
use toyos_abi::syscall::{self, SyscallError, SERVE_PREFIX, SVC_LABEL};

const SELF_PATH: &str = "/bin/test_rs_kill_while_blocked";
const SERVICE: &str = "blocked";

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some(role) => child(role),
        None => test(),
    }
}

fn test() {
    a_pipe_reader_killed_in_the_read();
    a_connection_peer_killed_in_the_read();
    an_acceptor_killed_in_the_accept();
    println!("a killed thread's blocking handle is released, and the peer is told");
}

/// Spawn a child in `role` and wait for it to say it has parked.
///
/// The child's stdin is a pipe this process holds the write end of, which is
/// what arms 1 and 2 measure afterwards.
fn parked(role: &str, extra: Option<(String, u32)>) -> std::process::Child {
    let mut command = Command::new(SELF_PATH);
    command.arg(role).stdin(Stdio::piped()).stdout(Stdio::piped());
    if let Some((label, handle)) = extra {
        command.endow(&label, handle);
    }
    let mut child = command.spawn().unwrap_or_else(|e| panic!("spawn {role}: {e}"));

    let mut byte = [0u8; 1];
    let mut line = Vec::new();
    let out = child.stdout.as_mut().expect("child stdout");
    while out.read(&mut byte).expect("read the child's marker") == 1 {
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    assert_eq!(
        String::from_utf8_lossy(&line),
        format!("parked in {role}"),
        "{role} never reached its blocking call, so nothing was killed while blocked",
    );
    child
}

/// Arm 1. The child is inside `read` on its stdin when it is killed.
///
/// A pipe with no reader refuses a write; a pipe whose only reader is a
/// stranded `Arc` on a freed kernel stack takes one and nobody ever reads it.
/// The write is what tells the two apart.
fn a_pipe_reader_killed_in_the_read() {
    let mut child = parked("pipe-read", None);
    // **Taken before `wait`, which closes it.** `Child::wait` drops the write
    // end first — a child blocked reading a pipe its parent still holds would
    // never exit otherwise — so the handle under test has to leave the `Child`
    // before then.
    let mut stdin = child.stdin.take().expect("the child's stdin");
    child.kill().expect("kill the parked child");
    let _ = child.wait();

    let refused = stdin.write_all(b"nobody is reading this");
    assert!(
        refused.is_err(),
        "a pipe whose only reader was killed mid-read still took a write",
    );
    println!("  pipe: the write end learned its reader had gone");
}

/// Arm 2. The soundd shape: the child is blocked reading a connection.
fn a_connection_peer_killed_in_the_read() {
    let (acceptor, connector) = port::create().expect("a port of our own");
    let ns = namespace::build()
        .add(SERVICE, &connector)
        .finish()
        .expect("a namespace carrying one connector");
    let mut child = parked("connection-read", Some((SVC_LABEL.to_string(), ns.into_raw().0)));
    let conn = acceptor.accept().expect("the child connected");

    child.kill().expect("kill the parked child");
    let _ = child.wait();

    // **`NotFound` and not `Gone`**, which
    // `issues/isolation/a-broken-pipe-answers-not-found.md` is about;
    // `connect_before_serve` asserts the same word for the same reason. What
    // this arm is for is the *release* — on a kernel where the killed thread's
    // stranded `Arc` kept the read end alive, this write succeeds — and either
    // word says the peer went.
    assert_eq!(
        conn.write_nonblock(b"nobody is reading this"),
        Err(SyscallError::NotFound),
        "a connection whose peer was killed mid-read still took a write",
    );
    println!("  connection: the write answered Gone");
}

/// Arm 3. Blocked in `accept`, which is the wait `Acceptor` added.
///
/// **The peer here is a client of the port**, and it is the only thing that can
/// answer. The parked thread holds the acceptor's `Arc` on its own kernel
/// stack, so the object survives the kill; what must not survive is the handle
/// count, because `Acceptor::on_zero_handles` is what sets the port `closed`
/// and a connect through the connector is what reads it.
fn an_acceptor_killed_in_the_accept() {
    let (acceptor, connector) = port::create().expect("a port of our own");
    let label = format!("{SERVE_PREFIX}{SERVICE}");
    let mut child = parked("accept", Some((label, acceptor.into_raw().0)));

    // The parent's own way to ask, over the connector it kept.
    let ns = namespace::build()
        .add(SERVICE, &connector)
        .finish()
        .expect("a namespace carrying one connector");
    child.kill().expect("kill the parked child");
    let _ = child.wait();
    drop(child);

    assert_eq!(
        ns.open(SERVICE).map(|conn| conn.fd()),
        Err(SyscallError::Gone),
        "a port whose only acceptor was killed mid-accept still queued a connection",
    );
    println!("  accept: the port closed and a connect answered Gone");
}

fn child(role: &str) -> ! {
    match role {
        "pipe-read" => {
            say("parked in pipe-read");
            let mut buf = [0u8; 64];
            let n = std::io::stdin().read(&mut buf).expect("read stdin");
            panic!("pipe-read came back with {n} bytes");
        }
        "connection-read" => {
            let ns = endow::namespace().expect("the parent endowed a namespace");
            let conn = ns.open(SERVICE).expect("connect through the endowed connector");
            say("parked in connection-read");
            let mut buf = [0u8; 64];
            let n = syscall::read(conn.as_handle(), &mut buf).expect("read the connection");
            panic!("connection-read came back with {n} bytes");
        }
        "accept" => {
            let acceptor = endow::acceptor(SERVICE).expect("the parent endowed an acceptor");
            say("parked in accept");
            let taken = acceptor.accept().map(|conn| conn.fd());
            panic!("accept came back with {taken:?}");
        }
        other => panic!("unknown role {other:?}"),
    }
}

/// The marker, in one write and flushed: the parent reads it to know the child
/// is *in* the call rather than on the way to it.
///
/// It is printed immediately before the blocking call, so the window between
/// the two is a handful of instructions. A kill that landed inside that window
/// would release the handle from an ordinary drain with nothing stranded, which
/// is the case this file is not about — and the arms would still pass, so the
/// residual is a weaker test rather than a wrong one.
fn say(line: &str) {
    let mut out = std::io::stdout();
    out.write_all(line.as_bytes()).expect("say");
    out.write_all(b"\n").expect("say");
    out.flush().expect("flush");
}

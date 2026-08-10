//! A client works from its first instruction, whether or not the server has
//! reached `accept` — or has been spawned at all.
//!
//! **This is the property the retry loops were hiding.** Connecting used to be
//! resolving a name through a global registry, so a client that started first
//! found nothing and the SDK spun: two loops, one in `audio` and one in `net`,
//! each with a bound and a sleep, each turning "the server is not up yet" into a
//! duration. A port exists before either end's process does, so there is no
//! "not yet" to retry — a connection made against an acceptor nobody holds yet
//! is queued on the port, and the bytes written into it are in the ring waiting
//! when the server first looks.
//!
//! Two arms, and they must answer differently:
//!
//! 1. The client connects and writes **before the server exists**. The server is
//!    started afterwards, and at its very first look — a *non-blocking* read
//!    taken the instruction after `accept` returns — the client's frame is
//!    already there. It replies, and the client reads the reply.
//! 2. The same client, and a server that takes the acceptor and exits. The
//!    client's read answers `0` and its next write is refused. Both are facts
//!    the kernel states, not conclusions a clock reached.
//!
//! **No wall clock is asserted anywhere here, deliberately.** A tree that put a
//! retry back would not fail an arm slowly, it would *hang* — the client would
//! spin against a port that is never served — and a hang is the harness's
//! timeout to report, which is the one place in this project where a duration
//! is allowed to decide anything.

use std::io::{BufRead, BufReader, Write};
use std::os::toyos::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};

use toyos::endow::Endowments;
use toyos::port::Acceptor;
use toyos::{namespace, port, AsHandle};
use toyos_abi::syscall::{self, SyscallError, SVC_LABEL};

const SELF_PATH: &str = "/bin/test_rs_connect_before_serve";
const SERVICE: &str = "before-serve";
/// Where the server role finds its acceptor. A test binary is not a `[programs]`
/// key, so no manifest row can name what it serves and the label is the test's
/// own.
const ACCEPTOR_LABEL: &str = "acceptor";

const QUESTION: &[u8] = b"queued before anyone accepted";
const ANSWER: &[u8] = b"and it was already here";

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("client") => client(),
        Some("server") => server(),
        Some("leaver") => leaver(),
        Some(other) => panic!("unknown role {other:?}"),
        None => test(),
    }
}

fn test() {
    served();
    left();
    println!("a client's frame outlives the absence of its server, and its exit is a word");
}

/// Arm 1. The client is running and has written before the server is spawned.
fn served() {
    let (acceptor, connector) = port::create().expect("a port");
    let (mut client, mut from_client) = start_client(&connector, "reply");

    let mut server = Command::new(SELF_PATH)
        .arg("server")
        .endow(ACCEPTOR_LABEL, acceptor.into_raw().0)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the server");

    let mut from_server = BufReader::new(server.stdout.take().expect("server stdout"));
    let said = line(&mut from_server);
    assert_eq!(
        said,
        format!("already here: {}", String::from_utf8_lossy(QUESTION)),
        "the server's first look did not find the client's frame",
    );

    let heard = line(&mut from_client);
    assert_eq!(
        heard,
        format!("reply: {}", String::from_utf8_lossy(ANSWER)),
        "the client did not read the server's reply",
    );
    assert!(client.wait().expect("wait the client").success(), "the client exited nonzero");
    assert!(server.wait().expect("wait the server").success(), "the server exited nonzero");
    println!("  served: the frame was buffered before the server had started");
}

/// Arm 2. The server takes the acceptor and leaves, which is the whole of the
/// port closing.
fn left() {
    let (acceptor, connector) = port::create().expect("a port");
    let (mut client, mut from_client) = start_client(&connector, "gone");

    let leaver = Command::new(SELF_PATH)
        .arg("leaver")
        .endow(ACCEPTOR_LABEL, acceptor.into_raw().0)
        .status()
        .expect("spawn the leaver");
    assert!(leaver.success(), "the leaver exited nonzero");

    let heard = line(&mut from_client);
    assert_eq!(
        heard, "read 0 then refused",
        "a client of a server that left was told something else",
    );
    assert!(client.wait().expect("wait the client").success(), "the client exited nonzero");
    println!("  left: the client read 0 and its next write was refused");
}

/// Spawn the client and return once its frame is provably in the ring.
///
/// The `sent` line is the ordering: it is printed after the write, so nothing
/// below it can be reading a port the client had not yet written to.
fn start_client(connector: &port::Connector, expect: &str) -> (Child, BufReader<ChildStdout>) {
    let ns = namespace::build().add(SERVICE, connector).finish().expect("a namespace");
    let mut client = Command::new(SELF_PATH)
        .arg("client")
        .arg(expect)
        .endow(SVC_LABEL, ns.into_raw().0)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the client");
    let mut out = BufReader::new(client.stdout.take().expect("client stdout"));
    assert_eq!(line(&mut out), "sent", "the client did not write before the server existed");
    (client, out)
}

fn line(out: &mut BufReader<ChildStdout>) -> String {
    let mut line = String::new();
    out.read_line(&mut line).expect("a line from a child");
    line.trim().to_string()
}

fn say(what: &str) {
    println!("{what}");
    std::io::stdout().flush().expect("flush");
}

fn client() -> ! {
    let expect = std::env::args().nth(2).expect("the client needs to know what to expect");
    let conn = toyos::endow::service(SERVICE).expect("the client holds a connector");
    conn.write_nonblock(QUESTION).expect("write into a port nobody has accepted from");
    say("sent");

    let mut buf = [0u8; 64];
    let n = syscall::read(conn.as_handle(), &mut buf).expect("read the connection");
    match expect.as_str() {
        "reply" => {
            assert_eq!(&buf[..n], ANSWER, "the reply was not the server's");
            say(&format!("reply: {}", String::from_utf8_lossy(&buf[..n])));
        }
        "gone" => {
            assert_eq!(n, 0, "a connection whose server left returned {n} bytes");
            // **`NotFound` and not `Gone`, which §8.2 asks for and
            // `specs/issues/isolation/a-broken-pipe-answers-not-found.md`
            // is about.** What this arm is for is that the client is *told*, by
            // the kernel, without a timer — which either word does.
            assert_eq!(
                conn.write_nonblock(QUESTION),
                Err(SyscallError::NotFound),
                "a write into a port whose acceptor is gone was taken",
            );
            say("read 0 then refused");
        }
        other => panic!("unknown expectation {other:?}"),
    }
    std::process::exit(0);
}

fn server() -> ! {
    let acceptor: Acceptor =
        Endowments::get().take(ACCEPTOR_LABEL).expect("the server was endowed an acceptor");
    let conn = acceptor.accept().expect("accept the queued connection");
    // The instruction after `accept`, and it must not block: the client wrote
    // before this process existed, so the bytes are in the ring already.
    let mut buf = [0u8; 64];
    let n = conn
        .read_nonblock(&mut buf)
        .expect("the client's frame was not in the ring at the server's first look");
    say(&format!("already here: {}", String::from_utf8_lossy(&buf[..n])));
    conn.write_nonblock(ANSWER).expect("reply");
    // Held until the client has read the reply, which its own exit reports.
    let mut wait = [0u8; 1];
    let _ = syscall::read(conn.as_handle(), &mut wait);
    std::process::exit(0);
}

/// Takes the acceptor and leaves. Dropping the last handle to it is the whole
/// of the port closing — there is no unregister and nothing to tell anybody.
fn leaver() -> ! {
    let acceptor: Acceptor =
        Endowments::get().take(ACCEPTOR_LABEL).expect("the leaver was endowed an acceptor");
    drop(acceptor);
    std::process::exit(0);
}

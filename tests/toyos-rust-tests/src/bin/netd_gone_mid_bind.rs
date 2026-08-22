//! A netd that goes away *while* a client is binding is still a netd that is
//! not there.
//!
//! `/bin/sshd` has one clean exit for a machine with no network and it is keyed
//! on an error kind: `ErrorKind::NotConnected`, which is what
//! `NetError::NetdNotFound` becomes. Anything else panics, by design — a
//! machine that *has* a NIC and cannot bind must be loud. On a NIC-less machine
//! netd prints its line and exits, and whether sshd took the quiet arm or put a
//! tokio backtrace across the boot depended on which side of netd's teardown
//! its bind landed:
//! `issues/build/sshd-panics-when-netd-exits-before-it-binds.md`, four sightings,
//! victim `boot_partition_identity`.
//!
//! **The race is not staged here. The sequence is.** What made the defect hard
//! to see is that it is a handful of instructions wide in a real boot; what
//! makes it testable is that nothing about it is actually timing-dependent once
//! the two events are ordered by hand. A port is a port, so this holds one of
//! its own and closes it at a chosen instant, with no netd, no NIC and no
//! clock anywhere.
//!
//! Three arms, and they are three different syscalls answering the same fact:
//!
//! 1. **Gone before the connect.** The port closes first, so
//!    `SYS_NAMESPACE_OPEN` refuses — `SyscallError::Gone`. This is the arm that
//!    already worked, and it is asserted through `std::net::TcpListener` as
//!    well as through the SDK, because `ErrorKind::NotConnected` is the literal
//!    thing `/bin/sshd` matches on.
//! 2. **Gone between the connect and the handle transfer.** The client is
//!    connected and queued when the port closes. `tcp_bind` hands netd a pipe
//!    end, and a request that carries handles moves them *before* it writes the
//!    frame — so `SYS_HANDLE_SEND` is the first thing it can be refused at, and
//!    it answers `SyscallError::Gone`.
//! 3. **The same, for a request that carries no handles**, which reaches the
//!    frame write instead. A pipe with no reader is `SyscallError::NotFound`
//!    (`issues/isolation/a-broken-pipe-answers-not-found.md` is about that word
//!    being `NotFound` rather than `Gone`; either way it is the peer).
//!
//! Arms 2 and 3 are the ones that produced the panic. They are red on a tree
//! where `toyos::net::hangup` maps anything but `IpcError::Disconnected` to
//! `NetError::Io`, which is `ErrorKind::Other`, which is sshd's `panic!`.
//!
//! **Nothing here waits.** The child's ordering edge is a *second* connection
//! to the same port, read with a blocking read: `Acceptor::on_zero_handles`
//! closes every queued connection's inbox and only then drops the connections,
//! so a read that answers `0` on one of them proves the other one's outbox is
//! already closed. The hook is deferred to a drain site
//! (`kernel/src/object/mod.rs`'s `ZERO_QUEUE`), so a sleep would be a guess and
//! this is not one.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};
use std::os::toyos::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};

use toyos::endow::EndowError;
use toyos::net::{
    MsgType, NetError, NetdConn, SocketCloseRequest, TcpBindPipedRequest, TcpBindResponse,
};
use toyos::{namespace, port, AsHandle};
use toyos_abi::syscall::{self, SVC_LABEL};

const SELF_PATH: &str = "/bin/test_rs_netd_gone_mid_bind";

/// The name `NetdConn::connect` resolves, and it is not configurable — so the
/// port this test hands its children has to be called that.
const SERVICE: &str = "netd";

/// sshd's own. Nothing here listens; the number is part of being the same call.
const SSH_PORT: u16 = 22;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("already-gone") => already_gone(),
        Some("bind") => mid_flight(Request::Bind),
        Some("close") => mid_flight(Request::Close),
        Some(other) => panic!("unknown role {other:?}"),
        None => test(),
    }
}

fn test() {
    arm_already_gone();
    arm_mid_flight("bind", "the handle transfer");
    arm_mid_flight("close", "the frame write");
    println!("a netd that leaves mid-bind is a netd that is not there, at all three syscalls");
}

/// Arm 1. The port is closed before the child exists, so its very first
/// `SYS_NAMESPACE_OPEN` is refused.
fn arm_already_gone() {
    let (acceptor, connector) = port::create().expect("a port");
    // The whole of a server leaving: there is no unregister and nothing to tell
    // anybody.
    drop(acceptor);

    let (mut child, mut out) = spawn(&connector, "already-gone");
    assert_eq!(
        line(&mut out),
        "not connected",
        "a client of a port that had already closed was told something else",
    );
    assert!(child.wait().expect("wait the child").success(), "the child exited nonzero");
    println!("  already gone: the connect itself is refused, and sshd's guard sees NotConnected");
}

/// Arms 2 and 3. The child connects first and the port closes under it.
fn arm_mid_flight(role: &str, what: &str) {
    let (acceptor, connector) = port::create().expect("a port");
    let (mut child, mut out) = spawn(&connector, role);
    assert_eq!(line(&mut out), "connected", "the child did not connect before the port closed");

    // Ordered by the line above and by the child's own blocking read below;
    // nothing here is timed.
    drop(acceptor);

    assert_eq!(
        line(&mut out),
        "netd not found",
        "a request into a port that closed after the connect was answered by {what} with \
         something other than a netd that is not there",
    );
    assert!(child.wait().expect("wait the child").success(), "the child exited nonzero");
    println!("  mid-flight: {what} says the peer is gone");
}

/// Spawn `role` with a namespace whose only name is this test's own port.
fn spawn(connector: &port::Connector, role: &str) -> (Child, BufReader<ChildStdout>) {
    let ns = namespace::build().add(SERVICE, connector).finish().expect("a namespace");
    let mut child = Command::new(SELF_PATH)
        .arg(role)
        .endow(SVC_LABEL, ns.into_raw().0)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {role}: {e}"));
    let out = BufReader::new(child.stdout.take().expect("child stdout"));
    (child, out)
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

/// The child of arm 1: no netd was ever there to reach.
fn already_gone() -> ! {
    assert_eq!(
        NetdConn::connect().err(),
        Some(NetError::NetdNotFound),
        "connecting to a closed port answered something other than a missing netd",
    );
    assert_eq!(
        toyos::endow::service(SERVICE).err(),
        Some(EndowError::ServerGone),
        "the kernel's own word for a closed port is not `Gone`",
    );

    // The same call `/bin/sshd` makes, through `std`, on the kind its quiet
    // exit is keyed to. A `SocketAddr` built here rather than parsed, so no
    // resolver is anywhere on this path.
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSH_PORT));
    let refused = TcpListener::bind(addr).expect_err("a bind with no netd behind it succeeded");
    assert_eq!(
        refused.kind(),
        std::io::ErrorKind::NotConnected,
        "sshd's quiet arm is keyed on NotConnected and this bind said {refused}",
    );

    say("not connected");
    std::process::exit(0);
}

/// Which syscall the child's request is refused at.
enum Request {
    /// Carries a pipe end, so `SYS_HANDLE_SEND` goes first — `tcp_bind`'s own
    /// shape.
    Bind,
    /// Carries none, so the frame write is the first thing the kernel sees.
    Close,
}

/// The child of arms 2 and 3.
fn mid_flight(request: Request) -> ! {
    // Two connections, both queued on the port before anything closes it. One
    // is the request's; the other is only ever read from, and its EOF is what
    // says the teardown has happened.
    let watch = toyos::endow::service(SERVICE).expect("the watch connection");
    let netd = NetdConn::connect().expect("a netd connection while the port is open");
    say("connected");

    let mut byte = [0u8; 1];
    let n = syscall::read(watch.as_handle(), &mut byte).expect("read the watch connection");
    assert_eq!(n, 0, "the watch connection carried {n} byte(s); nobody ever wrote to it");

    let err = match request {
        Request::Bind => {
            let (_notify, netd_notify) = toyos::pipe_pair().expect("the notify pipe");
            netd.request_with_handles(
                &[netd_notify.into_raw()],
                MsgType::TcpBindPiped,
                &TcpBindPipedRequest { addr: [0, 0, 0, 0], port: SSH_PORT, _pad: 0 },
            )
            .and_then(|pending| pending.response::<TcpBindResponse>().map(|_| ()))
            .expect_err("a bind into a port whose acceptor is gone was answered")
        }
        Request::Close => netd
            .request(MsgType::TcpClose, &SocketCloseRequest { socket_id: 0 })
            .and_then(|pending| pending.status())
            .expect_err("a request into a port whose acceptor is gone was answered"),
    };

    assert_eq!(
        err,
        NetError::NetdNotFound,
        "a netd that left mid-request is a netd that is not there, and this said {err:?} — \
         which `std` maps to ErrorKind::Other and `/bin/sshd` panics on",
    );

    say("netd not found");
    std::process::exit(0);
}

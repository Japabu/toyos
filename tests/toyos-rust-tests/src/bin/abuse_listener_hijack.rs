//! The service another process serves is not reachable by naming it.
//!
//! The original defect: `Descriptor::Listener` carried the service *name*, and
//! accept, close and poll all re-resolved that string through a global
//! registry. The attack was `listen(name)`, `dup`, `close(original)` — the
//! close unregistered the name and left the dup naming nothing, so when the
//! real service claimed the freed name its own `listen` succeeded, and from
//! that moment the stale fd resolved to *its* listener: `accept` on it took the
//! service's connections and `close` on it unregistered the service. Giving the
//! descriptor a `ListenerId` made a stale fd name nothing forever, and left the
//! squat itself — any process could take any name first.
//!
//! **The whole setup is gone.** There is no registry, no `listen` and no name a
//! process can present: a service is a port, its two ends are two types, and a
//! client is given a `Connector` inside a namespace its parent built. So the
//! squat is not something to detect at run time — it is not something that can
//! be written — and what is left to check is the boundary the type system draws
//! and the kernel enforces underneath it.
//!
//! Four arms, each the runtime half of a thing the compiler already refuses to
//! spell:
//!
//! 1. **A connector cannot accept.** `Connector` has no `accept` method; the
//!    handle behind one is refused by `SYS_ACCEPT` as well.
//! 2. **An acceptor cannot be put in a namespace as a connector.** Two types,
//!    one wire word, and the kernel checks the type rather than trusting it.
//! 3. **A handle number from another process's table names nothing here.** The
//!    child prints the raw number of a live acceptor of its own; this process
//!    presenting it reaches nothing.
//! 4. **A name a namespace does not carry resolves to nothing** — and that is
//!    `NotFound`, which is a different word from a port that has closed.
//!
//! Run with `child` it is the process in arm 3.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use toyos::{namespace, port, AsHandle};
use toyos_abi::syscall::{self, SyscallError};

const SELF_PATH: &str = "/bin/test_rs_abuse_listener_hijack";
const NAME: &str = "abuse-listener-hijack";

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("child") => child(),
        Some(other) => panic!("unknown role {other:?}"),
        None => test(),
    }
}

fn test() {
    let (acceptor, connector) = port::create().expect("a port of our own");

    // 1. The client's end has no read path at all. `Connector` exposes no
    //    `accept`, and the handle under it is refused by the syscall too — so
    //    a client given access to a service cannot take that service's
    //    connections however it addresses the call.
    assert_eq!(
        syscall::accept(connector.as_handle()).err(),
        Some(SyscallError::PermissionDenied),
        "a connector accepted a connection"
    );

    // 2. And the server's end is not a ticket to hand out: an acceptor in an
    //    `add` entry is refused, so nothing can build a namespace whose entry
    //    hands the *acceptor* to whoever holds it.
    let smuggled = namespace::build().add(NAME, unsafe { &fake_connector(&acceptor) }).finish();
    assert_eq!(
        smuggled.err(),
        Some(SyscallError::PermissionDenied),
        "an acceptor was accepted as a namespace's connector"
    );

    // 3. A handle is an index into one process's own table and means nothing
    //    outside it. The child holds a live acceptor and says what number it
    //    is; presenting that number here reaches whatever this process has at
    //    that slot, and never the child's port.
    let child = Command::new(SELF_PATH)
        .arg("child")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the child");
    let mut out = BufReader::new(child.stdout.expect("child stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("the child's acceptor handle");
    let theirs = toyos_abi::RawHandle(line.trim().parse().expect("a handle number"));
    match syscall::accept(theirs) {
        Err(SyscallError::NotFound) | Err(SyscallError::PermissionDenied) => {}
        Err(e) => panic!("another process's handle number answered {e:?}"),
        Ok(_) => panic!("another process's handle number accepted a connection"),
    }

    // 4. A name is resolved in a namespace this process holds, and in no other
    //    place. One that is not in it is `NotFound` — a fact about this
    //    process — and not `Gone`, which is a server that has left.
    let ns = namespace::build().add(NAME, &connector).finish().expect("a namespace of our own");
    assert!(ns.open(NAME).is_ok(), "our own port did not answer");
    assert_eq!(
        ns.open("something-we-were-not-given").err(),
        Some(SyscallError::NotFound),
        "a name outside the namespace resolved"
    );

    println!("a connector cannot accept, an acceptor cannot be a connector, and a handle is one process's");
}

/// An `Acceptor`'s handle wearing a `Connector`'s type, which is the only way
/// to make the kernel decide arm 2 — the SDK's two types make it unwritable.
///
/// # Safety
/// The returned value must not outlive `acceptor` and must not be dropped as an
/// owning handle; it is read for its number and nothing else.
unsafe fn fake_connector(acceptor: &port::Acceptor) -> core::mem::ManuallyDrop<port::Connector> {
    core::mem::ManuallyDrop::new(unsafe { port::Connector::from_raw(acceptor.as_handle()) })
}

fn child() {
    let (acceptor, _connector) = port::create().expect("child: a port of its own");
    println!("{}", acceptor.as_handle().0);
    // Held for the life of this process: the number above must name something
    // live, or arm 3 would pass against a table slot that is simply empty.
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

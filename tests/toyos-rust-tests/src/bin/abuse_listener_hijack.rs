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
//! spell. **They no longer answer the same way, and the split is the design.**
//! An attenuated handle is a thing a process may legitimately probe, so arms 1,
//! 2 and 4 come back as words; naming a handle from somebody else's table is a
//! bug no correct program has, so arm 3 ends the caller and is raised in a child
//! that prints a marker first.
//!
//! 1. **A connector cannot accept.** `Connector` has no `accept` method, and
//!    the handle behind one carries no `READ` — so `SYS_ACCEPT` refuses it with
//!    a word before it ever looks at the type.
//! 2. **An acceptor cannot be put in a namespace as a connector.** Two types,
//!    one wire word, and the kernel checks the type rather than trusting it.
//!    **It answers `InvalidArgument` rather than ending the caller, and it is
//!    the only wrong-typed handle in the ABI that does**: an `add` entry's
//!    connector is routinely one a *peer* transferred — a `provides` name is
//!    exactly that — so presenting the wrong one may be reporting a peer's bug
//!    rather than your own. `/bin/init`'s launcher is why, and
//!    `launcher_refusals` is the other end of the same property.
//! 3. **A handle number from another process's table names nothing here.** The
//!    victim prints the raw number of a live acceptor of its own; the thief
//!    presenting it is ended, and the victim's port is still serving afterwards.
//! 4. **A name a namespace does not carry resolves to nothing** — and that is
//!    `NotFound`, which is a different word from a port that has closed.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use toyos::{namespace, port, AsHandle};
use toyos_abi::syscall::{self, SyscallError};

const SELF_PATH: &str = "/bin/test_rs_abuse_listener_hijack";
const NAME: &str = "abuse-listener-hijack";

/// `process::HANDLE_FAULT_EXIT_CODE`.
const HANDLE_FAULT: i32 = 139;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("victim") => victim(),
        Some("thief") => thief(),
        Some(other) => panic!("unknown role {other:?}"),
        None => test(),
    }
}

fn test() {
    let (_acceptor, connector) = port::create().expect("a port of our own");

    // 1. The client's end has no read path at all. `Connector` exposes no
    //    `accept`, and the handle under it carries no `READ` — so a client
    //    given access to a service cannot take that service's connections
    //    however it addresses the call.
    assert_eq!(
        syscall::accept(connector.as_handle()).err(),
        Some(SyscallError::PermissionDenied),
        "a connector accepted a connection"
    );

    // 2. And the server's end is not a ticket to hand out: an acceptor in an
    //    `add` entry is a wrong-typed handle, so nothing can build a namespace
    //    whose entry hands the *acceptor* to whoever holds it. A word rather
    //    than a death — see the header — so it is raised here and needs no
    //    child and no marker.
    let (smuggled, its_connector) = port::create().expect("a second port, to smuggle");
    // SAFETY: read for its number and nothing else. `ManuallyDrop` is what
    // stops it being closed as a second owner, and it does not outlive
    // `smuggled`.
    let fake = core::mem::ManuallyDrop::new(unsafe {
        port::Connector::from_raw(smuggled.as_handle())
    });
    assert_eq!(
        namespace::build().add(NAME, &fake).finish().err(),
        Some(SyscallError::InvalidArgument),
        "an acceptor was taken as a namespace's connector",
    );
    // The *same port's* connector is taken, so the refusal was the type and
    // not the port — which is what stops this arm passing against a
    // `SYS_NAMESPACE_BUILD` that refuses everything.
    namespace::build()
        .add(NAME, &its_connector)
        .finish()
        .expect("the same port's connector is a namespace entry");
    drop(smuggled);

    // 3. A handle is an index into one process's own table and means nothing
    //    outside it. The victim holds a live acceptor and says what number it
    //    is; a sibling presenting that number is ended where it stands, and the
    //    victim's port is untouched — which is what stops this arm passing
    //    against a number that named nothing anywhere.
    let mut victim = Command::new(SELF_PATH)
        .arg("victim")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the victim");
    let mut out = BufReader::new(victim.stdout.take().expect("victim stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("the victim's acceptor handle");
    let theirs = line.trim().to_string();
    assert!(theirs.parse::<u32>().is_ok(), "the victim published {theirs:?}");

    killed("thief", Some(&theirs), "another process's handle number accepted a connection");

    drop(victim.stdin.take());
    let mut said = String::new();
    out.read_to_string(&mut said).expect("the victim's report");
    assert!(victim.wait().expect("wait the victim").success(), "the victim exited nonzero");
    assert_eq!(
        said.trim(),
        "port still mine",
        "the victim's port did not survive the number being presented elsewhere",
    );

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

/// Run `role` and require that the kernel ended it at its call.
///
/// The marker is what gives the arm teeth: without it a child that failed
/// before reaching the call would pass, having asserted nothing.
fn killed(role: &str, arg: Option<&str>, what_would_be_wrong: &str) {
    let mut command = Command::new(SELF_PATH);
    command.arg(role);
    command.args(arg);
    let child =
        command.stdout(Stdio::piped()).spawn().unwrap_or_else(|e| panic!("spawn {role}: {e}"));
    let out = child.wait_with_output().unwrap_or_else(|e| panic!("wait {role}: {e}"));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("reached {role}"),
        "{role} never reached its call",
    );
    assert_eq!(out.status.code(), Some(HANDLE_FAULT), "{what_would_be_wrong}");
}

fn marker(role: &str) {
    println!("reached {role}");
    std::io::stdout().flush().expect("flush the marker");
}

/// Presents a number the victim published. It holds no acceptor of its own, so
/// whatever is at that slot here is not a port anybody serves.
fn thief() -> ! {
    let theirs = toyos_abi::RawHandle(
        std::env::args().nth(2).expect("thief needs a handle number").parse().expect("a number"),
    );
    marker("thief");
    let taken = syscall::accept(theirs);
    panic!("another process's handle number answered {taken:?}");
}

/// Holds a live port for the length of arm 3, and proves afterwards that it
/// still holds it.
fn victim() -> ! {
    let (acceptor, connector) = port::create().expect("victim: a port of its own");
    println!("{}", acceptor.as_handle().0);
    std::io::stdout().flush().expect("victim: flush");

    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);

    // Nobody else ever reached this port, and it is still the victim's to
    // accept from: the number it published named something live for the whole
    // of the attack.
    let ns = namespace::build()
        .add(NAME, &connector)
        .finish()
        .expect("victim: a namespace over its own port");
    let _client = ns.open(NAME).expect("victim: connect to its own port");
    let _served = acceptor.accept().expect("victim: accept its own connection");
    println!("port still mine");
    std::io::stdout().flush().expect("victim: flush the report");
    std::process::exit(0);
}

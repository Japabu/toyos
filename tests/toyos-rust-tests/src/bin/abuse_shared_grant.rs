//! A region is reachable by the process that was **sent** it, and by nobody
//! else.
//!
//! This test used to be about `shared_memory::grant`'s ACL: the list accepted
//! the owner *or anyone already on it*, so permission was transitive and
//! unreported, and the target pid was unchecked so the list would take pids
//! that had never existed. None of that has a spelling any more — a region is a
//! handle, holding one is the whole of being allowed to map it, and giving one
//! away is `SYS_HANDLE_SEND` over a connection the giver already holds.
//!
//! So the subject moves to what replaced it. The negative arm is the one the
//! ACL could never state: a process that was sent nothing cannot reach the
//! region **by any number at all**, including the exact handle value its owner
//! is using — and it does not merely fail to, it is ended for trying, because
//! naming a handle you were not given is a bug rather than a request. The
//! positive arm is what makes that non-vacuous: the same region, reached by the
//! peer that was sent it, with the secret in it.
//!
//! Three roles. `invited` is spawned holding a connector and is sent the
//! region; `uninvited` is spawned holding nothing and told the owner's handle
//! value; `dropped` is the other end of the same fact — a region whose last
//! handle is gone leaves a number that is not a name for what lands there next.

use std::io::{BufRead, BufReader, Write};
use std::os::toyos::process::CommandExt;
use std::process::{Child, Command, Stdio};

use toyos::shm::SharedMemory;
use toyos::{namespace, port, AsHandle};
use toyos_abi::syscall::{self, SVC_LABEL};
use toyos_abi::RawHandle;

const SELF_PATH: &str = "/bin/test_rs_abuse_shared_grant";
const SECRET: &[u8] = b"owner-private-bytes-do-not-share";
const REGION: usize = 4096;
const SERVICE: &str = "region";

/// `process::HANDLE_FAULT_EXIT_CODE`.
const HANDLE_FAULT: i32 = 139;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("invited") => invited(),
        Some("uninvited") => uninvited(),
        Some("dropped") => dropped(),
        Some(other) => panic!("unknown role {other:?}"),
        None => owner(),
    }
}

fn owner() {
    let mut region = SharedMemory::create(REGION).expect("a region of our own");
    region.as_mut_slice()[..SECRET.len()].copy_from_slice(SECRET);
    let own_handle = region.as_handle();

    let (acceptor, connector) = port::create().expect("the kernel refused a port");

    // The peer that is *given* the region. It holds one connector and this is
    // where it points; it never names a number, because being sent the handle
    // is the whole of how it gets there.
    let ns = namespace::build()
        .add(SERVICE, &connector)
        .finish()
        .expect("the kernel refused a namespace");
    let mut invited = Command::new(SELF_PATH)
        .arg("invited")
        .endow(SVC_LABEL, ns.into_raw().0)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the invited peer");

    let conn = acceptor.accept().expect("the invited peer connects");
    let shared = region.share().expect("a second handle to our own region");
    syscall::handle_send(conn.as_handle(), &[shared]).expect("send the region to its peer");
    // The frame the handle was sent ahead of. The peer reads the frame first
    // and is guaranteed to find the handle already queued.
    conn.signal(1).expect("announce the region");

    let said = report(&mut invited);
    assert!(invited.wait().expect("wait the invited peer").success(), "the invited peer failed");
    assert_eq!(
        said,
        format!("read {}", String::from_utf8_lossy(SECRET)),
        "the peer that was sent the region could not read it",
    );

    // The peer that is given *nothing*. Same binary, same argument — the exact
    // handle value the owner is using — and no connection to this process at
    // all. It does not read the wrong bytes and it does not get an error word:
    // it is ended at the call.
    killed("uninvited", &own_handle.0.to_string(), "a process that was sent nothing reached the region");

    // A region whose last handle is gone is gone. There is no `release` and no
    // list to take a name off: the drop is the whole of it, and the number that
    // named it is a closed slot rather than a name for whatever lands there
    // next. Raised in a child on its own region, because it is the same fatal
    // fact from the owning side.
    killed("dropped", "", "a handle its owner had closed still named a region");

    // And through all of it the owner's own region is still the owner's, and
    // the invited peer's mapping was its own handle's business rather than a
    // second name for this one.
    assert_eq!(&region.as_slice()[..SECRET.len()], SECRET, "the owner's own region changed");

    println!("the region reached the peer it was sent to and no other, and a closed handle names nothing");
}

/// The one line a peer reports.
///
/// **No handshake, and that is a constraint rather than a simplification**: the
/// owner is blocked in `accept` when the invited peer starts, so a peer told to
/// wait for a go-ahead would be waiting for a process that is waiting for it.
fn report(child: &mut Child) -> String {
    let mut out = BufReader::new(child.stdout.take().expect("peer stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("peer report");
    line.trim().to_string()
}

/// Run `role` and require that the kernel ended it at its call.
///
/// The marker is what gives the arm teeth: a child that died before reaching
/// the call would otherwise pass while asserting nothing.
fn killed(role: &str, arg: &str, what_would_be_wrong: &str) {
    let mut command = Command::new(SELF_PATH);
    command.arg(role);
    if !arg.is_empty() {
        command.arg(arg);
    }
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

fn invited() {
    let conn = toyos::endow::service(SERVICE).expect("the invited peer holds a connector");
    // The frame first, then the handle it was sent ahead of.
    conn.recv_header().expect("the owner's announcement");
    let [handle] = conn.recv_handles_exact::<1>().expect("the region the owner sent");
    let region = SharedMemory::adopt(handle, REGION).expect("adopt the region");
    println!("read {}", String::from_utf8_lossy(&region.as_slice()[..SECRET.len()]));
    std::io::stdout().flush().expect("invited: flush the report");
}

/// Names the owner's number. Nothing about this process makes that number a
/// name for anything, and the kernel's answer is to end it.
fn uninvited() -> ! {
    let guess = RawHandle(
        std::env::args().nth(2).expect("uninvited needs the owner's handle value").parse().unwrap(),
    );
    marker("uninvited");
    let reached = unsafe { syscall::shm_map(guess) };
    panic!("the owner's handle value answered {:?} here", reached.map(|_| ()));
}

/// Names a region of its own that it has closed.
fn dropped() -> ! {
    let region = SharedMemory::create(REGION).expect("a region to close");
    let closed = region.as_handle();
    drop(region);
    marker("dropped");
    let reached = unsafe { syscall::shm_map(closed) };
    panic!("a closed handle answered {:?}", reached.map(|_| ()));
}

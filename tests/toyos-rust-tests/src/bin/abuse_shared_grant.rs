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
//! is using. The positive arm is what makes that non-vacuous — the same region,
//! reached by the peer that was sent it, with the secret in it.
//!
//! Two roles. The default role owns the region and serves a port; `peer` is
//! spawned twice from it, once holding a connector and once holding nothing.

use std::io::Write;
use std::os::toyos::process::CommandExt;
use std::process::{Command, Stdio};

use toyos::shm::SharedMemory;
use toyos::{namespace, port, AsHandle};
use toyos_abi::syscall::{self, SyscallError, SVC_LABEL};
use toyos_abi::RawHandle;

const SELF_PATH: &str = "/bin/test_rs_abuse_shared_grant";
const SECRET: &[u8] = b"owner-private-bytes-do-not-share";
const REGION: usize = 4096;
const SERVICE: &str = "region";

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("peer") => peer(),
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
    // where it points.
    let ns = namespace::build()
        .add(SERVICE, &connector)
        .finish()
        .expect("the kernel refused a namespace");
    let mut invited = Command::new(SELF_PATH)
        .arg("peer")
        .arg(own_handle.0.to_string())
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
    assert_eq!(
        said,
        format!("read {}", String::from_utf8_lossy(SECRET)),
        "the peer that was sent the region could not read it",
    );

    // The peer that is given *nothing*. Same binary, same argument — the exact
    // handle value the owner is using — and no namespace, so it holds no
    // connection to this process at all.
    let mut uninvited = Command::new(SELF_PATH)
        .arg("peer")
        .arg(own_handle.0.to_string())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the uninvited peer");
    let said = report(&mut uninvited);
    assert!(
        said.starts_with("no region"),
        "a process that was sent nothing reached the owner's region: {said:?}",
    );

    // A region whose last handle is gone is gone. There is no `release` and no
    // list to take a name off: the drop is the whole of it, and the mapping the
    // invited peer holds is its own handle's business.
    drop(region);
    assert_eq!(
        unsafe { syscall::shm_map(own_handle) }.err(),
        Some(SyscallError::NotFound),
        "the owner's own handle still resolved after it was dropped",
    );

    println!("the region reached the peer it was sent to and no other, and its handle is gone");
}

/// The one line a peer reports, and its exit.
///
/// **No handshake, and that is a constraint rather than a simplification**: the
/// owner is blocked in `accept` when the invited peer starts, so a peer told to
/// wait for a go-ahead would be waiting for a process that is waiting for it.
fn report(child: &mut std::process::Child) -> String {
    use std::io::{BufRead, BufReader};
    let mut out = BufReader::new(child.stdout.take().expect("peer stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("peer report");
    assert!(child.wait().expect("wait peer").success(), "peer exited nonzero");
    line.trim().to_string()
}

fn peer() {
    let guess = RawHandle(
        std::env::args().nth(2).expect("peer needs the owner's handle value").parse().unwrap(),
    );
    // Whatever this process ends up holding, the *number* is never how it got
    // there. Tried first, and on its own it must reach nothing.
    let guessed = unsafe { syscall::shm_map(guess) };

    let sent = toyos::endow::service(SERVICE).ok().and_then(|conn| {
        // The frame first, then the handle it was sent ahead of.
        conn.recv_header().ok()?;
        let [region] = conn.recv_handles_exact::<1>()?;
        SharedMemory::adopt(region, REGION).ok()
    });

    match sent {
        Some(region) => {
            println!("read {}", String::from_utf8_lossy(&region.as_slice()[..SECRET.len()]))
        }
        None => println!("no region ({guessed:?})"),
    }
    std::io::stdout().flush().expect("peer: flush report");
}

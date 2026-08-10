//! What a client can make `/bin/init` do by sending it a bad launch.
//!
//! **init is the one process the machine cannot lose.** It holds the only
//! `SysCap`, every unhanded acceptor and the `launcher` port, and nothing
//! restarts it — so a client that can end it, panic it or grow its handle
//! table without bound takes the machine's ability to start a process with it.
//! Every field of `MSG_LAUNCH` and every handle in its batch is a client's
//! claim about itself, and the launcher connector is held by the compositor,
//! every terminal, every shell and sshd.
//!
//! Three shapes, each of which was reachable before this gate existed:
//!
//! 1. **A frame whose handle count is not the batch's.** The handles are
//!    already in init's table when the count is checked, so a refusal that
//!    returns without closing them leaks one per attempt — and a client picks
//!    how often it attempts. Measured against the kernel's live-object census
//!    rather than believed: the batch is a duplicate of a pipe end this
//!    process then drops, so the object survives exactly if init kept it.
//! 2. **An extra that is not a connector.** init has no way to ask what a
//!    handle it received names, so it hands it to `SYS_NAMESPACE_BUILD` — and
//!    a wrong type there used to end the caller, which is init.
//! 3. **A connector the client narrowed `DUP` away from.** init duplicates a
//!    provided connector so the namespace and the label can both carry one; a
//!    duplicate that is refused used to be an `.expect`.
//!
//! The fourth arm is what stops the other three passing on a dead launcher: an
//! ordinary spawn, which goes through init because this process holds a
//! `launcher` connector and `/bin/toybox` is a declared program. It runs last,
//! so it also asserts init survived all three.

use std::process::Command;

use toyos::launch::{self, Launch};
use toyos::{namespace, port, AsHandle};
use toyos_abi::handle::Rights;
use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::RawHandle;

/// `SYS_DEBUG` action 14: how many kernel objects are alive right now.
const CENSUS_TOTAL: u64 = 14;

/// Rounds per census sample. Large enough that one leaked handle per round is
/// a number no drain lag can hide.
const ROUNDS: usize = 16;

/// A program `tests/netcase` declares that serves nothing and provides
/// nothing, so a refused launch of it takes no acceptor with it.
const DECLARED: &str = "/bin/toybox";

fn main() {
    the_kernel_answers_rather_than_faults();
    not_a_connector();
    a_connector_it_cannot_duplicate();

    let before = churn();
    let after = churn();
    assert!(
        after <= before,
        "{ROUNDS} more refused launches left {} more live objects ({before} then {after}): \
         init is keeping the handles a refusal took",
        after.saturating_sub(before),
    );
    println!("  census: {before} live objects, then {after}");

    the_launcher_still_works();
    println!("a bad launch is refused, and init is still the launcher");
}

fn launcher() -> toyos::ipc::Connection {
    toyos::endow::service("launcher").expect("this process was endowed a launcher connector")
}

/// One frame that promises no handles, with two in the batch beside it.
///
/// init cannot answer this — it does not know which handle was for what — so
/// there is no reply to read. What it must do is close them.
fn a_frame_that_lies() {
    let (read, write) = toyos::pipe_pair().expect("a pipe of our own");
    let first = syscall::dup(write.as_handle()).expect("a duplicate to send");
    let second = syscall::dup(write.as_handle()).expect("a second duplicate to send");

    let mut buf = [0u8; 512];
    let request =
        Launch { program: DECLARED, argv: b"", env: b"", cwd: "/", extras: &[], slots: &[] };
    let len = request.encode(&mut buf).expect("encode a launch");

    let conn = launcher();
    conn.send_bytes_with_handles(&[first, second], launch::MSG_LAUNCH, &buf[..len])
        .expect("the launcher took the frame");
    drop(conn);
    // Both ends go, so the pipe's objects are alive after this only if init
    // still holds one of the duplicates.
    drop(read);
    drop(write);
}

fn churn() -> u64 {
    for _ in 0..ROUNDS {
        a_frame_that_lies();
    }
    // **A launch init answers, and deliberately not one it grants.** init is
    // single-threaded and serves connections in the order they queued, so an
    // answer to a request sent after the sixteen is the proof it has served
    // every one of them. A *granted* launch would put a process exit inside
    // the sampled window, and an exiting process leaves objects on the
    // deferred release queue — the sample would be reading that lag.
    assert_eq!(a_launch_it_refuses(), launch::MSG_REFUSED, "the synchronising launch was granted");
    syscall::debug(CENSUS_TOTAL)
}

/// A launch init answers and does not grant: an extra naming a pipe where a
/// connector belongs.
fn a_launch_it_refuses() -> u32 {
    let (_read, write) = toyos::pipe_pair().expect("a pipe of our own");
    let handle = syscall::dup(write.as_handle()).expect("a duplicate to send");
    refused_with(&[("surface", handle)])
}

fn not_a_connector() {
    assert_eq!(a_launch_it_refuses(), launch::MSG_REFUSED);
    println!("  not a connector: refused, and init is still here");
}

/// A real connector, narrowed so init cannot duplicate it.
fn a_connector_it_cannot_duplicate() {
    let (_acceptor, connector) = port::create().expect("a port of our own");
    // Everything `SYS_NAMESPACE_BUILD` asks for and nothing `dup` does, so
    // init gets past the namespace and fails on the label.
    let narrowed = syscall::dup_narrowed(connector.as_handle(), Rights::TRANSFER)
        .expect("a connector carrying only TRANSFER");
    assert_eq!(refused_with(&[("surface", narrowed)]), launch::MSG_REFUSED);
    println!("  a connector it cannot duplicate: refused, and init is still here");
}

/// Send one launch carrying `extras` and answer the message type init replied
/// with. The reply is the liveness proof as much as the verdict.
fn refused_with(extras: &[(&str, RawHandle)]) -> u32 {
    let mut buf = [0u8; 512];
    let request =
        Launch { program: DECLARED, argv: b"", env: b"", cwd: "/", extras, slots: &[] };
    let (handles, count) = request.handles();
    let len = request.encode(&mut buf).expect("encode a launch");

    let conn = launcher();
    conn.send_bytes_with_handles(&handles[..count], launch::MSG_LAUNCH, &buf[..len])
        .expect("the launcher took the frame");
    conn.recv_header().expect("init answered the launch").msg_type
}

/// The non-vacuity arm. `/bin/toybox` is a `[programs]` key, so a caller
/// holding a `launcher` connector reaches it through init and not through
/// `SYS_SPAWN` — this only passes while init is alive and still launching.
fn the_launcher_still_works() {
    let out = Command::new(DECLARED)
        .arg("pwd")
        .output()
        .expect("the launcher started a declared program");
    assert!(out.status.success(), "the launched program exited {:?}", out.status.code());
}

/// **The half of it that is the kernel's**, asserted from a process that can
/// afford to die so that init does not have to.
///
/// `SYS_NAMESPACE_BUILD`'s added connector is the one handle argument in the
/// ABI that routinely crossed a trust boundary — a `provides` name is exactly
/// a connector somebody else made — so a wrong type there answers a word. Every
/// other `WrongType` in the table still ends the caller, and if this one goes
/// back to doing that, this arm never returns and the test reds on exit 139.
fn the_kernel_answers_rather_than_faults() {
    let (_read, write) = toyos::pipe_pair().expect("a pipe of our own");
    // SAFETY: it is not a connector, which is the point — the call must answer
    // a word rather than end this process.
    let pretend = unsafe { port::Connector::from_raw(write.as_handle()) };
    let refused = namespace::build().add("surface", &pretend).finish();
    let _ = pretend.into_raw();
    assert_eq!(refused.err(), Some(SyscallError::InvalidArgument));
}

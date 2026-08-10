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
//!
//! **A fourth shape, and it is the one init could not survive at all: a client
//! that connects and says nothing.** `serve_launch`'s first statement was a
//! blocking `recv_header` on the fresh connection, so two syscalls from any
//! holder of the connector — the compositor, every terminal, every shell, sshd
//! — parked the machine's only way to start a process for ever, with init alive
//! and looking healthy.
//!
//! **Every answer this file waits for is bounded, and that is not decoration.**
//! A test that hangs instead of failing is worse than no test: a harness
//! timeout is a liveness guard rather than a verdict, and a guest that never
//! returns takes its whole shared boot down with it. So the launcher's replies
//! are read with [`answer_within`], and a launcher that has stopped answering
//! is an assertion with a name on it. The one arm that cannot be bounded from
//! here — `Command`, which blocks inside `std` — runs after a bounded launch
//! has already proved init is answering.

use std::process::Command;
use std::time::{Duration, Instant};

use toyos::census::Census;
use toyos::ipc::{Connection, FrameRx, RxStep};
use toyos::launch::{self, Launch};
use toyos::{namespace, port, AsHandle};
use toyos_abi::handle::Rights;
use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::RawHandle;

/// Rounds per census sample. Large enough that one leaked handle per round is
/// a number no drain lag can hide.
const ROUNDS: usize = 16;

/// How long init may take to answer a launch before this file calls it wedged.
///
/// Generous by two orders of magnitude: a refusal is a frame decode and a
/// namespace build, and a grant is one `SYS_SPAWN`. What this bounds is the
/// launcher that answers *never*, and the number only decides how long the red
/// takes to arrive.
const ANSWER_BUDGET: Duration = Duration::from_secs(5);

/// Clients that connect to the launcher and then say nothing, held open across
/// the launch that must still be answered.
///
/// Well under init's own `MAX_PENDING_LAUNCHES`, because what is under test is
/// that a silent client costs a slot rather than the event loop — not the bound
/// on how many slots there are.
const QUIET_CLIENTS: usize = 8;

/// A program `tests/netcase` declares that serves nothing and provides
/// nothing, so a refused launch of it takes no acceptor with it.
const DECLARED: &str = "/bin/toybox";

fn main() {
    the_kernel_answers_rather_than_faults();
    a_quiet_client_does_not_wedge_the_launcher();
    not_a_connector();
    a_connector_it_cannot_duplicate();

    let before = churn();
    let after = churn();
    let grown: Vec<_> = after.grown_since(&before).collect();
    assert!(
        grown.is_empty(),
        "{ROUNDS} more refused launches left more live objects behind: {grown:?} — \
         first {before}, then {after}: init is keeping the handles a refusal took",
    );
    println!("  census: {} live objects, then {}", before.total(), after.total());

    the_launcher_still_works();
    println!("a bad launch is refused, and init is still the launcher");
}

fn launcher() -> Connection {
    toyos::endow::service("launcher").expect("this process was endowed a launcher connector")
}

/// Read the launcher's reply without ever blocking on it.
///
/// `Err` is the verdict this file exists to be able to reach: a launcher that
/// has not answered inside the budget is one no `recv_header` would ever come
/// back from.
fn answer_within(conn: &Connection, budget: Duration) -> Result<u32, &'static str> {
    let deadline = Instant::now() + budget;
    // The replies are bare headers, so nothing of a payload has to be kept.
    let mut rx = FrameRx::<0>::new();
    loop {
        match rx.pump(conn) {
            RxStep::Frame { msg_type, .. } => return Ok(msg_type),
            RxStep::Eof => return Err("the launcher dropped the connection"),
            RxStep::Malformed => return Err("the launcher sent a frame this protocol cannot describe"),
            RxStep::Idle => {
                if Instant::now() >= deadline {
                    return Err("the launcher never answered");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// Clients that connect and go quiet, and a launch that must be answered anyway.
///
/// Two silences, because they park a server at different statements: a
/// connection that never writes a byte, and one that writes half a header and
/// stops. The first is what `accept` used to be fused to; the second is what a
/// frame read in one blocking call used to wait out.
fn a_quiet_client_does_not_wedge_the_launcher() {
    let quiet: Vec<Connection> = (0..QUIET_CLIENTS).map(|_| launcher()).collect();
    let half = launcher();
    half.write_nonblock(&[0u8; 4]).expect("half a frame header");

    // **A launch init answers and does not grant.** What is under test is that
    // the event loop reaches a frame at all while other connections are silent;
    // a *granted* launch would put a spawned process's output on init's own
    // stdio, which is this boot's console, and hand back a `Process` handle for
    // the census arm below to account for.
    let conn = launcher();
    let mut buf = [0u8; 512];
    let request = Launch {
        program: "/bin/no-such-program",
        argv: b"",
        env: b"",
        cwd: "/",
        extras: &[],
        slots: &[],
    };
    let len = request.encode(&mut buf).expect("encode a launch");
    conn.send_bytes_with_handles(&[], launch::MSG_LAUNCH, &buf[..len])
        .expect("the launcher took the frame");

    match answer_within(&conn, ANSWER_BUDGET) {
        Ok(launch::MSG_NOT_DECLARED) => {}
        Ok(other) => panic!("the launcher answered {other} for a program nothing declares"),
        Err(why) => panic!(
            "{QUIET_CLIENTS} clients that said nothing and one that said half a header \
             left the machine unable to start a process: {why}",
        ),
    }
    drop(quiet);
    drop(half);
    println!("  quiet clients: {QUIET_CLIENTS} silent and one half-spoken, and a launch still ran");
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

fn churn() -> Census {
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
    Census::now()
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
    answer_within(&conn, ANSWER_BUDGET).expect("init answered the launch")
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

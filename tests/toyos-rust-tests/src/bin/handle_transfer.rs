//! A batch of handles crossing a connection, and what happens at each point a
//! peer can die.
//!
//! `specs/capability-endowment-spec.md` §8.6 requires this gate and nothing in
//! the tree was it: the only two `handle_send` call sites in the whole test
//! estate sent a `SharedMem`, which is why three of this branch's defects lived
//! here unseen.
//!
//! Five arms, one per point a batch can be at.
//!
//! 1. **The peer is already gone.** The send answers `Gone` and **the handles
//!    are still the sender's**. That is not a nicety: the batch used to be
//!    taken out of the table and dropped on the refusal, so a caller told
//!    `ResourceExhausted` — ordinary backpressure — had silently lost the
//!    capabilities it was about to retry with, and its own `close` of one was
//!    `Stale`, which ends it. `/bin/init` was that caller.
//! 2. **The queue is full.** `MAX_QUEUED_BATCHES` refuses by name, and the
//!    refused batch is still the sender's for the same reason.
//! 3. **Sent and never received.** The peer dies holding the batch; the queue
//!    releases it, and the census says so per kind.
//! 4. **Sent by a peer that then died.** The batch is still there and still
//!    works: what a process sent is the receiver's, not something its exit
//!    retracts.
//! 5. **An `immediate` object inside a `deferred` one.** `kobject!` classifies
//!    each object, and a `deferred` container may hold an `immediate` member: a
//!    dirty `File` sent over a connection whose peer dies is flushed from
//!    `drain_zero_handles`. Nothing in the tree constructed that before, which
//!    is why the suite was green over it.
//!
//! Every arm is followed by the per-kind census, because a total hides a leak
//! of one kind behind churn in another.

use std::io::{Read, Write};
use std::os::toyos::process::CommandExt;
use std::process::{Command, Stdio};

use toyos::census::Census;
use toyos::ipc::Connection;
use toyos::{endow, namespace, port, AsHandle};
use toyos_abi::syscall::{
    self, debug_action, OpenFlags, SyscallError, MAX_QUEUED_BATCHES, MAX_TRANSFER_HANDLES,
    SVC_LABEL,
};
use toyos_abi::RawHandle;

const SELF_PATH: &str = "/bin/test_rs_handle_transfer";

/// The name the child's namespace carries the connector under.
const SERVICE: &str = "transfer";

/// A file on the one mount in this config that reaches a real block device, so
/// the flush arm exercises the deep path rather than tmpfs.
const DIRTY_PATH: &str = "/home/handle_transfer_flush.bin";
const DIRTY_BYTES: &[u8] = b"a file whose last handle went while it was queued";

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some(role) => child(role),
        None => test(),
    }
}

fn test() {
    let before = Census::now();

    a_dead_peer_keeps_nothing();
    a_full_queue_keeps_nothing();
    an_unreceived_batch_is_released();
    a_senders_exit_does_not_retract_what_it_sent();
    an_immediate_object_is_flushed_off_the_idle_stack();

    let after = Census::now();
    // **`Process` is excluded, and this is why.** A `ProcessObject` outlives its
    // last handle by design: the process table holds one for the process's whole
    // life and gives it up when the *scheduler* retires the task, which is a
    // later pass and not a syscall this test makes. Counting it here would be
    // timing the reaper. `handle_kill_policy` and `process_lifecycle` are where
    // process lifetime is asserted.
    let grown: Vec<_> =
        after.grown_since(&before).filter(|(kind, _, _)| *kind != "Process").collect();
    assert!(
        grown.is_empty(),
        "handle transfer left more live objects behind: {grown:?} — \
         first {before}, then {after}",
    );
    println!("a batch crosses, and every way its peer can die gives the objects back");
}

/// Spawn a child holding a connector for a fresh port, and accept its
/// connection. The child is this binary in one of the roles below.
fn peer(role: &str) -> (Connection, std::process::Child) {
    let (acceptor, connector) = port::create().expect("a port of our own");
    let ns = namespace::build()
        .add(SERVICE, &connector)
        .finish()
        .expect("a namespace carrying one connector");
    let child = Command::new(SELF_PATH)
        .arg(role)
        .endow(SVC_LABEL, ns.into_raw().0)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {role}: {e}"));
    let conn = acceptor.accept().expect("the child connected");
    (conn, child)
}

/// Wait for the child's one line, which is how it says it has reached the state
/// the arm is about.
fn marker(child: &mut std::process::Child, role: &str) {
    let mut byte = [0u8; 1];
    let out = child.stdout.as_mut().expect("child stdout");
    let mut line = Vec::new();
    while out.read(&mut byte).expect("read the child's marker") == 1 {
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    assert_eq!(
        String::from_utf8_lossy(&line),
        format!("reached {role}"),
        "{role} never reached its state",
    );
}

/// A pipe write end this process can check afterwards: writing to it proves the
/// handle still names the pipe, and the read end says the bytes arrived.
fn probe() -> (toyos::Pipe, toyos::Pipe) {
    toyos::pipe_pair().expect("a pipe of our own")
}

/// Arm 1. The peer exited before the send.
fn a_dead_peer_keeps_nothing() {
    let (conn, mut child) = peer("connect-and-exit");
    assert!(child.wait().expect("wait the peer").success(), "the peer did not exit cleanly");

    let (read, write) = probe();
    let sending = syscall::dup(write.as_handle()).expect("a duplicate to send");
    assert_eq!(
        syscall::handle_send(conn.fd(), &[sending]),
        Err(SyscallError::Gone),
        "a connection whose peer has exited took a batch",
    );

    still_ours(sending, &read, b"gone");
    syscall::close(sending);
    println!("  dead peer: Gone, and the handle is still ours");
}

/// Arm 2. The peer is alive and has never received, so the queue fills.
///
/// `ResourceExhausted` is exactly what a slow or hostile client produces, and
/// the server that hits it is the one that would have lost a capability.
fn a_full_queue_keeps_nothing() {
    let (conn, mut child) = peer("connect-and-wait");
    marker(&mut child, "connect-and-wait");

    let (read, write) = probe();
    for i in 0..MAX_QUEUED_BATCHES {
        let handle = syscall::dup(write.as_handle()).expect("a duplicate to send");
        syscall::handle_send(conn.fd(), &[handle])
            .unwrap_or_else(|e| panic!("batch {i} of {MAX_QUEUED_BATCHES} was refused: {e:?}"));
    }
    let refused = syscall::dup(write.as_handle()).expect("one more duplicate");
    assert_eq!(
        syscall::handle_send(conn.fd(), &[refused]),
        Err(SyscallError::ResourceExhausted),
        "the {MAX_QUEUED_BATCHES}-batch queue took one more",
    );

    still_ours(refused, &read, b"full");
    syscall::close(refused);
    child.kill().expect("kill the waiting peer");
    let _ = child.wait();
    println!("  full queue: ResourceExhausted, and the handle is still ours");
}

/// The refused handle names what it always named. A `Stale` here would end this
/// process instead of failing the assertion, which is the failure mode the fix
/// exists to remove — so an exit 139 on this arm is the same finding.
fn still_ours(handle: RawHandle, read: &toyos::Pipe, mark: &[u8]) {
    syscall::write_nonblock(handle, mark)
        .expect("the refused handle no longer names the pipe it was made from");
    let mut buf = [0u8; 8];
    let n = read.read(&mut buf).expect("read our own pipe");
    assert_eq!(&buf[..n], mark, "the refused handle wrote somewhere else");
}

/// Arm 3. The batch is queued and the peer dies without receiving it.
fn an_unreceived_batch_is_released() {
    let before = Census::now();
    let (conn, mut child) = peer("connect-and-wait");
    marker(&mut child, "connect-and-wait");

    let region = toyos::shm::SharedMemory::create(4096).expect("a region to send");
    let sending = syscall::dup(region.as_handle()).expect("a duplicate to send");
    syscall::handle_send(conn.fd(), &[sending]).expect("the peer took the batch");
    drop(region);

    child.kill().expect("kill the peer holding the batch");
    let _ = child.wait();
    drop(conn);
    // **Dropped before the reading, not after it.** A `Stdio::piped()` child
    // leaves the parent holding the read end of its stdout for as long as the
    // `Child` is alive, so a census taken with one in scope counts a `PipeRead`
    // this arm created and blames the batch for it.
    drop(child);

    // The region and nothing else: this arm creates a process too, and a
    // `ProcessObject`'s release is the scheduler's rather than this test's.
    let after = Census::now();
    assert_eq!(
        after.kind("SharedMem"),
        before.kind("SharedMem"),
        "a batch its peer never received was not released: first {before}, then {after}",
    );
    println!("  unreceived: the queue gave the region back");
}

/// Arm 4. The peer sends and exits; the batch is still the receiver's.
fn a_senders_exit_does_not_retract_what_it_sent() {
    let (conn, mut child) = peer("send-and-exit");
    assert!(child.wait().expect("wait the sender").success(), "the sender did not exit cleanly");

    let mut batch = [RawHandle(0); MAX_TRANSFER_HANDLES];
    let n = conn.recv_handles(&mut batch).expect("receive the batch");
    assert_eq!(n, 1, "a batch a dead sender left behind arrived {n} wide");

    // **The handle resolves, which is the whole assertion.** The child made both
    // ends of the pipe and both went with it, so the write itself is refused for
    // the reader rather than for the handle — `NotFound`, which
    // `specs/issues/isolation/a-broken-pipe-answers-not-found.md` is about and
    // which `connect_before_serve` asserts too. A handle a dead sender's batch
    // no longer backed would instead end this process on `Stale`, so reaching
    // the next statement at all is the verdict.
    assert_eq!(
        syscall::write_nonblock(batch[0], b"still here"),
        Err(SyscallError::NotFound),
        "a handle its sender no longer holds answered something other than its dead reader",
    );
    syscall::close(batch[0]);
    println!("  sender exited: the batch was still there and still worked");
}

/// Arm 5. A `deferred` container releasing an `immediate` member.
///
/// `HandleQueue` holds arbitrary `HandleEntry`s and `ConnectionEnd` is a
/// `deferred` row, so its zero-handle hook drops whatever is queued. A `File`
/// is an `immediate` row, so its destructor runs *there*: `vfs::lock()`,
/// `flush_file`, the FAT32 adapter and a device round trip, wherever the drain
/// is running.
///
/// **Which stack that is, is not this test's to choose**, and that is the whole
/// hazard. The drain has three sites: two are a task's 128 KiB kernel stack and
/// the third is the idle loop's, which a killer on another CPU reaches without
/// anything here deciding it. So the arm asserts both halves separately — the
/// bytes on disk say the release path ran to the end, and the machine-wide
/// idle-stack high water says that stack has room for the deepest thing this
/// boot has run on it. With a guard page below, the alternative to a reading is
/// a halted machine and a test that reports nothing.
fn an_immediate_object_is_flushed_off_the_idle_stack() {
    let _ = std::fs::remove_file(DIRTY_PATH);
    let file = syscall::open(DIRTY_PATH.as_bytes(), OpenFlags::WRITE | OpenFlags::CREATE)
        .expect("create the file to send");
    syscall::write(file, DIRTY_BYTES).expect("write it without flushing");

    let (conn, mut child) = peer("connect-and-wait");
    marker(&mut child, "connect-and-wait");
    syscall::handle_send(conn.fd(), &[file]).expect("the peer took the file");
    child.kill().expect("kill the peer holding the file");
    let _ = child.wait();
    drop(conn);

    let on_disk = std::fs::read(DIRTY_PATH).expect("the file the release path flushed");
    assert_eq!(
        on_disk, DIRTY_BYTES,
        "a file released from the zero-handle drain was not flushed",
    );
    let _ = std::fs::remove_file(DIRTY_PATH);

    let used = syscall::debug(debug_action::IDLE_STACK_HIGH_WATER);
    let size = syscall::debug(debug_action::IDLE_STACK_SIZE);
    assert!(used > 0 && used < size, "the idle stack reading is {used} of {size}");
    assert!(
        used * 2 <= size,
        "the deepest the idle stack has been is {used} of {size} — a release path that \
         reaches the filesystem runs there, and it must leave the stack room to double",
    );
    println!("  immediate in a deferred: flushed, idle stack {used} of {size}");
}

fn child(role: &str) -> ! {
    let ns = endow::namespace().expect("the parent endowed a namespace");
    let conn = ns.open(SERVICE).expect("connect through the endowed connector");
    match role {
        "connect-and-exit" => syscall::exit(0),
        "connect-and-wait" => {
            say("reached connect-and-wait");
            // Nothing is ever received: every arm that uses this role is about
            // a batch the peer is still holding when it dies.
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
        "send-and-exit" => {
            let (_read, write) = toyos::pipe_pair().expect("a pipe of our own");
            let handle = syscall::dup(write.as_handle()).expect("a duplicate to send");
            syscall::handle_send(conn.fd(), &[handle]).expect("send the batch");
            syscall::exit(0)
        }
        other => panic!("unknown role {other:?}"),
    }
}

fn say(line: &str) {
    let mut out = std::io::stdout();
    out.write_all(line.as_bytes()).expect("say");
    out.write_all(b"\n").expect("say");
    out.flush().expect("flush");
}

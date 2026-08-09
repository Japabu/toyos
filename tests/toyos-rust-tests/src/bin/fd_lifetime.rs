//! What a handle holds is released when the *last* handle goes, and no sooner —
//! on `close` and on being killed alike.
//!
//! `Descriptor::clone` used to copy a `ListenerId` and a `RingId` as bare
//! numbers while `close` unregistered the service and destroyed the ring
//! unconditionally, so `dup` and then closing either fd took the object out
//! from under the survivor. A file was already refcounted; it is here because
//! the kinds are one property, and a test covering three of them says nothing
//! about the fourth.
//!
//! **The service half is now a port**, and its witness is better for it: there
//! is no name to ask about, so what says the acceptor is alive is that a client
//! connecting through the connector is accepted, and what says it is gone is
//! that the next open answers [`SyscallError::Gone`] — the kernel's own record
//! of a server that has left, rather than a name nobody re-took.
//!
//! The kill half is why this is a guest test and not a host one. This kernel
//! does not unwind, so a `Drop` reached only by an orderly `close` would be
//! decoration: `kill` runs on another CPU and drains the victim's descriptor
//! table itself, and that is the path each case below re-checks.
//!
//! Roles: no argument is the test; `holder <kind>` takes one object, reports
//! what it can about it, and waits to be killed.

use std::io::{BufRead, BufReader, Write};
use std::os::toyos::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};

use toyos::{namespace, port, AsHandle};
use toyos_abi::syscall::{self, OpenFlags, SeekFrom, SyscallError, SERVE_PREFIX};

const SELF_PATH: &str = "/bin/test_rs_fd_lifetime";
/// The name this test's own namespaces map to the port under test. Private to
/// this process and its children, which is the whole of what a namespace is.
const SERVICE: &str = "fd-lifetime-service";
const PATH: &[u8] = b"/tmp/fd-lifetime.txt";
const KILLED_PATH: &[u8] = b"/home/fd-lifetime-killed.txt";
const PAYLOAD: &[u8] = b"a file outlives the fd that was closed first";
const KILLED_PAYLOAD: &[u8] = b"written by a process that was killed before it could close";

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("holder") => holder(&args.next().expect("holder needs a kind")),
        Some(other) => panic!("unknown role {other:?}"),
        None => test(),
    }
}

fn test() {
    file_survives_one_close();
    acceptor_survives_one_close();
    ring_survives_one_close();

    kill_releases_acceptor();
    kill_releases_ring();
    kill_flushes_file();

    println!("file, acceptor and ring each outlive the first close and are released by kill");
}

fn file_survives_one_close() {
    let a = syscall::open(PATH, OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE)
        .expect("create the file");
    let b = syscall::dup(a).expect("dup a file fd");
    syscall::write(b, PAYLOAD).expect("write through the dup");
    syscall::close(a);

    // Reading through the survivor is what says the first close did not take
    // the file's cache entry with it.
    syscall::seek(b, SeekFrom::Start(0)).expect("seek on the surviving fd");
    let mut buf = [0u8; 128];
    let n = syscall::read(b, &mut buf).expect("read through the surviving fd");
    assert_eq!(&buf[..n], PAYLOAD, "the surviving fd no longer names the file");
    syscall::close(b);
}

fn acceptor_survives_one_close() {
    let (acceptor, connector) = port::create().expect("a port of our own");
    let ns = namespace::build().add(SERVICE, &connector).finish().expect("a namespace for it");

    let a = acceptor.into_raw();
    let b = syscall::dup(a).expect("dup an acceptor handle");
    syscall::close(a);

    // The survivor still serves: a client's connection is queued on the port
    // and this handle takes it.
    let client = syscall::namespace_open(ns.as_handle(), SERVICE)
        .expect("open through the connector of a live port");
    let accepted = syscall::accept(b).expect("accept on the surviving acceptor handle");
    syscall::close(accepted.fd);
    syscall::close(client);

    // And the last close is what closes the port. `Gone` and not `NotFound`:
    // the name is still in the namespace, and it is the server that has left.
    syscall::close(b);
    assert_eq!(
        syscall::namespace_open(ns.as_handle(), SERVICE).err(),
        Some(SyscallError::Gone),
        "the last close of an acceptor did not close the port"
    );
}

fn ring_survives_one_close() {
    let (a, token) = syscall::io_uring_setup(8).expect("io_uring_setup");
    let b = syscall::dup(a).expect("dup a ring fd");
    syscall::close(a);

    // Two independent witnesses that the instance is alive: its pages are
    // still ours to map, and it still accepts an `enter`.
    unsafe { syscall::try_map_shared(token) }
        .expect("closing one of two ring fds freed the ring's pages");
    syscall::io_uring_enter(b, 0, 0, 0)
        .expect("closing one of two ring fds destroyed the instance");

    syscall::close(b);
    assert_eq!(
        unsafe { syscall::try_map_shared(token) }.err(),
        Some(SyscallError::NotFound),
        "the last close did not free the ring"
    );
}

/// The acceptor is *endowed* to the holder, which is the only way one changes
/// hands — so this process keeps the connector and watches the port from the
/// client's side while the holder is killed.
fn kill_releases_acceptor() {
    let (acceptor, connector) = port::create().expect("a port for the holder");
    let ns = namespace::build().add(SERVICE, &connector).finish().expect("a namespace for it");
    assert!(
        syscall::namespace_open(ns.as_handle(), SERVICE).is_ok(),
        "the port was not live before its holder was killed"
    );

    let mut child = spawn_holder_endowed("acceptor", &acceptor.into_raw()).0;
    kill_and_reap(&mut child);

    assert_eq!(
        syscall::namespace_open(ns.as_handle(), SERVICE).err(),
        Some(SyscallError::Gone),
        "a killed process did not give its acceptor back"
    );
}

/// The ring's release is visible through its shared-memory token: the region
/// is not granted to us, so a live one answers `PermissionDenied` and a freed
/// one `NotFound`. Read before anything else can allocate, because tokens are
/// reusable.
fn kill_releases_ring() {
    let (mut child, mut out) = spawn_holder("ring");
    let mut line = String::new();
    out.read_line(&mut line).expect("holder's ring token");
    let token: u32 = line.trim().parse().expect("holder's ring token");
    assert_eq!(
        unsafe { syscall::try_map_shared(token) }.err(),
        Some(SyscallError::PermissionDenied),
        "the ring's region was already gone while its holder still ran"
    );

    kill_and_reap(&mut child);
    assert_eq!(
        unsafe { syscall::try_map_shared(token) }.err(),
        Some(SyscallError::NotFound),
        "a killed process did not give its io_uring back"
    );
}

/// A killed process's dirty file must still reach the filesystem: that flush
/// used to be a hand-written arm of `close_all`, and is now the descriptor's
/// own drop on the same teardown path.
fn kill_flushes_file() {
    let (mut child, _) = spawn_holder("file");
    kill_and_reap(&mut child);

    let fd = syscall::open(KILLED_PATH, OpenFlags::READ)
        .expect("the killed holder's file does not exist");
    let mut buf = [0u8; 128];
    let n = syscall::read(fd, &mut buf).expect("read the killed holder's file");
    syscall::close(fd);
    assert_eq!(
        &buf[..n],
        KILLED_PAYLOAD,
        "a killed process's unclosed file was not written back"
    );
}

fn spawn_holder(kind: &str) -> (Child, BufReader<ChildStdout>) {
    spawn_with(kind, Command::new(SELF_PATH))
}

/// The same, with an acceptor moved into the child under the label
/// `endow::acceptor` looks up.
fn spawn_holder_endowed(kind: &str, acceptor: &toyos_abi::RawHandle) -> (Child, BufReader<ChildStdout>) {
    let mut command = Command::new(SELF_PATH);
    command.endow(&format!("{SERVE_PREFIX}{SERVICE}"), acceptor.0);
    spawn_with(kind, command)
}

fn spawn_with(kind: &str, mut command: Command) -> (Child, BufReader<ChildStdout>) {
    let mut child = command
        .arg("holder")
        .arg(kind)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn holder");
    let mut out = BufReader::new(child.stdout.take().expect("holder stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("holder ready line");
    assert_eq!(line.trim(), "held", "the {kind} holder did not report: {line:?}");
    (child, out)
}

/// `Child::kill` is unimplemented in the ToyOS std, so this is the syscall.
fn kill_and_reap(child: &mut Child) {
    syscall::kill(toyos_abi::Pid(child.id())).expect("kill the holder");
    child.wait().expect("reap the holder");
}

fn holder(kind: &str) {
    match kind {
        "acceptor" => {
            let acceptor =
                toyos::endow::acceptor(SERVICE).expect("holder: the acceptor it was endowed");
            // Held for the process's life. Nothing accepts from it: the point
            // is what the *kill* does to it.
            core::mem::forget(acceptor);
            println!("held");
        }
        "ring" => {
            let (_fd, token) = syscall::io_uring_setup(8).expect("holder: io_uring_setup");
            println!("held");
            println!("{token}");
        }
        "file" => {
            let fd = syscall::open(
                KILLED_PATH,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
            )
            .expect("holder: open");
            syscall::write(fd, KILLED_PAYLOAD).expect("holder: write");
            println!("held");
        }
        other => panic!("holder: unknown kind {other:?}"),
    }
    std::io::stdout().flush().expect("holder: flush");
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

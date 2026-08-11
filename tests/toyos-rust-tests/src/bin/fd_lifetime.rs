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
use toyos_abi::io_uring::IoUringParams;
use toyos_abi::syscall::{self, OpenFlags, SeekFrom, SyscallError, SERVE_PREFIX};

const SELF_PATH: &str = "/bin/test_rs_fd_lifetime";
/// The name this test's own namespaces map to the port under test. Private to
/// this process and its children, which is the whole of what a namespace is.
const SERVICE: &str = "fd-lifetime-service";
const PATH: &[u8] = b"/tmp/fd-lifetime.txt";
const KILLED_PATH: &[u8] = b"/home/fd-lifetime-killed.txt";
const PAYLOAD: &[u8] = b"a file outlives the fd that was closed first";
const KILLED_PAYLOAD: &[u8] = b"written by a process that was killed before it could close";
/// `process::HANDLE_FAULT_EXIT_CODE`.
const HANDLE_FAULT: i32 = 139;
/// How many rings the `ring` holder makes. One is 2 MiB, and the witness for
/// a killed process giving them back is the machine's own free memory — so the
/// figure has to be far enough above what the rest of a boot moves under it
/// that a reclaim cannot hide in the noise.
const HOLDER_RINGS: usize = 8;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("holder") => holder(&args.next().expect("holder needs a kind")),
        Some("closed-ring") => closed_ring(),
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
    syscall::close(accepted);
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
    let (a, base) = unsafe { syscall::io_uring_setup(8) }.expect("io_uring_setup");
    let b = syscall::dup(a).expect("dup a ring handle");
    syscall::close(a);

    // Two independent witnesses that the instance is alive: it still accepts an
    // `enter`, and its own page is still mapped. The page is the ring's now —
    // there is no separate region and no token naming one — so reading the
    // params back through the pointer setup handed over is what says the close
    // did not unmap it.
    syscall::io_uring_enter(b, 0, 0, 0)
        .expect("closing one of two ring handles destroyed the instance");
    let params = unsafe { core::ptr::read_volatile(base as *const IoUringParams) };
    assert_eq!(params.sq_ring_size, 8, "the ring's page no longer describes the ring");

    syscall::close(b);

    // **And the last close leaves no handle behind, which is now a fact about
    // the caller rather than a word it is handed.** Naming a slot a process
    // closed is a bug in that process, so the kernel ends it — which is why
    // this is a child and not a fourth line here.
    let probe = Command::new(SELF_PATH)
        .arg("closed-ring")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the closed-ring probe");
    let out = probe.wait_with_output().expect("wait the closed-ring probe");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "closed",
        "the closed-ring probe never reached its call",
    );
    assert_eq!(
        out.status.code(),
        Some(HANDLE_FAULT),
        "the last close left the ring's handle behind",
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

/// A ring's pages are its own and no second name reaches them, so the witness
/// is the machine's free memory rather than a token this process could try to
/// map. The holder makes [`HOLDER_RINGS`] of them, which is 16 MiB.
fn kill_releases_ring() {
    let before = free_bytes();
    let (mut child, _) = spawn_holder("ring");
    let held = free_bytes();

    // Non-vacuity: an instrument that cannot see 16 MiB leave cannot see it
    // come back either, and the reclaim assertion would pass on a kernel that
    // frees nothing.
    let taken = before.saturating_sub(held);
    assert!(
        taken >= 12 * 1024 * 1024,
        "the holder made {HOLDER_RINGS} rings and free memory only moved {taken} bytes"
    );

    kill_and_reap(&mut child);
    let leaked = before.saturating_sub(free_bytes());
    assert!(
        leaked < 6 * 1024 * 1024,
        "a killed process kept {leaked} bytes of its io_urings"
    );
}

fn free_bytes() -> u64 {
    let mut buf = [0u8; 48];
    let n = syscall::sysinfo(&mut buf);
    assert!(n >= 48, "sysinfo returned {n} bytes");
    let total = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let used = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    total - used
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

fn kill_and_reap(child: &mut Child) {
    child.kill().expect("kill the holder");
    child.wait().expect("reap the holder");
}

/// Both handles to one ring, both closed, and then the number presented again.
fn closed_ring() -> ! {
    let (a, _base) = unsafe { syscall::io_uring_setup(8) }.expect("closed-ring: io_uring_setup");
    let b = syscall::dup(a).expect("closed-ring: dup");
    syscall::close(a);
    syscall::close(b);
    println!("closed");
    std::io::stdout().flush().expect("closed-ring: flush");
    let answered = syscall::io_uring_enter(b, 0, 0, 0);
    panic!("a ring handle closed twice over answered {answered:?}");
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
            let rings: Vec<_> = (0..HOLDER_RINGS)
                .map(|_| unsafe { syscall::io_uring_setup(8) }.expect("holder: io_uring_setup"))
                .collect();
            // Held for the process's life: the point is what the kill does.
            core::mem::forget(rings);
            println!("held");
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

//! Which handle failures end the caller, and which ones it is allowed to see.
//!
//! **Three of the five are bugs in the process that named the handle.** A
//! handle is a local name a process was given, so naming one it does not hold
//! (`BadHandle`), one it closed (`Stale`), or asking a pipe to accept a
//! connection (`WrongType`) is not something a correct program can do — and a
//! word it can ignore lets the bug survive. The other two are not: a process
//! may legitimately hold an attenuated handle and probe what it can do with it,
//! and a table with no room is a resource limit.
//!
//! So the policy is a matrix and this is its gate. Each fatal kind is raised in
//! a child, which prints its marker, makes the one call and must never print
//! again; each survivable kind is raised here, where the answer is a word and
//! the process carries on to raise the next.
//!
//! **The marker is what gives the fatal arms teeth.** Without it a child that
//! died before reaching the call would pass, and the arm would assert nothing.
//! With it, a tree that put the three error words back reds on the exit code
//! while still printing the marker, and a tree that killed for all five reds on
//! the two survivable arms.
//!
//! The last arm is the census. `handle_count` reaching zero is what releases an
//! object, and a kill is the path where nothing unwinds — so a leak per killed
//! process is exactly the defect this policy could introduce and the only place
//! it is visible is the kernel's own live-object count.

use std::io::Write;
use std::process::{Command, Stdio};

use toyos::census::Census;
use toyos::AsHandle;
use toyos_abi::handle::Rights;
use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::RawHandle;

const SELF_PATH: &str = "/bin/test_rs_handle_kill_policy";

/// `process::HANDLE_FAULT_EXIT_CODE`. The shell convention for "died on
/// SIGSEGV", which is the same class of mistake with a pointer instead of a
/// handle.
const HANDLE_FAULT: i32 = 139;

/// A slot no process in this tree reaches. `RawHandle::MAX_SLOTS` is 4096 and a
/// process holding 3000 handles would be a different bug.
const UNHELD_SLOT: u32 = 3000;

/// How many kill-and-close rounds each census sample is taken over. Large
/// enough that one leaked object per round is a number no drain lag can hide.
const CHURN_ROUNDS: usize = 16;

/// The three kinds that end the caller. Each is a role this binary runs as, and
/// the description is what the kernel is being asked to refuse.
const FATAL: &[(&str, &str)] = &[
    ("bad-handle", "a slot this process never held"),
    ("stale", "a slot this process closed"),
    ("wrong-type", "a pipe where the call takes an acceptor"),
    ("faulting-thread", "a bad handle named by a thread that is not the main one"),
];

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some(role) => fatal_role(role),
        None => test(),
    }
}

fn test() {
    for (role, what) in FATAL {
        let child = Command::new(SELF_PATH)
            .arg(role)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {role}: {e}"));
        let out = child.wait_with_output().unwrap_or_else(|e| panic!("wait {role}: {e}"));
        let said = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            said.trim(),
            format!("reached {role}"),
            "{role} ({what}) never reached its call, or answered past it",
        );
        assert_eq!(
            out.status.code(),
            Some(HANDLE_FAULT),
            "{role} ({what}) did not end the caller",
        );
        println!("  {role}: killed at the call, exit {HANDLE_FAULT}");
    }

    rights_are_a_word();
    a_full_table_is_a_word();
    the_kills_release_what_they_held();

    println!("three handle failures end the caller, two answer it, and neither leaks");
}

/// A right the handle does not carry is refused and the process carries on.
///
/// It has to be an answer for ever: rights only shrink, so a program that
/// narrowed a handle and then asked what it can still do is doing the one thing
/// attenuation is for.
fn rights_are_a_word() {
    let (read, write) = toyos::pipe_pair().expect("a pipe of our own");
    let blind = syscall::dup_narrowed(write.as_handle(), Rights::NONE)
        .expect("a handle carrying nothing is still a handle");
    assert_eq!(
        syscall::write_nonblock(blind, b"denied"),
        Err(SyscallError::PermissionDenied),
        "a handle with no rights took a write",
    );
    syscall::close(blind);
    // The unnarrowed handle still works, so the refusal was the rights and not
    // the pipe.
    write.write(b"allowed").expect("the full handle writes");
    let mut buf = [0u8; 8];
    let n = read.read(&mut buf).expect("read our own pipe");
    assert_eq!(&buf[..n], b"allowed", "the pipe did not carry its own bytes");
    println!("  rights: PermissionDenied, and the process is still here");
}

/// A table with no room is a resource limit, and the caller is told.
///
/// **In a child, because filling a table is not something a process comes back
/// from.** `dup2` names the slot, so filling every one of them displaces
/// whatever was there — this process's namespace among them — and the SDK holds
/// that handle for the life of the process. The child's own exit 0 is the
/// assertion that the refusal was survivable.
fn a_full_table_is_a_word() {
    let child = Command::new(SELF_PATH)
        .arg("fill-the-table")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the table-filling child");
    let out = child.wait_with_output().expect("wait the table-filling child");
    let said = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "the table cap was not survivable: {said}");
    assert!(
        said.contains("ResourceExhausted at slot"),
        "the table-filling child said {said:?}",
    );
    println!("  full table: {}", said.trim());
}

/// A killed process gives back every object it held.
///
/// **The one defect the flip could introduce, and the only instrument for it.**
/// Nothing unwinds on a kill, so an object whose release rode a `Drop` on the
/// dying thread's stack would be leaked once per handle fault — invisible from
/// userland, and invisible in the kernel too except as a live-object count that
/// no longer comes back down.
///
/// Two samples rather than one against a baseline: an object released by a
/// child is dropped from the deferred queue on whichever CPU drains next, so a
/// single reading can be high by whatever has not drained yet. A leak is not a
/// lag — it accumulates — so no *kind* may be higher after the second round of
/// rounds than after the first. Per kind and not in total, because a total
/// hides a leak of one kind behind churn in another.
fn the_kills_release_what_they_held() {
    let after_first = churn(CHURN_ROUNDS);
    let after_second = churn(CHURN_ROUNDS);
    let grown: Vec<_> = after_second.grown_since(&after_first).collect();
    assert!(
        grown.is_empty(),
        "{CHURN_ROUNDS} more killed processes left more live objects behind: \
         {grown:?} — first {after_first}, then {after_second}",
    );
    println!("  census: {} live objects, then {}", after_first.total(), after_second.total());
}

/// `CHURN_ROUNDS` processes that each hold a pipe and a region and then die on a
/// bad handle, and what the kernel holds when they are all gone.
fn churn(rounds: usize) -> Census {
    for _ in 0..rounds {
        let status = Command::new(SELF_PATH)
            .arg("holder")
            .stdout(Stdio::null())
            .status()
            .expect("spawn a holder");
        assert_eq!(status.code(), Some(HANDLE_FAULT), "a holder did not die on its bad handle");
    }
    Census::now()
}

/// Fill every slot and require the refusal to be a word. Exits 0, which is the
/// other half of what the parent asserts.
fn fill_the_table() -> ! {
    let mut refused = None;
    for slot in 3..RawHandle::MAX_SLOTS as u16 + 1 {
        if let Err(e) = syscall::dup2(RawHandle(1), slot) {
            refused = Some((slot, e));
            break;
        }
    }
    let (slot, e) = refused.expect("filling the table must eventually be refused");
    assert_eq!(e, SyscallError::ResourceExhausted, "wrong word at the table cap");
    // Slot 2 is stderr and untouched by the loop above, so this reaches the
    // host whatever became of slot 1.
    let line = format!("ResourceExhausted at slot {slot}, and the process is still here\n");
    syscall::write(RawHandle(1), line.as_bytes()).expect("say so through the filled slot");
    syscall::exit(0)
}

fn fatal_role(role: &str) -> ! {
    if role == "fill-the-table" {
        fill_the_table();
    }
    // Printed before the call and flushed, so the parent can tell "the kernel
    // ended it here" from "it never got here".
    println!("reached {role}");
    std::io::stdout().flush().expect("flush the marker");
    match role {
        "bad-handle" => {
            let mut buf = [0u8; 8];
            let n = syscall::read_nonblock(RawHandle(UNHELD_SLOT), &mut buf);
            panic!("a slot this process never held answered {n:?}");
        }
        "stale" => {
            let (read, _write) = toyos::pipe_pair().expect("a pipe to close");
            let closed = read.as_handle();
            drop(read);
            let mut buf = [0u8; 8];
            let n = syscall::read_nonblock(closed, &mut buf);
            panic!("a handle this process closed answered {n:?}");
        }
        // The *read* end, because `SYS_ACCEPT` checks `Rights::READ` before it
        // looks at the type: presenting the write end would be refused for the
        // right it lacks and would never reach the question this arm asks.
        "wrong-type" => {
            let (read, _write) = toyos::pipe_pair().expect("a pipe to mistype");
            let taken = syscall::accept(read.as_handle());
            panic!("a pipe accepted a connection: {taken:?}");
        }
        // The kill is the process's, not the thread's: a handle fault raised on
        // any thread ends every thread. Asserted from the exit code, which the
        // main thread never reaches to set.
        "faulting-thread" => {
            std::thread::spawn(|| {
                let mut buf = [0u8; 8];
                let n = syscall::read_nonblock(RawHandle(UNHELD_SLOT), &mut buf);
                panic!("a slot no thread of this process held answered {n:?}");
            });
            std::thread::sleep(std::time::Duration::from_secs(10));
            panic!("a thread's handle fault left the process running");
        }
        // Holds two objects the kernel has to give back, then dies where
        // nothing unwinds.
        "holder" => {
            let (_read, _write) = toyos::pipe_pair().expect("a pipe to leak");
            let _region = toyos::shm::SharedMemory::create(4096).expect("a region to leak");
            let mut buf = [0u8; 8];
            let n = syscall::read_nonblock(RawHandle(UNHELD_SLOT), &mut buf);
            panic!("a holder's bad handle answered {n:?}");
        }
        other => panic!("unknown role {other:?}"),
    }
}

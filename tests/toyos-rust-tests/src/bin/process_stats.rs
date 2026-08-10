//! `SYS_PROCESS_STATS`, which is now a question about an object rather than a
//! claim on a snapshot.
//!
//! Three things changed with the handle and all three are asserted here: a
//! *live* process answers, an exited one keeps answering, and answering twice
//! gives the same numbers. The old shape — a snapshot the parent could read
//! exactly once, only after the child died — is what the third case used to
//! assert the opposite of.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::toyos::process::ChildExt;
use std::process::{Command, Stdio};
use toyos::process::Process;
use toyos_abi::handle::Rights;
use toyos_abi::syscall::{self, ProcessStats, SyscallError};

const SELF_PATH: &str = "/bin/test_rs_process_stats";

fn main() {
    if std::env::args().nth(1).as_deref() == Some("held") {
        return held();
    }
    exited_child();
    live_process();
    repeatable();
    refused_without_read();
    println!("all process_stats tests passed");
}

/// Says it is running, then blocks until it is killed. The marker is flushed, so
/// a parent that has read it knows this process has been scheduled and has
/// faulted its own image in.
fn held() {
    println!("running");
    std::io::stdout().flush().expect("held: flush the marker");
    let mut buf = [0u8; 1];
    let _ = std::io::stdin().read(&mut buf);
}

/// std hands back the handle rather than wrapping the call: `ProcessStats` is
/// `toyos-abi`'s type, and a std signature naming it would bind every caller to
/// the sysroot's copy of that crate instead of its own.
fn stats_of(child: &std::process::Child) -> Result<ProcessStats, SyscallError> {
    let mut stats = ProcessStats::default();
    syscall::process_stats(toyos_abi::RawHandle(child.as_raw_handle()), &mut stats)?;
    Ok(stats)
}

fn exited_child() {
    let mut child = Command::new("/bin/echo").arg("hello").spawn().expect("spawn echo");
    let status = child.wait().expect("wait");
    assert!(status.success());

    let s = stats_of(&child).expect("an exited child still answers, because the object holds it");

    assert!(s.pid > 0, "pid should be > 0, got {}", s.pid);
    assert!(s.wall_ns > 0, "wall_ns should be > 0, got {}", s.wall_ns);
    assert!(s.cpu_ns > 0, "cpu_ns should be > 0, got {}", s.cpu_ns);
    assert!(s.syscall_total > 0, "syscall_total should be > 0, got {}", s.syscall_total);
    assert!(
        s.fault_demand_count > 0 || s.fault_zero_count > 0,
        "should have at least one fault, got demand={} zero={}",
        s.fault_demand_count,
        s.fault_zero_count
    );
    assert!(s.peak_memory > 0, "peak_memory should be > 0, got {}", s.peak_memory);

    println!(
        "  exited child: ok (pid={} wall={}ns cpu={}ns syscalls={} faults={} peak={})",
        s.pid,
        s.wall_ns,
        s.cpu_ns,
        s.syscall_total,
        s.fault_demand_count + s.fault_zero_count,
        s.peak_memory
    );
}

/// The whole of what the handle bought: a target that has not exited.
fn live_process() {
    // **A line out of the child, not a bare spawn.** `spawn` returns before the
    // child has been scheduled, so a sample taken there reads a process that has
    // faulted nothing and the assertions below become a race against the
    // scheduler. A role of this binary rather than `/bin/cat`, because what is
    // needed is a *flushed* line from something still running, and a filter's
    // buffering is not this test's to depend on.
    let mut child = Command::new(SELF_PATH)
        .arg("held")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the held child");
    let mut out = BufReader::new(child.stdout.take().expect("held stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("the held child's marker");
    assert_eq!(line.trim(), "running", "the held child said {line:?}");

    let s = stats_of(&child).expect("a live process answers");
    assert!(s.wall_ns > 0, "a running process has spent wall time");
    assert!(
        s.fault_demand_count > 0 || s.fault_zero_count > 0,
        "a running process has faulted its own image in"
    );
    println!("  live process: ok (pid={} wall={}ns)", s.pid, s.wall_ns);
    child.kill().expect("kill the held child");
    child.wait().expect("wait the held child");
}

/// Reading does not spend it. This asserted the opposite before the handle:
/// the snapshot lived on the parent and the read deleted it.
fn repeatable() {
    let mut child = Command::new("/bin/echo").arg("once").spawn().expect("spawn");
    child.wait().expect("wait");

    let first = stats_of(&child).expect("first read");
    let second = stats_of(&child).expect("second read: the numbers are the object's");
    assert_eq!(first.pid, second.pid, "two reads named two processes");
    assert_eq!(first.wall_ns, second.wall_ns, "a finished process's wall time moved");
    assert_eq!(first.syscall_total, second.syscall_total, "a finished process made a syscall");
    println!("  repeatable: ok");
}

/// The right is the gate, and a handle without it is refused rather than
/// answered.
fn refused_without_read() {
    let mut child = Command::new("/bin/echo").arg("rights").spawn().expect("spawn");
    child.wait().expect("wait");

    let full = toyos_abi::RawHandle(child.into_raw_handle());
    let blind = syscall::dup_narrowed(full, Rights::WAIT).expect("narrow to WAIT alone");
    let mut stats = ProcessStats::default();
    let refused = syscall::process_stats(blind, &mut stats);
    assert_eq!(
        refused,
        Err(SyscallError::PermissionDenied),
        "a Process handle without READ answered its accounting"
    );

    // SAFETY: both handles are this process's and nothing else answers for
    // them; wrapping them is what closes them.
    drop(unsafe { Process::from_raw(full) });
    drop(unsafe { Process::from_raw(blind) });
    println!("  refused without READ: ok");
}

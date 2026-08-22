//! A process is something you hold, and holding it is the whole of the right to
//! wait for it.
//!
//! **There is no zombie here and no parent.** A pid-keyed wait needed the
//! process table to keep a corpse until somebody claimed it, and rules for who
//! was allowed to claim one and what happened when nobody did. The exit code
//! lives on the object instead, published once by whichever of exit, kill or
//! panic recovery owns the teardown — so a wait after the fact reads a value, a
//! wait before it parks and is woken by the publish, and two holders both get
//! the answer.
//!
//! Each arm below is one sentence of that paragraph, and three of them assert
//! the *opposite* of what the pid-keyed shape did: reading the code does not
//! spend it, a process that never started the child can still wait for it, and
//! a pid on its own reaches nothing at all.
//!
//! One arm is about the wait rather than the shape.
//! `an_unrelated_wake_does_not_end_the_wait` provokes a wake that is not this
//! child's exit while the wait is parked on it, which used to return the wait
//! and panic the kernel on the exit code that was not there yet.
//!
//! Two roles besides the test. `held` exits with a code of the parent's
//! choosing, but not until its stdin closes — which is what lets every arm here
//! order the exit against the wait without a clock deciding anything. `waiter`
//! is the process that was endowed a handle to somebody else's child.

use std::io::Read;
use std::os::toyos::process::{ChildExt, CommandExt};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use toyos::endow::{Endowments, SYSCAP_LABEL};
use toyos::AsHandle;
use toyos::process::Process;
use toyos::syscap::SysCap;
use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::RawHandle;

const SELF_PATH: &str = "/bin/test_rs_process_lifecycle";

/// The label the `waiter` role finds its subject under. A local name in one
/// process's own table, and it names nothing anywhere else.
const SUBJECT_LABEL: &str = "subject";

/// `process::KILLED_EXIT_CODE`.
const KILLED: i32 = 137;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("held") => held(),
        Some("waiter") => waiter(),
        Some(other) => panic!("unknown role {other:?}"),
        None => test(),
    }
}

fn test() {
    reading_the_code_does_not_spend_it();
    a_wait_before_the_exit_is_woken_by_it();
    an_unrelated_wake_does_not_end_the_wait();
    two_handles_answer_the_same();
    a_kill_publishes_like_an_exit();
    a_handle_is_the_whole_of_the_right();
    a_pid_is_not_authority();
    println!("a process is a handle: the code is read, not claimed, and a pid grants nothing");
}

/// Wait, wait again, and ask a third time without blocking. The pid-keyed shape
/// answered the first and lost the process; this reads a value off an object
/// three times.
fn reading_the_code_does_not_spend_it() {
    let (mut child, release) = start(3);
    drop(release);
    assert_eq!(child.wait().expect("wait").code(), Some(3), "the first wait");

    let handle = RawHandle(child.as_raw_handle());
    assert_eq!(syscall::process_wait(handle), Ok(3), "the second wait");
    assert_eq!(syscall::process_wait_nonblock(handle), Ok(3), "a wait long after the fact");
    println!("  the code is read three times and is the same each time");
}

/// The park half. The child cannot exit while this process holds its stdin, so
/// the `WouldBlock` is a fact rather than a race — and the `wait` that follows
/// has nothing to return until another thread lets it go.
fn a_wait_before_the_exit_is_woken_by_it() {
    let (mut child, release) = start(7);
    let handle = RawHandle(child.as_raw_handle());
    assert_eq!(
        syscall::process_wait_nonblock(handle),
        Err(SyscallError::WouldBlock),
        "a process that cannot have exited reported an exit code",
    );

    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        drop(release);
    });
    assert_eq!(child.wait().expect("wait").code(), Some(7), "the woken wait");
    releaser.join().expect("the releasing thread");
    println!("  a wait taken before the exit is woken by it");
}

/// **The kernel-crash gate.** A wait is a wait *for a condition*, and a wake
/// that arrives for some other reason must not end it.
///
/// The other reason is ordinary and needs nothing staged: a thread's exit wakes
/// its process's main thread *by name* (`process::thread_exit`), whatever that
/// thread is waiting for. Before the park rechecked its own predicate and
/// re-parked, the wake was the answer — `sys_process_wait` read an exit code
/// that had not been published and the kernel panicked on `expect`, from a
/// plain userland `Child::wait()`. So this arm makes that wake land, on
/// purpose, in the middle of a wait whose condition is provably false.
///
/// The two handshakes are what make it land rather than merely be likely, and
/// both read a state the kernel publishes for `ps` and nothing else can
/// announce: the poker does not exit until the kernel says this process's main
/// thread is parked, and the releaser does not let the child go until the
/// kernel says a thread of this process has reached its own zombie mark — which
/// `release_thread` writes under the table lock immediately before the wake it
/// then posts. Until that release the child is blocked reading a pipe this
/// process holds the only write end of, so it cannot have exited: the condition
/// the wait holds for is false at the instant the unrelated wake arrives.
fn an_unrelated_wake_does_not_end_the_wait() {
    static POKED: AtomicBool = AtomicBool::new(false);
    static RELEASED: AtomicBool = AtomicBool::new(false);

    let (mut child, release) = start(5);
    let poker = std::thread::spawn(|| {
        await_true("the main thread never parked", main_thread_is_parked);
        POKED.store(true, Ordering::Release);
        // Returning is the poke: nothing else in this closure matters, because
        // `thread_exit` is what wakes the main thread.
    });
    let releaser = std::thread::spawn(move || {
        await_true("the poking thread never exited", a_thread_of_mine_has_exited);
        RELEASED.store(true, Ordering::Release);
        drop(release);
    });

    assert_eq!(child.wait().expect("wait").code(), Some(5), "the poked wait");
    assert!(POKED.load(Ordering::Acquire), "the poking thread never ran: nothing was proved");
    assert!(
        RELEASED.load(Ordering::Acquire),
        "the wait answered before the child could exit — an unrelated wake ended it",
    );
    poker.join().expect("the poking thread");
    releaser.join().expect("the releasing thread");
    println!("  a wake meant for something else does not end a wait");
}

/// Poll until `cond` holds. The bound is a hang guard and not a timing
/// assumption: both callers wait for a state the kernel has already decided and
/// reaches in microseconds, and the `sysinfo` call inside `cond` is the loop's
/// preemption point (`thread::yield_now` is a spin hint on this platform).
fn await_true(what: &str, cond: fn() -> bool) {
    let give_up = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < give_up, "{what}");
    }
}

/// `sched::payload::SCHED_BLOCKED` — the state column `ps` prints.
const BLOCKED: u8 = 2;

/// `SCHED_UNKNOWN`, which `sys_sysinfo` also answers for a thread whose entry
/// is a zombie. A live thread's scheduler record is installed under the same
/// table lock that inserts its entry, so a thread of ours reading this has
/// exited and nothing else.
const ZOMBIE: u8 = 3;

fn main_thread_is_parked() -> bool {
    my_threads().iter().any(|&(is_thread, state)| !is_thread && state == BLOCKED)
}

fn a_thread_of_mine_has_exited() -> bool {
    my_threads().iter().any(|&(is_thread, state)| is_thread && state == ZOMBIE)
}

/// The estate's system capability, taken once.
///
/// **Once, because taking is a swap**: a second `take` of the same label finds
/// `HANDLE_INVALID` and answers `None`, and two arms here want the same cap —
/// one for the `MANAGE` refusal, one for the roster below.
fn cap() -> &'static SysCap {
    static CAP: OnceLock<SysCap> = OnceLock::new();
    CAP.get_or_init(|| {
        Endowments::get()
            .take(SYSCAP_LABEL)
            .expect("test-runner endows every binary it spawns a system capability")
    })
}

/// This process's threads as the kernel publishes them: `(is a child thread,
/// scheduler state)`.
///
/// Even one's own threads arrive in the machine-wide roster, which is
/// `Rights::ROSTER` on a `SysCap` — there is no narrower question in the ABI,
/// and `tests/testcases` names `roster` on the test-runner row for this.
fn my_threads() -> Vec<(bool, u8)> {
    const HEADER: usize = toyos::system::SYSINFO_HEADER_SIZE;
    const ENTRY: usize = toyos::system::SYSINFO_ENTRY_SIZE;
    let mut buf = vec![0u8; HEADER + ENTRY * 256];
    let n = cap().roster(&mut buf);
    assert!((HEADER..=buf.len()).contains(&n), "sysinfo answered {n}");
    let me = syscall::getpid().raw();
    (HEADER..)
        .step_by(ENTRY)
        .take_while(|pos| pos + ENTRY <= n)
        .filter(|&pos| u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) == me)
        .map(|pos| (buf[pos + 9] != 0, buf[pos + 8]))
        .collect()
}

/// A second handle is a second name for one object, and the object is where the
/// code is.
fn two_handles_answer_the_same() {
    let (mut child, release) = start(11);
    let first = RawHandle(child.as_raw_handle());
    let second = syscall::dup(first).expect("a Process handle duplicates");
    drop(release);
    assert_eq!(child.wait().expect("wait").code(), Some(11), "through the first handle");
    assert_eq!(syscall::process_wait(second), Ok(11), "through the second handle");
    syscall::close(second);
    println!("  two handles to one process answer the same code");
}

/// A kill is a teardown like any other, so it publishes like one — and asking
/// for a process that is already gone to be gone is not a failure.
fn a_kill_publishes_like_an_exit() {
    let (mut child, _release) = start(0);
    let handle = RawHandle(child.as_raw_handle());
    syscall::process_kill(handle).expect("kill");
    assert_eq!(child.wait().expect("wait").code(), Some(KILLED), "a killed process's code");
    syscall::process_kill(handle).expect("killing a process that has gone is not a failure");
    assert_eq!(syscall::process_wait(handle), Ok(KILLED), "the code after the second kill");
    println!("  a kill publishes {KILLED} once, and a second kill changes nothing");
}

/// **The arm the pid-keyed shape could not have.** The waiter did not spawn the
/// subject, is not its parent by any spelling, and holds nothing but a handle
/// somebody moved into its table — and that is enough.
fn a_handle_is_the_whole_of_the_right() {
    let (mut subject, release) = start(9);
    let for_waiter =
        syscall::dup(RawHandle(subject.as_raw_handle())).expect("a handle for the waiter");

    let mut waiter = Command::new(SELF_PATH)
        .arg("waiter")
        .endow(SUBJECT_LABEL, for_waiter.0)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the waiter");

    drop(release);
    let mut said = String::new();
    waiter.stdout.take().expect("waiter stdout").read_to_string(&mut said).expect("waiter output");
    assert!(waiter.wait().expect("wait the waiter").success(), "the waiter exited nonzero");
    assert_eq!(said.trim(), "waited 9", "a holder that is not the parent could not wait");

    assert_eq!(subject.wait().expect("wait").code(), Some(9), "and the spawner still can");
    println!("  a process that did not start the child waited for it, and so did the one that did");
}

/// A pid is a name everybody can say, and saying it is not a key. The one call
/// that turns one into a handle needs a capability carrying `MANAGE`, and the
/// kernel mints exactly one — `/bin/init`'s. The test estate's carries `DEVICE`
/// and `DUP`, which is what makes this refusal non-vacuous: the handle resolves,
/// and it is the right that is missing.
fn a_pid_is_not_authority() {
    assert_eq!(
        syscall::process_open(cap().as_handle(), syscall::getpid()),
        Err(SyscallError::PermissionDenied),
        "a capability without MANAGE opened a process by pid",
    );
    println!("  a pid does not become a handle without MANAGE");
}

/// A child that exits with `code` when this process says so, and the write end
/// that says so: dropping it is what lets the child go.
///
/// It is what keeps a clock out of every arm above — until the drop the child is
/// provably running, because it is blocked reading a pipe whose only writer is
/// this process.
fn start(code: i32) -> (Child, ChildStdin) {
    let mut child = Command::new(SELF_PATH)
        .arg("held")
        .arg(code.to_string())
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn a held child");
    let stdin = child.stdin.take().expect("the held child's stdin");
    (child, stdin)
}

fn held() -> ! {
    let code: i32 = std::env::args().nth(2).expect("held needs a code").parse().expect("a code");
    let mut buf = [0u8; 1];
    let _ = std::io::stdin().read(&mut buf);
    std::process::exit(code);
}

fn waiter() -> ! {
    let subject: Process = Endowments::get()
        .take(SUBJECT_LABEL)
        .expect("the waiter was endowed a process handle");
    let code = subject.wait().expect("wait through an endowed handle");
    println!("waited {code}");
    std::process::exit(0);
}

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
//! The census arm is next. `handle_count` reaching zero is what releases an
//! object, and a kill is the path where nothing unwinds — so a leak per killed
//! process is exactly the defect this policy could introduce and the only place
//! it is visible is the kernel's own live-object count.
//!
//! **The last arms are the same matrix asked through a `POLL_ADD`, which is
//! where the kernel had a third answer nobody had written down.**
//! `process_poll_add` resolved its handle through one `let … else` that
//! swallowed `BadHandle`, `Stale` and a missing `Rights::WAIT` alike, and then
//! pushed a pending poll carrying no source — which no event site can reach and
//! no recheck can complete. A program that polled a handle it had already
//! closed was therefore neither answered nor ended: it went quiet. A submission
//! does have an error channel — the CQE — and these arms are what say so, one
//! per kind, plus the direction in which an object has no readiness at all.
//!
//! **`spawn-stale` is the one arm that is not a call refusing its own
//! argument.** A slot map is a parent deciding what its child is born holding,
//! and the kernel skipped a pair it could not resolve — so the child started
//! without a capability its parent had named and could not tell that from
//! having asked for nothing, while the parent was told its spawn happened as
//! asked. That is silent degradation of a capability, which is the one thing
//! this policy exists to remove, and the owner ruled it a kill on 2026-08-19.
//! The rule keeps exactly one exception and this is not it
//! (`kernel/src/object/handle.rs`).

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use toyos::census::Census;
use toyos::poller::{Poller, READABLE, WRITABLE};
use toyos::AsHandle;
use toyos_abi::handle::Rights;
use toyos_abi::syscall::{self, MmapFlags, MmapProt, SpawnArgs, SyscallError};
use toyos_abi::RawHandle;

const SELF_PATH: &str = "/bin/test_rs_handle_kill_policy";

/// How long a `POLL_ADD` that cannot ever fire is given to say so.
///
/// **A liveness bound and never a verdict.** Every arm below submits a poll the
/// kernel can answer without waiting for anything — a refusal, or an object
/// with no readiness in the direction asked for — so the answer is due in the
/// submitting syscall itself. What this number separates is "answered" from
/// "waits forever", which is the whole defect, and four orders of magnitude
/// above a syscall is enough to do that on any machine this suite runs on.
const POLL_ANSWER: Duration = Duration::from_secs(2);

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

/// The most 10 ms census samples `settled_census` takes before answering with
/// what it last saw. `handle_lifetime`'s bound, for the same deferred queues.
const SETTLE_SAMPLES: usize = 100;

/// The three kinds that end the caller. Each is a role this binary runs as, and
/// the description is what the kernel is being asked to refuse.
const FATAL: &[(&str, &str)] = &[
    ("bad-handle", "a slot this process never held"),
    ("stale", "a slot this process closed"),
    ("wrong-type", "a pipe where the call takes an acceptor"),
    ("faulting-thread", "a bad handle named by a thread that is not the main one"),
    // The same two mistakes made through a submission queue. They are separate
    // roles rather than a loop over the first two because what has to be
    // asserted is the same in both places and the *call* is what differs: a
    // syscall refuses where it stands, a `POLL_ADD` is refused inside
    // `inbox_submit` on the submitting thread.
    ("poll-bad-handle", "a POLL_ADD on a slot this process never held"),
    ("poll-stale", "a POLL_ADD on a slot this process closed"),
    // The third site the same audit found. It answered `NotFound` for every
    // way the handle could fail, so "you named a handle you do not hold" and
    // "this machine has no such device" were one word — and the second is a
    // fact a driver acts on.
    ("device-reg-bad-handle", "a device register read on a slot this process never held"),
    // The fourth, and the one that is not a call refusing its own argument: a
    // spawn's slot map is a parent deciding what its child is born holding.
    // The kernel skipped a pair it could not resolve, so the child started
    // without a capability its parent had named and could not tell that from
    // having asked for nothing — and the parent was told its spawn happened as
    // asked. Ruled a kill on 2026-08-19 (`object::HandleError`).
    ("spawn-stale", "a spawn's slot map naming a handle this process closed"),
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
    a_poll_without_wait_is_a_word();
    a_poll_with_no_source_is_answered();

    println!("three handle failures end the caller, two answer it, and neither leaks");
}

/// A `POLL_ADD` on a handle that does not carry `Rights::WAIT` is a word.
///
/// **The direction the fix must not overshoot.** A process may legitimately
/// hold an attenuated handle and ask what it can still do with it, so this may
/// never be a kill — and the kernel's old answer was neither: the poll was
/// registered on nothing and the caller waited for a completion that could not
/// exist. A region is the honest subject, because `Rights::WAIT` is not in what
/// `SYS_SHM_CREATE` mints (`object::ops::initial_rights`), so nothing is
/// narrowed here to stage it.
fn a_poll_without_wait_is_a_word() {
    let region = toyos::shm::SharedMemory::create(4096).expect("a region to poll");
    let answered = answered_within(POLL_ANSWER, region.as_handle(), READABLE);
    assert!(
        answered.is_some(),
        "a POLL_ADD on a handle carrying no WAIT was neither answered nor refused in \
         {POLL_ANSWER:?} — it was registered on nothing",
    );
    println!("  poll without WAIT: answered in {:?}, and the process is still here", answered.unwrap());
}

/// A `POLL_ADD` in a direction the object has no readiness in is answered.
///
/// **Not a refusal, and that is why it is its own arm.** The handle resolves,
/// it carries `WAIT`, and the caller is entitled to ask — a pipe's read end
/// simply has no writability, so there is no source to register on and nothing
/// that could ever complete the poll. POSIX's `poll` for `POLLOUT` on a read end is
/// exactly this call, so it is a mistake real programs make, and the answer the
/// kernel gave was silence.
fn a_poll_with_no_source_is_answered() {
    let (read, _write) = toyos::pipe_pair().expect("a pipe with a read end");
    let answered = answered_within(POLL_ANSWER, read.as_handle(), WRITABLE);
    assert!(
        answered.is_some(),
        "a POLL_ADD for writability on a pipe's read end went unanswered for \
         {POLL_ANSWER:?} — the poll was pushed with no source behind it",
    );
    println!("  poll with no source: answered in {:?}", answered.unwrap());
}

/// How long a poll on `handle` took to complete, or `None` if it never did.
///
/// The result word is deliberately not asserted on: `Poller::wait` hands back
/// the token and not the CQE, and what these arms are about is that an answer
/// arrives at all. Which word it is belongs to the kernel's own matrix.
fn answered_within(bound: Duration, handle: RawHandle, flags: u32) -> Option<Duration> {
    let poller = Poller::new(1);
    poller.watch_raw(handle, flags, 0);
    let started = Instant::now();
    let mut seen = 0usize;
    poller.wait(1, bound.as_nanos() as u64, |_| seen += 1);
    (seen > 0).then(|| started.elapsed())
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
///
/// And each sample is a *settled* census, because the two-sample design alone
/// still loses to the lag it describes: on a loaded CI shard the last corpse's
/// own `Process` object outlived the parent's `wait` into the second census —
/// `[("Process", 6, 7)]`, twice on one shard, green alone both times (PR #141
/// run 32307331537, the same deferral `handle_lifetime` measured decaying across
/// eight back-to-back reads). The kernel half is
/// `issues/kernel/deferred-release-outlives-its-syscall.md`; here it is a lag
/// and not a leak exactly when settling converges, which is what
/// `settled_census` requires before it answers.
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
    settled_census()
}

/// The census once the deferred queues have finished giving back what the
/// kills released. `handle_lifetime`'s `settled_free_bytes`, for object counts:
/// sample until two readings ten milliseconds apart agree, which is the
/// machine saying it has finished. **A liveness bound and not a margin** — a
/// kernel that leaks holds a stable, elevated census, is quiescent on the
/// first pair, and the grown-kinds assertion above reds exactly as before.
fn settled_census() -> Census {
    let mut last = Census::now();
    for _ in 0..SETTLE_SAMPLES {
        std::thread::sleep(Duration::from_millis(10));
        let next = Census::now();
        if next == last {
            return next;
        }
        last = next;
    }
    last
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

/// `SYS_SPAWN` with `[[3, handle]]` as its slot map.
///
/// **The program is one no image carries, and that is deliberate.** The slot
/// map is read before the path is resolved, so a kernel that holds the ruling
/// never looks at it — and a kernel that put the skip back is refused for the
/// path instead, which reaches the caller's `panic!` with the wrong exit code
/// rather than starting a second copy of this test.
///
/// One mmap region for both blobs: `user_bytes` reads a physically contiguous
/// window, so a stack buffer straddling a page would be refused on
/// `BadAddress` without ever reaching `build_child_handles`.
fn spawn_naming(handle: RawHandle) -> Result<RawHandle, SyscallError> {
    const REGION: usize = 4096;
    const SLOT_MAP_OFF: usize = 2048;
    const ARGV: &str = "/bin/no-such-program\0";

    let region = unsafe {
        syscall::mmap(
            core::ptr::null_mut(),
            REGION,
            MmapProt::READ | MmapProt::WRITE,
            MmapFlags::ANONYMOUS | MmapFlags::PRIVATE,
        )
    };
    assert!(!region.is_null(), "mmap a region for the spawn blobs");
    let pair = [3u32.to_ne_bytes(), handle.0.to_ne_bytes()].concat();
    unsafe {
        core::ptr::copy_nonoverlapping(ARGV.as_ptr(), region, ARGV.len());
        core::ptr::copy_nonoverlapping(pair.as_ptr(), region.add(SLOT_MAP_OFF), pair.len());
    }
    unsafe {
        syscall::spawn(&SpawnArgs {
            argv_ptr: region as u64,
            argv_len: ARGV.len() as u64,
            slot_map_ptr: region as u64 + SLOT_MAP_OFF as u64,
            slot_map_count: 1,
            env_ptr: 0,
            env_len: 0,
            endow_ptr: 0,
            endow_count: 0,
            labels_ptr: 0,
            labels_len: 0,
        })
    }
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
        // The submission form of `bad-handle`. The kill lands inside
        // `inbox_submit`, on this thread, while it is processing the SQE —
        // so a tree that answers instead of ending comes back from `wait` and
        // reaches the panic below with the wrong exit code.
        "poll-bad-handle" => {
            let poller = Poller::new(1);
            poller.watch_raw(RawHandle(UNHELD_SLOT), READABLE, 0);
            let mut seen = 0usize;
            poller.wait(1, POLL_ANSWER.as_nanos() as u64, |_| seen += 1);
            panic!("a POLL_ADD on a slot this process never held left it running ({seen} CQEs)");
        }
        // And of `stale`. The pipe is closed before the poll is submitted, so
        // the handle names a slot at an earlier generation — the one case a
        // program reaches by forgetting the order of its own close and poll.
        "poll-stale" => {
            let (read, _write) = toyos::pipe_pair().expect("a pipe to close");
            let closed = read.as_handle();
            drop(read);
            let poller = Poller::new(1);
            poller.watch_raw(closed, READABLE, 0);
            let mut seen = 0usize;
            poller.wait(1, POLL_ANSWER.as_nanos() as u64, |_| seen += 1);
            panic!("a POLL_ADD on a handle this process closed left it running ({seen} CQEs)");
        }
        // No capability needed: the handle never resolves, so the call is
        // refused before anything asks what device it names.
        "device-reg-bad-handle" => {
            let read = syscall::device_reg_read(
                RawHandle(UNHELD_SLOT),
                0,
                toyos_abi::syscall::RegWidth::U32,
            );
            panic!("a device register read on a slot this process never held answered {read:?}");
        }
        // A parent naming a handle it does not hold in a spawn's slot map. The
        // pipe is closed before the spawn, which is the shape a real parent
        // reaches — a program that closed a stdio slot and then spawned a
        // child asking to inherit it.
        "spawn-stale" => {
            let (read, _write) = toyos::pipe_pair().expect("a pipe to close");
            let closed = read.as_handle();
            drop(read);
            let started = spawn_naming(closed);
            panic!("a spawn naming a handle this process closed answered {started:?}");
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

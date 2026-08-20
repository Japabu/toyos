//! A thread killed while blocked on a handle still gives the handle back.
//!
//! **This is the handle property that matters most for this architecture, and
//! nothing in the tree executed it.**
//!
//! `handle_count` is deliberately not the `Arc` count (`kernel/src/object/`).
//! The reason is exactly this shape: a blocking syscall clones an `Arc` out of
//! the table before it parks, this kernel does not unwind, and a thread another
//! CPU kills never runs a destructor — so that `Arc` is stranded on a kernel
//! stack that is simply freed. If EOF and dead-peer detection rode `Arc`
//! counts, killing a client blocked in its signal-pipe read would leak the read
//! end and the server would never learn. That is the steady state of every cpal
//! client, so "never learn" means soundd writing into a ring nobody reads for
//! the rest of the boot.
//!
//! Four arms, each a child killed at a point it cannot come back from:
//!
//! 1. **Blocked reading a pipe.** The parent holds the only write end and must
//!    see the read side gone.
//! 2. **Blocked reading an IPC connection**, which is the soundd shape one
//!    layer up: the parent's write must answer `Gone`.
//! 3. **Blocked in `accept`.** The child holds the port's only acceptor; the
//!    parent's next connect through the connector must be `Gone`.
//! 4. **Not blocked at all** — spinning in Ring 3, with an empty kernel stack
//!    and no syscall to cancel. The other three ask what a kill *releases*;
//!    this one asks whether it **ends** — "a killed thread is never dispatched
//!    into userland again", the claim about a kill that no other test in this
//!    tree executes. It is the one arm that does not issue
//!    its own kill: a `kill` that does not end its target does not return
//!    either, so the killer is a process of its own and this one only watches.
//!
//! **No census assertion anywhere in this file, and that is the point rather
//! than an omission.** A live-object count is the one instrument that cannot
//! judge these three: a thread parked in `accept` has cloned the `Arc` out of
//! its table onto its own kernel stack, so killing it strands that `Arc` on
//! memory that is freed without unwinding and the object stays *alive* — which
//! this design accepts and says so (`kernel/src/object/`, §1.1). What must not
//! survive is the **handle count**, because that is what every peer-visible
//! event rides. So each arm asks a peer, and none of them counts objects.
//!
//! **The marker is what gives every arm teeth.** Without it a child killed
//! before it reached its `read` would pass while asserting nothing — the handle
//! would have been released by an ordinary table drain with no `Arc` stranded
//! anywhere, which is the case that is *not* under test.

use std::io::{Read, Write};
use std::os::toyos::process::{ChildExt, CommandExt};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use toyos::process::Process;
use toyos::{endow, namespace, port, AsHandle};
use toyos_abi::syscall::{self, SyscallError, SERVE_PREFIX, SVC_LABEL};
use toyos_abi::RawHandle;

const SELF_PATH: &str = "/bin/test_rs_kill_while_blocked";
const SERVICE: &str = "blocked";

/// The label arm 4's killer finds the spinner under. A local name in one
/// process's own table, and it names nothing anywhere else.
const VICTIM_LABEL: &str = "victim";

/// How long arm 4 gives a killed Ring 3 spinner to reach its last exit
/// boundary, watched from outside the kill.
///
/// **A number rather than a hang**: a guest that stops making progress reds
/// as `STALL`, which the harness prints apart and tells nobody to bisect.
/// What it buys is a failure that names itself.
///
/// **Priced against the quantity it actually bounds**, which is not one
/// interrupt delivery: what this constant covers is the whole of
/// [go byte → boundary → teardown], and the only measurement of that window is
/// this arm's own recorded green run, 4.972884 ms. Two seconds against it is
/// 402×. The sentence that used to stand here said "four orders of magnitude of
/// headroom", which would be true of the interrupt delivery alone — precisely
/// the part this arm cannot observe from outside the kill, and the part the
/// window's several scheduler dispatches sit on top of.
///
/// **It has to be smaller than `scheduler::retire_task`'s own tripwire, and
/// that ordering is the whole of the arm's ability to report.** The process
/// doing the killing is parked inside that tripwire for as long as the victim
/// stays in userland, and when it blows the kernel panics and takes the machine
/// with it — so an arm that had not spoken by then never speaks. Nothing here
/// reads that constant or restates its value: what this one needs is only to be
/// first, and a bound derived from device timeouts and scheduler quanta is not
/// going to come in under two seconds.
const ENDS_WITHIN: Duration = Duration::from_secs(2);

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some(role) => child(role),
        None => test(),
    }
}

fn test() {
    a_pipe_reader_killed_in_the_read();
    a_connection_peer_killed_in_the_read();
    an_acceptor_killed_in_the_accept();
    a_ring_three_spinner_ends_at_its_next_exit_boundary();
    println!("a killed thread's blocking handle is released, and the peer is told");
}

/// Spawn a child in `role` and wait for it to say it has parked.
///
/// The child's stdin is a pipe this process holds the write end of, which is
/// what arms 1 and 2 measure afterwards and what releases arm 4's killer at a
/// moment this process picks.
fn parked(role: &str, extra: Option<(String, u32)>) -> std::process::Child {
    let mut command = Command::new(SELF_PATH);
    command.arg(role).stdin(Stdio::piped()).stdout(Stdio::piped());
    if let Some((label, handle)) = extra {
        command.endow(&label, handle);
    }
    let mut child = command.spawn().unwrap_or_else(|e| panic!("spawn {role}: {e}"));

    let mut byte = [0u8; 1];
    let mut line = Vec::new();
    let out = child.stdout.as_mut().expect("child stdout");
    while out.read(&mut byte).expect("read the child's marker") == 1 {
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    assert_eq!(
        String::from_utf8_lossy(&line),
        format!("parked in {role}"),
        "{role} never reached its blocking call, so nothing was killed while blocked",
    );
    child
}

/// Arm 1. The child is inside `read` on its stdin when it is killed.
///
/// A pipe with no reader refuses a write; a pipe whose only reader is a
/// stranded `Arc` on a freed kernel stack takes one and nobody ever reads it.
/// The write is what tells the two apart.
fn a_pipe_reader_killed_in_the_read() {
    let mut child = parked("pipe-read", None);
    // **Taken before `wait`, which closes it.** `Child::wait` drops the write
    // end first — a child blocked reading a pipe its parent still holds would
    // never exit otherwise — so the handle under test has to leave the `Child`
    // before then.
    let mut stdin = child.stdin.take().expect("the child's stdin");
    child.kill().expect("kill the parked child");
    let _ = child.wait();

    let refused = stdin.write_all(b"nobody is reading this");
    assert!(
        refused.is_err(),
        "a pipe whose only reader was killed mid-read still took a write",
    );
    println!("  pipe: the write end learned its reader had gone");
}

/// Arm 2. The soundd shape: the child is blocked reading a connection.
fn a_connection_peer_killed_in_the_read() {
    let (acceptor, connector) = port::create().expect("a port of our own");
    let ns = namespace::build()
        .add(SERVICE, &connector)
        .finish()
        .expect("a namespace carrying one connector");
    let mut child = parked("connection-read", Some((SVC_LABEL.to_string(), ns.into_raw().0)));
    let conn = acceptor.accept().expect("the child connected");

    child.kill().expect("kill the parked child");
    let _ = child.wait();

    // **`NotFound` and not `Gone`**, which
    // `issues/isolation/a-broken-pipe-answers-not-found.md` is about;
    // `connect_before_serve` asserts the same word for the same reason. What
    // this arm is for is the *release* — on a kernel where the killed thread's
    // stranded `Arc` kept the read end alive, this write succeeds — and either
    // word says the peer went.
    assert_eq!(
        conn.write_nonblock(b"nobody is reading this"),
        Err(SyscallError::NotFound),
        "a connection whose peer was killed mid-read still took a write",
    );
    println!("  connection: the write answered Gone");
}

/// Arm 3. Blocked in `accept`, which is the wait `Acceptor` added.
///
/// **The peer here is a client of the port**, and it is the only thing that can
/// answer. The parked thread holds the acceptor's `Arc` on its own kernel
/// stack, so the object survives the kill; what must not survive is the handle
/// count, because `Acceptor::on_zero_handles` is what sets the port `closed`
/// and a connect through the connector is what reads it.
fn an_acceptor_killed_in_the_accept() {
    let (acceptor, connector) = port::create().expect("a port of our own");
    let label = format!("{SERVE_PREFIX}{SERVICE}");
    let mut child = parked("accept", Some((label, acceptor.into_raw().0)));

    // The parent's own way to ask, over the connector it kept.
    let ns = namespace::build()
        .add(SERVICE, &connector)
        .finish()
        .expect("a namespace carrying one connector");
    child.kill().expect("kill the parked child");
    let _ = child.wait();
    drop(child);

    assert_eq!(
        ns.open(SERVICE).map(|conn| conn.as_handle()),
        Err(SyscallError::Gone),
        "a port whose only acceptor was killed mid-accept still queued a connection",
    );
    println!("  accept: the port closed and a connect answered Gone");
}

/// Arm 4. The child is spinning in Ring 3 — no syscall, no handle, nothing to
/// cancel — when it is killed, and it must be gone.
///
/// **This is the arm the other three cannot stand in for.** They kill a thread
/// that is *inside the kernel*, where a retire reaches it as a park to cancel;
/// this one kills a thread whose kernel stack is empty, so the only thing that
/// can end it is the boundary it crosses on its way back into userland. Both
/// ways that boundary can be missed were live on this branch:
/// `kernel_exit_to_user_check` read the kill bit once *above* its reschedule
/// loop, and the Ring 3 timer stub — which is where the retire's own IPI lands,
/// because `apic::kick_cpu` sends TIMER_VECTOR — did not run that epilogue at
/// all. With either miss the child here is preempted, queued in the dying list,
/// picked straight back off it and returned to Ring 3, once per tick, forever.
///
/// **Three processes, because the killer cannot report.** `Process::kill` is
/// `scheduler::retire_task`, which parks until the victim's record is released
/// and panics the kernel at its own tripwire when it never is. On exactly the
/// tree this arm exists to catch, then, the call does not come back: an arm
/// that killed and then timed its own `kill` would be *inside* the panic it is
/// meant to name, and the version of this arm that did so could only ever have
/// reported on a tree that was already fixed. So the kill is a child of its
/// own, and the deadline below is held by a process with nothing of the kernel
/// under it.
///
/// **What it watches is the victim's own stdout.** That pipe's write end is in
/// the victim's handle table and in no other, so the read end this process
/// holds reaches EOF when — and only when — the victim's handles are drained.
/// That is `kill_process`'s phase 3, which runs after `retire_task` has seen
/// the victim released, which the victim publishes from its own pass out of
/// `exit_if_killed` — the last exit boundary itself. EOF is downstream of the
/// boundary by a teardown and by nothing that can wait, and it is unreachable
/// without it.
///
/// **The clock starts before the kill and not after it**, which is the second
/// half of what was wrong here: the previous shape took its `Instant` once
/// `kill()` had already returned, and a returned `kill` has already published
/// the exit — so what it timed was the reap of a zombie and never the boundary.
/// What this one bounds is the whole of [go byte → boundary → teardown], so the
/// number is an upper bound on the boundary rather than a measurement of it.
fn a_ring_three_spinner_ends_at_its_next_exit_boundary() {
    let mut victim = parked("spin", None);
    // Taken out of the `Child` because the observation is the pipe and not the
    // process: `parked` has already read the marker line off it, so the next
    // thing that can ever arrive on it is the end of the victim.
    let mut spun = victim.stdout.take().expect("the spinner's stdout");

    // A duplicate, because `endow` moves what it is handed: this process keeps
    // its own handle so that dropping the `Child` stays its business.
    let for_killer =
        syscall::dup(RawHandle(victim.as_raw_handle())).expect("a second handle to the spinner");
    let mut killer = parked("kill", Some((VICTIM_LABEL.to_string(), for_killer.0)));
    let mut go = killer.stdin.take().expect("the killer's stdin");

    /// Nanoseconds from the go byte to EOF, and `u64::MAX` until there is one.
    /// Stored by the reader so the answer is the instant it saw rather than the
    /// poll that noticed.
    static GONE_AFTER_NS: AtomicU64 = AtomicU64::new(u64::MAX);
    let started = Instant::now();
    thread::spawn(move || {
        let mut byte = [0u8; 1];
        while spun.read(&mut byte).expect("read the spinner's stdout") != 0 {}
        GONE_AFTER_NS.store(started.elapsed().as_nanos() as u64, Ordering::Release);
    });
    go.write_all(b"g").expect("release the killer");

    while GONE_AFTER_NS.load(Ordering::Acquire) == u64::MAX && started.elapsed() < ENDS_WITHIN {
        thread::sleep(Duration::from_millis(1));
    }
    let gone = GONE_AFTER_NS.load(Ordering::Acquire);
    if gone == u64::MAX {
        println!(
            "a child killed while spinning in Ring 3 still held its handles {:?} later — \
             it is being re-dispatched into userland, so nothing on the return path reads \
             the kill bit, and the process that killed it is parked in retire_task with \
             nothing it can say",
            started.elapsed(),
        );
        std::process::exit(1);
    }
    // Bounded by the line above and not a second deadline: EOF is phase 3, and
    // phases 4 and 5 behind it are straight-line kernel code with no wait in
    // them, so a killer that produced the EOF has already returned from its
    // `kill`. What this adds is the killer's own verdict on that call.
    assert!(
        killer.wait().expect("wait for the killer").success(),
        "the spinner ended, but the process that killed it did not agree",
    );
    println!(
        "  ring 3: a killed spinner reached its last exit boundary in {:?}, with no syscall \
         to cancel",
        Duration::from_nanos(gone),
    );
}

fn child(role: &str) -> ! {
    match role {
        // No syscall after the marker, deliberately: a `sleep` or a `write`
        // would put a kernel stack under the thread and give the retire a park
        // to cancel, which is arms 1–3 and not this one. `black_box` is what
        // stops the loop being optimised into nothing.
        "spin" => {
            say("parked in spin");
            loop {
                std::hint::black_box(0u64);
                std::hint::spin_loop();
            }
        }
        // Arm 4's killer. It is a process rather than a thread because
        // `Process::kill` does not return on the tree that arm is about — it
        // is `retire_task`, which parks until the victim's record is released
        // and panics the kernel at its own tripwire when it never is. Nothing
        // the observer does is behind this call.
        "kill" => {
            let victim: Process = endow::Endowments::get()
                .take(VICTIM_LABEL)
                .expect("the parent endowed a handle to the spinner");
            say("parked in kill");
            let mut go = [0u8; 1];
            std::io::stdin().read_exact(&mut go).expect("wait for the parent's go");
            victim.kill().expect("kill the spinning child");
            std::process::exit(0);
        }
        "pipe-read" => {
            say("parked in pipe-read");
            let mut buf = [0u8; 64];
            let n = std::io::stdin().read(&mut buf).expect("read stdin");
            panic!("pipe-read came back with {n} bytes");
        }
        "connection-read" => {
            let ns = endow::namespace().expect("the parent endowed a namespace");
            let conn = ns.open(SERVICE).expect("connect through the endowed connector");
            say("parked in connection-read");
            let mut buf = [0u8; 64];
            let n = syscall::read(conn.as_handle(), &mut buf).expect("read the connection");
            panic!("connection-read came back with {n} bytes");
        }
        "accept" => {
            let acceptor = endow::acceptor(SERVICE).expect("the parent endowed an acceptor");
            say("parked in accept");
            let taken = acceptor.accept().map(|conn| conn.as_handle());
            panic!("accept came back with {taken:?}");
        }
        other => panic!("unknown role {other:?}"),
    }
}

/// The marker, in one write and flushed: the parent reads it to know the child
/// is *in* the call rather than on the way to it.
///
/// It is printed immediately before the blocking call, so the window between
/// the two is a handful of instructions. A kill that landed inside that window
/// would release the handle from an ordinary drain with nothing stranded, which
/// is the case this file is not about — and the arms would still pass, so the
/// residual is a weaker test rather than a wrong one.
fn say(line: &str) {
    let mut out = std::io::stdout();
    out.write_all(line.as_bytes()).expect("say");
    out.write_all(b"\n").expect("say");
    out.flush().expect("flush");
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::process::{Command, Stdio};

use toyos_abi::syscall;
use toyos::poller::{Poller, IORING_POLL_IN};
use toyos::{namespace, port};

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("burn") => {
            let ms: u64 = std::env::args().nth(2).unwrap().parse().unwrap();
            child_burn(ms);
        }
        Some("sched-info") => child_sched_info(),
        _ => run_tests(),
    }
}

fn run_tests() {
    test_acceptor_isolation_io_uring();
    test_min_vruntime_invariant();
    test_connect_storm();
    println!("all sched_stress tests passed");
}

fn child_burn(ms: u64) {
    let mut count = 0u64;
    let start = std::time::Instant::now();
    let dur = Duration::from_millis(ms);
    while start.elapsed() < dur {
        count += 1;
        if count % 1000 == 0 { thread::yield_now(); }
    }
    println!("{count}");
}

fn child_sched_info() {
    // Force a Runnable→NonRunnable→Runnable cycle so the lag we report is
    // the freshly-clamped post-wake value, not whatever drift has built up
    // since spawn. The kernel clamps lag to ±MAX_VRUNTIME_LAG_NS at the
    // transition; reading immediately after wake is the only point where
    // the invariant holds deterministically.
    thread::sleep(Duration::from_millis(10));
    let info = syscall::sched_info();
    println!("{} {} {}", info.vruntime, info.min_vruntime, info.lag);
}

// Test 1: Acceptor isolation via io_uring POLL_IN

/// Two acceptors create io_uring POLL_IN watches on their handles. A connection
/// opened through port A's connector must complete A's poll and only A's — a
/// readiness signal keyed on "some acceptor" instead of on the object wakes
/// every waiter in the system, which froze the compositor.
fn test_acceptor_isolation_io_uring() {
    let a_ready = Arc::new(AtomicBool::new(false));
    let b_ready = Arc::new(AtomicBool::new(false));
    let a_ready2 = Arc::clone(&a_ready);
    let b_ready2 = Arc::clone(&b_ready);

    let (acc_a, con_a) = port::create().expect("port a");
    let (acc_b, _con_b) = port::create().expect("port b");

    // Thread A: watch its acceptor, report whether the poll completed
    let a = thread::spawn(move || -> bool {
        let fd = acc_a.into_raw();
        a_ready2.store(true, Ordering::Release);
        let poller = Poller::new(1);
        poller.poll_add_fd(fd, IORING_POLL_IN, 0);
        let mut ready = false;
        poller.wait(1, 500_000_000, |_| ready = true);
        syscall::close(fd);
        ready
    });

    // Thread B: a different port, watched the same way
    let b = thread::spawn(move || -> bool {
        let fd = acc_b.into_raw();
        b_ready2.store(true, Ordering::Release);
        let poller = Poller::new(1);
        poller.poll_add_fd(fd, IORING_POLL_IN, 0);
        let mut ready = false;
        poller.wait(1, 200_000_000, |_| ready = true);
        syscall::close(fd);
        ready
    });

    // Wait for both to be watching
    while !a_ready.load(Ordering::Acquire) || !b_ready.load(Ordering::Acquire) {
        thread::yield_now();
    }
    thread::sleep(Duration::from_millis(20));

    // Open a connection through port A's connector only.
    let ns = namespace::build().add("a", &con_a).finish().expect("a namespace naming port a");
    let client = ns.open("a").expect("open a");
    drop(client);

    let a_poll_ready = a.join().unwrap();
    let b_poll_ready = b.join().unwrap();

    assert!(a_poll_ready, "port a's poll should have completed (connection pending)");
    assert!(!b_poll_ready, "port b's poll completed spuriously — acceptor isolation broken!");

    println!("  acceptor isolation (io_uring): ok");
}

/// Verify the post-wake lag invariant: when a process transitions from
/// non-runnable back to runnable, its lag is clamped to ±MAX_VRUNTIME_LAG_NS
/// (50ms). Read immediately after the wake transition.
fn test_min_vruntime_invariant() {
    let me = "/bin/test_rs_sched_stress";

    // Spawn 3 CPU burners to drive min_vruntime forward.
    let mut burners = Vec::new();
    for _ in 0..3 {
        burners.push(Command::new(me).arg("burn").arg("1000")
            .stdout(Stdio::piped()).spawn().expect("spawn burner"));
    }

    // Let them run for 500ms to accumulate vruntime.
    thread::sleep(Duration::from_millis(500));

    // Spawn a process that sleeps briefly (forces Runnable→NonRunnable→
    // Runnable) and reports lag immediately after wake.
    let info_child = Command::new(me).arg("sched-info")
        .stdout(Stdio::piped()).spawn().expect("spawn sched-info");
    let info_out = info_child.wait_with_output().expect("wait sched-info");
    let output = String::from_utf8_lossy(&info_out.stdout);
    let parts: Vec<&str> = output.trim().split_whitespace().collect();
    assert_eq!(parts.len(), 3, "sched-info output should be 'vruntime min_vruntime lag', got: {output:?}");
    let vruntime: u64 = parts[0].parse().expect("parse vruntime");
    let min_vruntime: u64 = parts[1].parse().expect("parse min_vruntime");
    let lag: i64 = parts[2].parse().expect("parse lag");

    // Clean up burners.
    for child in burners {
        let _ = child.wait_with_output();
    }

    println!("  sched_info: vruntime={vruntime} min_vruntime={min_vruntime} lag={lag}");

    assert!(min_vruntime > 0,
        "min_vruntime is still 0 after 500ms of CPU-bound work — not being updated!");

    let max_lag_ns: i64 = 50_000_000;
    assert!(lag.abs() <= max_lag_ns,
        "post-wake lag ({lag}) exceeds ±MAX_VRUNTIME_LAG_NS ({max_lag_ns}) — \
         leave_runnable clamp not enforced or wake is not re-deriving from lag!");

    println!("  post-wake lag invariant: ok");
}

fn test_connect_storm() {
    let num_clients = 8;

    let (acceptor, connector) = port::create().expect("a port to storm");
    let ns = Arc::new(
        namespace::build().add("storm", &connector).finish().expect("a namespace naming it"),
    );

    let server = thread::spawn(move || {
        let fd = acceptor.into_raw();
        for _ in 0..num_clients {
            let result = syscall::accept(fd).expect("accept failed");
            syscall::close(result.fd);
        }
        syscall::close(fd);
    });

    thread::sleep(Duration::from_millis(50));

    let mut clients = Vec::new();
    for _ in 0..num_clients {
        let ns = Arc::clone(&ns);
        clients.push(thread::spawn(move || {
            drop(ns.open("storm").expect("open failed"));
        }));
    }

    for c in clients {
        c.join().unwrap();
    }
    server.join().unwrap();
    println!("  connect storm ({num_clients} clients): ok");
}

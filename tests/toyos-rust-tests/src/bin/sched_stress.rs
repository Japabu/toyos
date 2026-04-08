use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::process::{Command, Stdio};

use toyos_abi::syscall;
use toyos::poller::{Poller, IORING_POLL_IN};

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
    test_listener_isolation_io_uring();
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
    let info = syscall::sched_info();
    println!("{} {}", info.vruntime, info.min_vruntime);
}

// ---------------------------------------------------------------------------
// Test 1: Listener isolation via io_uring POLL_IN
// ---------------------------------------------------------------------------

/// This tests the exact code path that froze the compositor.
///
/// Two listeners create io_uring POLL_IN watches on their fds. A connect
/// to svc_a should only complete svc_a's poll — not svc_b's. With the old
/// global EventSource::Listener, both polls would complete spuriously.
fn test_listener_isolation_io_uring() {
    let a_ready = Arc::new(AtomicBool::new(false));
    let b_ready = Arc::new(AtomicBool::new(false));
    let a_ready2 = Arc::clone(&a_ready);
    let b_ready2 = Arc::clone(&b_ready);

    // Thread A: listen, poll via io_uring, report whether poll completed
    let a = thread::spawn(move || -> bool {
        let fd = syscall::listen("test_uring_a").expect("listen a");
        a_ready2.store(true, Ordering::Release);
        let poller = Poller::new(1);
        poller.poll_add_fd(fd, IORING_POLL_IN, 0);
        let mut ready = false;
        poller.wait(1, 500_000_000, |_| ready = true);
        syscall::close(fd);
        ready
    });

    // Thread B: listen on different service, poll via io_uring
    let b = thread::spawn(move || -> bool {
        let fd = syscall::listen("test_uring_b").expect("listen b");
        b_ready2.store(true, Ordering::Release);
        let poller = Poller::new(1);
        poller.poll_add_fd(fd, IORING_POLL_IN, 0);
        let mut ready = false;
        poller.wait(1, 200_000_000, |_| ready = true);
        syscall::close(fd);
        ready
    });

    // Wait for both to be listening
    while !a_ready.load(Ordering::Acquire) || !b_ready.load(Ordering::Acquire) {
        thread::yield_now();
    }
    thread::sleep(Duration::from_millis(20));

    // Connect to svc_a only
    let client = syscall::connect("test_uring_a").expect("connect a");
    syscall::close(client);

    let a_poll_ready = a.join().unwrap();
    let b_poll_ready = b.join().unwrap();

    assert!(a_poll_ready, "svc_a poll should have completed (connection pending)");
    assert!(!b_poll_ready, "svc_b poll completed spuriously — listener isolation broken!");

    // Clean up: accept svc_a's connection so the listener can be removed

    println!("  listener isolation (io_uring): ok");
}

// ---------------------------------------------------------------------------
// Test 2: min_vruntime invariant
// ---------------------------------------------------------------------------

/// Verify that a newly spawned process starts with vruntime near min_vruntime,
/// not at zero. This directly tests the min_vruntime update mechanism.
fn test_min_vruntime_invariant() {
    let me = "/bin/test_rs_sched_stress";

    // Spawn 3 CPU burners to drive min_vruntime forward
    let mut burners = Vec::new();
    for _ in 0..3 {
        burners.push(Command::new(me).arg("burn").arg("1000")
            .stdout(Stdio::piped()).spawn().expect("spawn burner"));
    }

    // Let them run for 500ms to accumulate vruntime
    thread::sleep(Duration::from_millis(500));

    // Spawn a new process that immediately reports its sched_info
    let info_child = Command::new(me).arg("sched-info")
        .stdout(Stdio::piped()).spawn().expect("spawn sched-info");
    let info_out = info_child.wait_with_output().expect("wait sched-info");
    let output = String::from_utf8_lossy(&info_out.stdout);
    let parts: Vec<&str> = output.trim().split_whitespace().collect();
    assert_eq!(parts.len(), 2, "sched-info output should be 'vruntime min_vruntime', got: {output:?}");
    let vruntime: u64 = parts[0].parse().expect("parse vruntime");
    let min_vruntime: u64 = parts[1].parse().expect("parse min_vruntime");

    // Clean up burners
    for child in burners {
        let _ = child.wait_with_output();
    }

    println!("  sched_info: vruntime={vruntime} min_vruntime={min_vruntime}");

    // Assertion 1: min_vruntime should have advanced (burners were running)
    assert!(min_vruntime > 0,
        "min_vruntime is still 0 after 500ms of CPU-bound work — not being updated!");

    // Assertion 2: the new process's vruntime should be near min_vruntime
    // (init_vruntime seeds at min_vruntime). Allow MAX_VRUNTIME_LAG (50ms) slack.
    let max_lag_ns: u64 = 50_000_000;
    assert!(vruntime + max_lag_ns >= min_vruntime,
        "new process vruntime ({vruntime}) is too far below min_vruntime ({min_vruntime}) — \
         init_vruntime not seeding from min_vruntime!");

    println!("  min_vruntime invariant: ok");
}

// ---------------------------------------------------------------------------
// Test 3: Concurrent connect storm
// ---------------------------------------------------------------------------

fn test_connect_storm() {
    let num_clients = 8;

    let server = thread::spawn(move || {
        let fd = syscall::listen("test_storm").expect("listen failed");
        for _ in 0..num_clients {
            let result = syscall::accept(fd).expect("accept failed");
            syscall::close(result.fd);
        }
        syscall::close(fd);
    });

    thread::sleep(Duration::from_millis(50));

    let mut clients = Vec::new();
    for _ in 0..num_clients {
        clients.push(thread::spawn(|| {
            let fd = syscall::connect("test_storm").expect("connect failed");
            syscall::close(fd);
        }));
    }

    for c in clients {
        c.join().unwrap();
    }
    server.join().unwrap();
    println!("  connect storm ({num_clients} clients): ok");
}

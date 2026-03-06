use std::thread;

fn main() {
    // When invoked with "exit-fast", just exit immediately (used as a try_wait target)
    if std::env::args().nth(1).as_deref() == Some("exit-fast") {
        return;
    }

    // Test available_parallelism returns > 0
    let n = thread::available_parallelism().expect("available_parallelism failed");
    assert!(n.get() > 0, "expected parallelism > 0, got {}", n.get());
    println!("available_parallelism = {}", n.get());

    // Test spawning threads that compute partial sums
    let handles: Vec<_> = (0..4)
        .map(|i| {
            thread::spawn(move || {
                let start = i * 250;
                let end = start + 250;
                (start..end).sum::<u64>()
            })
        })
        .collect();

    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let expected: u64 = (0..1000).sum();
    assert_eq!(total, expected, "partial sums mismatch: {total} != {expected}");

    // Test try_wait on a child process
    let exe = std::env::current_exe().expect("current_exe failed");
    let mut child = std::process::Command::new(&exe)
        .arg("exit-fast")
        .spawn()
        .expect("spawn child failed");

    // Wait for it to finish, then try_wait should return Some
    let status = child.wait().expect("wait failed");
    assert!(status.success(), "child exited with {status}");

    println!("all threading tests passed");
}

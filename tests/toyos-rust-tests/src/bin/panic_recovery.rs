use std::process::Command;

fn main() {
    test_syscall_panic();
    test_syscall_fault();
    test_user_segfault();
    test_system_alive();
    println!("all panic recovery tests passed");
}

/// Kernel panic!() during syscall → process killed, system survives.
fn test_syscall_panic() {
    let status = Command::new("/bin/test_rs_test_panic_child")
        .arg("0")
        .status()
        .expect("failed to spawn child");
    assert!(!status.success(), "child that triggers kernel panic should be killed");
    println!("  PASS: syscall panic killed process (exit={})", status.code().unwrap_or(-1));
}

/// Kernel null-pointer fault during syscall → process killed, system survives.
fn test_syscall_fault() {
    let status = Command::new("/bin/test_rs_test_panic_child")
        .arg("1")
        .status()
        .expect("failed to spawn child");
    assert!(!status.success(), "child that triggers kernel fault should be killed");
    println!("  PASS: syscall fault killed process (exit={})", status.code().unwrap_or(-1));
}

/// User-mode segfault → process killed, system survives.
fn test_user_segfault() {
    let status = Command::new("/bin/test_rs_segfault_child")
        .status()
        .expect("failed to spawn child");
    assert!(!status.success(), "child that segfaults should be killed");
    println!("  PASS: user segfault killed process (exit={})", status.code().unwrap_or(-1));
}

/// System still works after all three fault types.
fn test_system_alive() {
    let output = Command::new("/bin/echo")
        .arg("still alive")
        .output()
        .expect("failed to run echo after recoveries");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "still alive");
    println!("  PASS: system alive after panic + fault + segfault recovery");
}

use std::process::Command;

fn main() {
    test_syscall_panic();
    test_syscall_fault();
    test_lock_across_switch();
    test_lock_across_switch_is_one_shot();
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

/// A kernel spinlock held across a scheduler entry → spec §6.4's baseline
/// assert fires at the call site instead of the pass parking with the lock on a
/// stack nothing returns to. Without the assert the syscall returns normally and
/// the child exits 0, so this case has teeth in the negative direction too.
fn test_lock_across_switch() {
    let status = Command::new("/bin/test_rs_test_panic_child")
        .arg("2")
        .status()
        .expect("failed to spawn child");
    assert!(!status.success(), "child that yields under a kernel lock should be killed");
    println!("  PASS: lock-across-switch tripwire killed process (exit={})", status.code().unwrap_or(-1));
}

/// The trip above dies holding the kernel lock it took, and the kernel does not
/// unwind, so nothing will ever release it. A second call must be refused: it
/// would otherwise spin `Lock::lock` to its 500M-spin deadline with interrupts
/// masked and preemption disabled, freezing a single-CPU machine for the whole
/// window — from an ungated syscall. Refused means the child survives.
fn test_lock_across_switch_is_one_shot() {
    let status = Command::new("/bin/test_rs_test_panic_child")
        .arg("2")
        .status()
        .expect("failed to spawn child");
    assert!(status.success(), "second lock-across-switch call must be refused, not honoured");
    println!("  PASS: second lock-across-switch call refused (exit={})", status.code().unwrap_or(-1));
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

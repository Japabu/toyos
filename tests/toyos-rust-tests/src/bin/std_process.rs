use std::process::Command;

fn main() {
    test_status();
    test_output();
    test_exit_code();
    test_spawn_and_wait();
    test_piped_stdin();
    println!("all process tests passed");
}

fn test_status() {
    let status = Command::new("/bin/echo")
        .arg("hi")
        .status()
        .expect("failed to run echo");
    assert!(status.success(), "echo should succeed");
    println!("  Command::status(): ok");
}

fn test_output() {
    let output = Command::new("/bin/echo")
        .arg("hello world")
        .output()
        .expect("failed to run echo");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "hello world");
    println!("  Command::output(): ok");
}

fn test_exit_code() {
    let status = Command::new("/bin/cat")
        .arg("/nonexistent_file")
        .status()
        .expect("failed to run cat");
    assert!(!status.success(), "cat nonexistent file should fail");
    println!("  exit code (failure): ok");
}

fn test_spawn_and_wait() {
    let mut child = Command::new("/bin/echo")
        .arg("spawned")
        .spawn()
        .expect("failed to spawn echo");
    let status = child.wait().expect("failed to wait");
    assert!(status.success());
    println!("  spawn + wait: ok");
}

fn test_piped_stdin() {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn cat");

    child.stdin.take().unwrap().write_all(b"piped input\n").unwrap();
    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "piped input");
    println!("  piped stdin/stdout: ok");
}

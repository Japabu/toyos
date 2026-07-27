use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("echo") => subcmd_echo(),
        Some("repl") => subcmd_repl(),
        _ => run_tests(),
    }
}

fn run_tests() {
    test_status();
    test_output();
    test_piped_stdout();
    test_piped_stdin_stdout();
    test_interactive();

    println!("all tests passed");
}

fn subcmd_echo() {
    std::os::toyos::io::set_stdin_raw(true);
    let mut buf = Vec::new();
    std::io::stdin().read_to_end(&mut buf).unwrap();
    std::io::stdout().write_all(&buf).unwrap();
}

fn subcmd_repl() {
    std::os::toyos::io::set_stdin_raw(true);
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.unwrap();
        let (cmd, arg) = line.split_once(' ').unwrap_or((&line, ""));
        match cmd {
            "echo" => println!("you said {}", arg),
            "exit" => {
                println!("exiting");
                return;
            }
            _ => println!("unknown command: {}", cmd),
        }
    }
}

fn test_status() {
    print!("test Command::status()... ");
    let status = Command::new("/bin/echo")
        .arg("hi")
        .status()
        .expect("failed to run echo");
    assert!(status.success(), "echo exited with failure");
    println!("ok");
}

fn test_output() {
    print!("test Command::output()... ");
    let output = Command::new("/bin/echo")
        .args(&["hello", "world"])
        .output()
        .expect("failed to run echo");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello world"), "unexpected output: {stdout:?}");
    println!("ok (got: {})", stdout.trim());
}

fn test_piped_stdout() {
    print!("test spawn() with piped stdout... ");
    let mut child = Command::new("/bin/echo")
        .args(&["piped", "test"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn echo");
    let mut output = String::new();
    child.stdout.take().unwrap().read_to_string(&mut output).unwrap();
    let status = child.wait().expect("failed to wait");
    assert!(status.success());
    assert!(output.contains("piped test"), "unexpected output: {output:?}");
    println!("ok (got: {})", output.trim());
}

fn test_piped_stdin_stdout() {
    print!("test spawn() with piped stdin+stdout... ");
    let mut child = Command::new("/bin/proctest")
        .arg("echo")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn proctest echo");

    let msg = "hello from parent\n";
    child.stdin.take().unwrap().write_all(msg.as_bytes()).unwrap();

    let mut output = String::new();
    child.stdout.take().unwrap().read_to_string(&mut output).unwrap();
    let status = child.wait().expect("failed to wait");
    assert!(status.success());
    assert_eq!(output, msg, "echo mismatch: {output:?}");
    println!("ok (echoed: {})", output.trim());
}

fn test_interactive() {
    print!("test interactive subprocess... ");
    let mut child = Command::new("/bin/proctest")
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn proctest repl");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();

    stdin.write_all(b"echo hello\n").unwrap();
    line.clear();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line, "you said hello\n", "unexpected: {line:?}");

    stdin.write_all(b"echo world\n").unwrap();
    line.clear();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line, "you said world\n", "unexpected: {line:?}");

    stdin.write_all(b"exit\n").unwrap();
    line.clear();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line, "exiting\n", "unexpected: {line:?}");

    let status = child.wait().expect("failed to wait");
    assert!(status.success());
    println!("ok");
}

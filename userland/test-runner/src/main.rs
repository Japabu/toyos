use std::io::{self, BufRead, Write};
use std::process::{Command, Stdio};

fn main() {
    println!("===READY===");
    let _ = io::stdout().flush();

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let cmd = line.trim().to_string();
        if cmd.is_empty() {
            continue;
        }

        if cmd == "quit" {
            std::process::exit(0);
        }

        let Some(name) = cmd.strip_prefix("run ") else {
            eprintln!("unknown command: {cmd}");
            continue;
        };
        let name = name.trim();
        let path = format!("/bin/{name}");

        println!("===TEST_START {name}===");
        let _ = io::stdout().flush();

        // Spawn with piped stdin (so child doesn't consume serial commands)
        // but inherited stdout/stderr (output goes directly to serial).
        match Command::new(&path).stdin(Stdio::piped()).spawn() {
            Ok(mut child) => {
                // Drop stdin pipe so child gets EOF if it tries to read
                drop(child.stdin.take());
                match child.wait() {
                    Ok(status) => {
                        let code = status.code().unwrap_or(-1);
                        println!("===TEST_END {name} exit={code}===");
                    }
                    Err(e) => println!("===TEST_END {name} error={e}==="),
                }
            }
            Err(e) => println!("===TEST_END {name} error={e}==="),
        }
        let _ = io::stdout().flush();
    }
}

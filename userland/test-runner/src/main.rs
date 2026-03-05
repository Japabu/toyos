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
        let path = format!("/initrd/{name}");

        println!("===TEST_START {name}===");
        let _ = io::stdout().flush();

        match Command::new(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => match child.wait_with_output() {
                Ok(output) => {
                    if !output.stdout.is_empty() {
                        io::stdout().write_all(&output.stdout).ok();
                        let _ = io::stdout().flush();
                    }
                    let code = output.status.code().unwrap_or(-1);
                    println!("===TEST_END {name} exit={code}===");
                }
                Err(e) => println!("===TEST_END {name} error={e}==="),
            },
            Err(e) => println!("===TEST_END {name} error={e}==="),
        }
        let _ = io::stdout().flush();
    }
}

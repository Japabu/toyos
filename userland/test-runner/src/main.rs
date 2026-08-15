mod log_gate;

use std::io::{self, BufRead, Write};
use std::os::toyos::process::CommandExt;
use std::process::{Command, Stdio};

use toyos::endow::{Endowments, SYSCAP_LABEL};
use toyos::syscap::SysCap;

/// Tests that run **inside** this process rather than in a binary it spawns.
///
/// **Not a shortcut: a spawned binary cannot hold what these need.** This
/// process passes its whole namespace to every child, and a `SysCap` dup is not
/// a namespace entry — so a gate whose subject is a right on this program's own
/// capability has nowhere else to run (`specs/capability-endowment-spec.md`
/// §6.7a). They answer the same `===TEST_START===`/`===TEST_END===` protocol as
/// a binary, so the host cannot tell the difference and does not have to.
const BUILTINS: &[(&str, fn(Option<&SysCap>) -> i32)] = &[("log-gate", log_gate::run)];

fn main() {
    // **The test estate's authority, and the one place least authority is not
    // enforced** (`specs/capability-endowment-spec.md` §4). The guest
    // binaries are not `[programs]` keys, so no manifest row can name what any
    // of them holds: a test binary holds what test-runner holds. The namespace
    // travels by inheritance; this capability is handed over explicitly, as a
    // duplicate rather than the cap itself, because one boot runs several
    // binaries that each need the keyboard and a device claim moves.
    let cap: Option<SysCap> = Endowments::get().take(SYSCAP_LABEL);

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
        // `run <name> [args...]`: the markers still carry only the binary
        // name, so the host protocol is unchanged for the argument-less case.
        let mut words = name.split_whitespace();
        let Some(name) = words.next() else { continue };
        let args: Vec<&str> = words.collect();
        let path = format!("/bin/{name}");

        println!("===TEST_START {name}===");
        let _ = io::stdout().flush();

        if let Some((_, builtin)) = BUILTINS.iter().find(|(n, _)| *n == name) {
            let code = builtin(cap.as_ref());
            println!("===TEST_END {name} exit={code}===");
            let _ = io::stdout().flush();
            continue;
        }

        // Spawn with piped stdin (so child doesn't consume serial commands)
        // but inherited stdout/stderr (output goes directly to serial).
        let mut command = Command::new(&path);
        command.args(&args).stdin(Stdio::piped());
        // **A refused dup is an answer and not a failure.** `duplicate` needs
        // `DUP` on the capability, which a manifest grants by name — so a cap
        // without it is one this program holds *for itself*, and the child gets
        // the namespace and no capability at all. `logread` is exactly such a
        // cap, as `realtime` is: the estate does not hand either down
        // (`specs/capability-endowment-spec.md` §6.7a). The `expect` here
        // assumed every cap was dup-able and took the whole boot down on the
        // first config that endowed one without `dup`.
        if let Some(dup) = cap.as_ref().and_then(|cap| cap.duplicate().ok()) {
            command.endow(SYSCAP_LABEL, dup.into_raw().0);
        }
        match command.spawn() {
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

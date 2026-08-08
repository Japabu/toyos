//! A CPU exception raised by a Ring 3 process kills that process and leaves the
//! machine running.
//!
//! A vector with no IDT gate does not fault the process. The CPU takes the
//! absent gate as a second, contributory fault and escalates to #DF, and
//! `double_fault_handler` halts every CPU — so before the gates went in, the
//! first arm below took the whole guest down and this test timed out.
//!
//! **The loop is the assertion.** Arm N+1 can only run because the machine
//! survived arm N, and the `echo` at the end proves a fresh process still
//! starts. That is why the arms this environment cannot raise are still worth
//! spawning: they cost 10 ms each, and the day one of them starts faulting is
//! the day its gate has to be there.

use std::process::Command;

enum Expect {
    /// Measured: the CPU raises this from Ring 3 here, so the child must die.
    Killed,
    /// Measured: it does not, and the reason is in `fault_gate_child`. The exit
    /// code is not asserted — a kernel or an emulator that starts raising one
    /// of these must not red a test whose subject is the gate, and the gate is
    /// still covered, because an escalation would hang the machine instead.
    NotRaisedHere,
}

const ARMS: &[(&str, Expect)] = &[
    ("de", Expect::Killed),
    ("de_overflow", Expect::Killed),
    // Both #SS routes arrive as #GP under TCG; see the child.
    ("ss", Expect::Killed),
    ("ss_rsp", Expect::Killed),
    ("mf", Expect::Killed),
    ("xm", Expect::NotRaisedHere),
    ("ac", Expect::NotRaisedHere),
];

fn main() {
    for (kind, expect) in ARMS {
        let status = Command::new("/bin/test_rs_fault_gate_child")
            .arg(kind)
            .status()
            .unwrap_or_else(|e| panic!("failed to spawn fault_gate_child {kind}: {e}"));
        match expect {
            Expect::Killed => assert!(
                !status.success(),
                "{kind} left Ring 3 alive: the CPU did not raise it, or the kernel resumed a \
                 fault it should have killed for (exit {:?})",
                status.code(),
            ),
            Expect::NotRaisedHere => {}
        }
        println!("  {kind}: killed={}", !status.success());
    }

    let out = Command::new("/bin/echo")
        .arg("still alive")
        .output()
        .expect("failed to spawn a process after the fault arms");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "still alive",
        "the machine survived every fault but can no longer start a process",
    );
    println!("every Ring 3 fault killed its process and left the machine up");
}

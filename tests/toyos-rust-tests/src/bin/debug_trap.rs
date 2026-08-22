//! `#DB` raised from Ring 3, which is a userland bug and not a debugger session.
//!
//! **A Ring 3 process can raise `#DB` whenever it likes, and it needs no
//! privilege to do it.** `RFLAGS.TF` is not a privileged bit — a `popfq` sets
//! it, and the instruction after that traps — and `INT1` (opcode `0xF1`) raises
//! the vector directly, without the DPL check `INT n` performs against the gate.
//! So both arms below are ordinary userland instruction sequences, and both are
//! things a program can do by accident.
//!
//! Neither may reach a kernel report path. `#DB`'s handler used to be a
//! debugger-session aid — a marker straight out the UART, a `DR7`/`DR6` disarm,
//! `DR0` read back, a symbol resolved and a backtrace walked — and it *returned
//! to resume*, so a Ring 3 trap made the kernel walk kernel state on a user
//! fault and then carry on. With `TF` still set the resumed instruction traps
//! again, which is a report per instruction for as long as the process runs.
//!
//! This is the gate on the answer: `#DB` from Ring 3 is the process's fault and
//! ends it, exactly as `#BP` and `#UD` from Ring 3 do. The parent asserts the
//! child reached its trap and never spoke again; `check_debug_trap` in
//! `tests/toyos.rs` asserts the *kernel's* half out of the serial capture — that
//! it named the vector in a Ring 3 report, and that the watchpoint report is not
//! in the capture at all.

use std::io::Write;
use std::process::{Command, Stdio};

const SELF_PATH: &str = "/bin/test_rs_debug_trap";

/// The two ways Ring 3 reaches vector 1, and what each is.
const ARMS: &[(&str, &str)] = &[
    ("int1", "the INT1 instruction, which raises #DB with no DPL check on the gate"),
    ("single-step", "RFLAGS.TF, set by an ordinary Ring 3 popfq"),
    ("tf-syscall", "RFLAGS.TF across a syscall, where IA32_FMASK decides who takes the trap"),
];

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some(role) => raise(role),
        None => test(),
    }
}

fn test() {
    for (role, what) in ARMS {
        dies(role, what);
    }
    still_alive();
    println!("both Ring 3 routes to #DB end the process, and the machine is still here");
}

/// Spawn one arm and require the kernel to have ended it at the trap.
///
/// **The marker is what gives this teeth.** Without it a child that died before
/// reaching its instruction sequence would pass and the arm would assert
/// nothing; with it, a kernel that resumes the trap prints `SURVIVED` and reds
/// on the line rather than on the exit code, which says *which* half broke.
fn dies(role: &str, what: &str) {
    let child = Command::new(SELF_PATH)
        .arg(role)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {role}: {e}"));
    let out = child.wait_with_output().unwrap_or_else(|e| panic!("wait {role}: {e}"));
    let said = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        said.trim(),
        format!("armed {role}"),
        "{role} ({what}) never reached its trap, or the kernel resumed it",
    );
    assert!(
        !out.status.success(),
        "{role} ({what}) did not end the process (exit {:?})",
        out.status.code(),
    );
    println!("  {role}: killed at the trap, exit {:?}", out.status.code());
}

/// The other half of both refusals: the kernel is unharmed by a trap it
/// delivered. A `#DB` that took the machine down — or one whose handler resumed
/// into a `TF` storm — fails here rather than above.
fn still_alive() {
    let out = Command::new("/bin/echo")
        .arg("still alive")
        .output()
        .expect("run echo after two debug traps");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "still alive");
    println!("  the kernel is still running after two Ring 3 debug traps");
}

fn raise(role: &str) -> ! {
    // Printed and flushed before the trap, so the parent can tell "the kernel
    // ended it here" from "it never got here".
    println!("armed {role}");
    std::io::stdout().flush().expect("flush the marker");
    match role {
        // `INT1`, not `int 1`. The two differ where it matters: `INT n` is a
        // software interrupt and checks the gate's DPL against CPL, so `int 1`
        // from Ring 3 is refused against a DPL 0 gate and never reaches vector
        // 1 at all. `INT1` generates the exception itself and is not subject to
        // that check (SDM Vol. 2A, INT n/INTO/INT3/INT1).
        //
        // **Written as its opcode, because no spelling of it assembles.** This
        // toolchain's LLVM rejects `int1` and `icebp` alike — measured, both
        // `invalid instruction mnemonic` — and `.byte 0xf1` is the encoding
        // itself, which is the one form that cannot be the wrong instruction.
        "int1" => {
            // SAFETY: one instruction that raises vector 1 and touches no
            // memory. The whole point is that it traps; where the kernel is
            // correct nothing after it runs.
            unsafe { core::arch::asm!(".byte 0xf1") };
            println!("SURVIVED int1");
        }
        // `TF` set the way a Ring 3 program can set it. The trap is taken after
        // the instruction *following* the `popfq` — the architecture defers it
        // by one so that a `popfq` which sets `TF` does not trap on itself —
        // which is what the `nop` is for.
        "single-step" => {
            // SAFETY: a balanced `pushfq`/`popfq` around one `or` on the word it
            // pushed, which is `fault_gate_child::alignment_check`'s sequence
            // with `TF` in place of `AC`. No `nostack`, because the pair uses
            // the stack. Where the kernel is correct the `nop` never retires.
            unsafe {
                core::arch::asm!(
                    "pushfq",
                    "or qword ptr [rsp], 0x100",
                    "popfq",
                    "nop",
                );
            }
            println!("SURVIVED single-step");
        }
        // The same `TF`, with the deferred instruction being a *syscall*.
        //
        // **This is the arm that decides who takes the trap.** `SYSCALL` clears
        // exactly what `IA32_FMASK` names and nothing else, so a kernel whose
        // mask leaves `TF` alone enters Ring 0 single-stepping and takes the
        // `#DB` at `LSTAR` — a Ring 0 trap with a Ring 3 cause, which a
        // userland program raises by writing one bit. `arch::syscall::init` is
        // where the mask is declared and where that has to be answered.
        "tf-syscall" => {
            let pid: u64;
            // SAFETY: a balanced `pushfq`/`popfq` around one `or`, then the ABI's
            // own `syscall` sequence for `SYS_GETPID` (51) — number in `rdi`,
            // result in `rax`, `rcx` and `r11` clobbered by the instruction. The
            // syscall has to be the instruction *immediately* after the `popfq`,
            // which is why it is written here rather than called: the trap is
            // deferred by exactly one instruction and that instruction is the
            // subject.
            unsafe {
                core::arch::asm!(
                    "pushfq",
                    "or qword ptr [rsp], 0x100",
                    "popfq",
                    "syscall",
                    in("rdi") 51u64,
                    lateout("rax") pid,
                    out("rcx") _,
                    out("r11") _,
                );
            }
            println!("SURVIVED tf-syscall, getpid answered {pid}");
        }
        other => panic!("unknown role {other:?}"),
    }
    std::process::exit(0)
}

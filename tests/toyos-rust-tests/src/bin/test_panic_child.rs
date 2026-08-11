//! Ask the kernel for one `SYS_DEBUG` action, and report what came back.
//!
//! Every action this is driven with kills the caller, except action 9 — so
//! reaching the end at all is a finding, and *which* finding is the whole of
//! what it prints. `InvalidArgument` is the kernel saying it has no debug
//! syscall: the boot needs `test-actuators` and asked for nothing, which is a
//! harness mistake and not a kernel that failed to kill anybody.
//!
//! It exits 0 in both cases on purpose. Every caller asserts that the child did
//! *not* succeed, so a return is a red wherever it happens; the message is what
//! tells the two apart.

use toyos_abi::syscall::SyscallError;

fn main() {
    let action: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let rc = toyos_abi::syscall::debug(action);
    if rc == SyscallError::InvalidArgument.to_u64() {
        eprintln!(
            "ERROR: SYS_DEBUG {action} answered InvalidArgument — this kernel carries no \
             actuators, so the boot that drives this one needs `test-actuators`"
        );
    } else {
        eprintln!("ERROR: SYS_DEBUG {action} returned {rc:#x}, kernel did not kill the process");
    }
    std::process::exit(0);
}

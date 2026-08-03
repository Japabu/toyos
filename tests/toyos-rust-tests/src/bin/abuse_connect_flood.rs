//! A connect is an allocation request, and the kernel has to be able to say no.
//!
//! `sys_connect` hands the server's ends of two pipes to the listener's
//! pending queue. The queue had no depth, and the `bool` `push_connection`
//! returned to report failure was discarded at the call site. So a process
//! that listens on a name of its own — `SYS_LISTEN` is ungated, so it never
//! has to find a real service to abuse — and then connects to itself without
//! ever accepting queued connections until `pmm::alloc_page` returned `None`
//! and `pipe::create`'s `.expect` took the kernel down with it.
//!
//! Fail-fast is for kernel bugs, not for untrusted input: this has to be a
//! `ResourceExhausted`, and the machine has to still be running afterwards.
//!
//! The other half, which the depth alone does not give: each of those calls
//! built two 2 MiB rings *eagerly*, so the allowance the kernel grants a
//! server that has not answered yet cost 128 MiB of physical memory before a
//! single byte had been sent. A pipe now allocates its page on first use, and
//! `a_queued_connection_pins_no_memory` is what says so.

use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::Fd;

const NAME: &str = "abuse-connect-flood";
/// Comfortably past the kernel's burst allowance, and far short of `MAX_FDS`.
const ATTEMPTS: usize = 200;

/// Physical bytes in use, from `SYS_SYSINFO`'s header.
fn used_bytes() -> u64 {
    let mut buf = [0u8; 48];
    let n = toyos_abi::syscall::sysinfo(&mut buf);
    assert_eq!(n, buf.len(), "SYS_SYSINFO would not fill its header");
    u64::from_le_bytes(buf[8..16].try_into().unwrap())
}

fn main() {
    let listener = syscall::listen(NAME).expect("listen on a name of our own");

    let before = used_bytes();
    let mut held: Vec<Fd> = Vec::new();
    let mut refused_after = None;
    for i in 0..ATTEMPTS {
        match syscall::connect(NAME) {
            Ok(fd) => held.push(fd),
            Err(SyscallError::ResourceExhausted) => {
                refused_after = Some(i);
                break;
            }
            Err(e) => panic!("connect {i}: unexpected {e:?}"),
        }
    }
    let after = used_bytes();

    let depth = refused_after.unwrap_or_else(|| {
        panic!(
            "{ATTEMPTS} connections queued with nothing accepting them and the kernel \
             never refused one — that is {} MiB pinned by one process",
            ATTEMPTS * 4
        )
    });

    // The exact allowance, not a range: a silent change to it should be a
    // decision someone makes on purpose. Tracks `listener::MAX_PENDING_CONNECTIONS`.
    assert_eq!(depth, 32, "the burst allowance moved");

    // What the queued connections cost. Eagerly allocated, `depth` of them is
    // `depth * 4 MiB`; lazily, it is the queue entries and nothing else. The
    // ceiling is a quarter of the eager figure, which no amount of ordinary
    // boot noise reaches and no eager allocation can stay under.
    let grew = after.saturating_sub(before);
    let eager = depth as u64 * 4 * 1024 * 1024;
    assert!(
        grew < eager / 4,
        "{depth} connections nobody accepted grew physical memory by {} MiB; \
         allocated eagerly they would cost {} MiB, and lazily they cost the queue",
        grew / (1024 * 1024),
        eager / (1024 * 1024)
    );
    println!("  {depth} unaccepted connections cost {} KiB of physical memory", grew / 1024);

    // A depth, not a permanent refusal: draining one must let one more in.
    let accepted = syscall::accept(listener).expect("accept a queued connection");

    // The other direction, without which "costs nothing" could be a pipe that
    // never works: one byte on a live connection has to buy a ring. This is
    // the negative control for the ceiling above — a kernel that allocated
    // nothing ever would pass that assertion and fail this one.
    let before_write = used_bytes();
    let wrote = syscall::write(accepted.fd, b"x").expect("write one byte to an accepted socket");
    assert_eq!(wrote, 1);
    let ring = used_bytes().saturating_sub(before_write);
    assert!(
        ring >= 2 * 1024 * 1024,
        "the first write to a connection grew physical memory by {ring} bytes; a ring is 2 MiB"
    );
    println!("  the first write on a connection allocated {} KiB", ring / 1024);

    syscall::close(accepted.fd);
    let fd = syscall::connect(NAME).expect("a connect must succeed once the server drains one");
    held.push(fd);

    for fd in held {
        syscall::close(fd);
    }
    syscall::close(listener);

    println!("connect refused after {depth} unaccepted, and resumed once one was drained");
}

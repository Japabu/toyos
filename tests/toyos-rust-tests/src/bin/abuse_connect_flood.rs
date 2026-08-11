//! A connect is an allocation request, and the kernel has to be able to say no.
//!
//! Opening a connection hands the server's ends of two pipes to the port's
//! pending queue. The queue had no depth, and the `bool` `push_connection`
//! returned to report failure was discarded at the call site — so a process
//! could queue connections nobody accepted until `pmm::alloc_page` returned
//! `None` and `pipe::create`'s `.expect` took the kernel down with it.
//!
//! Fail-fast is for kernel bugs, not for untrusted input: this has to be a
//! `ResourceExhausted`, and the machine has to still be running afterwards.
//!
//! The other half, which the depth alone does not give: each of those calls
//! built two 2 MiB rings *eagerly*, so the allowance the kernel grants a
//! server that has not answered yet cost 128 MiB of physical memory before a
//! single byte had been sent. A pipe now allocates its page on first use, and
//! the memory assertion below is what says so.
//!
//! **The attack's "be your own service" clause is dead and this is now a bound
//! check.** It used to `SYS_LISTEN` on a name of its own, because listening was
//! ungated and any process could take any name; there is no name registry left,
//! so the flood is against a port this process created and holds both ends of.
//! A process given only a connector can still do exactly this to a real
//! service, which is what the depth is for.

use toyos::port;
use toyos::AsHandle;
use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::RawHandle;

/// Comfortably past the kernel's burst allowance, and far short of the handle
/// table's cap.
const ATTEMPTS: usize = 200;

/// The name this process's own namespace maps to its own port.
const NAME: &str = "abuse-connect-flood";

/// Physical bytes in use, from `SYS_SYSINFO`'s header.
fn used_bytes() -> u64 {
    let mut buf = [0u8; 48];
    let n = toyos_abi::syscall::sysinfo(&mut buf);
    assert_eq!(n, buf.len(), "SYS_SYSINFO would not fill its header");
    u64::from_le_bytes(buf[8..16].try_into().unwrap())
}

fn main() {
    let (acceptor, connector) = port::create().expect("a port of our own");
    // One namespace holding that connector, which is the only way to open a
    // connection at all: a name resolves in a namespace this process holds and
    // nowhere else.
    let ns = toyos::namespace::build()
        .add(NAME, &connector)
        .finish()
        .expect("a namespace naming our own port");

    let before = used_bytes();
    let mut held: Vec<RawHandle> = Vec::new();
    let mut refused_after = None;
    for i in 0..ATTEMPTS {
        match syscall::namespace_open(ns.as_handle(), NAME) {
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
    // decision someone makes on purpose. Tracks `MAX_PENDING_CONNECTIONS`.
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
    let accepted = syscall::accept(acceptor.as_handle()).expect("accept a queued connection");

    // The other direction, without which "costs nothing" could be a pipe that
    // never works: one byte on a live connection has to buy a ring. This is
    // the negative control for the ceiling above — a kernel that allocated
    // nothing ever would pass that assertion and fail this one.
    let before_write = used_bytes();
    let wrote = syscall::write(accepted, b"x").expect("write one byte to an accepted socket");
    assert_eq!(wrote, 1);
    let ring = used_bytes().saturating_sub(before_write);
    assert!(
        ring >= 2 * 1024 * 1024,
        "the first write to a connection grew physical memory by {ring} bytes; a ring is 2 MiB"
    );
    println!("  the first write on a connection allocated {} KiB", ring / 1024);

    syscall::close(accepted);
    let fd = syscall::namespace_open(ns.as_handle(), NAME)
        .expect("an open must succeed once the server drains one");
    held.push(fd);

    for fd in held {
        syscall::close(fd);
    }

    println!("connect refused after {depth} unaccepted, and resumed once one was drained");
}

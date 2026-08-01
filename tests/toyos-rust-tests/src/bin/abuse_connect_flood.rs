//! A connect is an allocation request, and the kernel has to be able to say no.
//!
//! `sys_connect` builds two 2 MiB rings per call and hands the server's ends
//! to the listener's pending queue. The queue had no depth, and the `bool`
//! `push_connection` returned to report failure was discarded at the call
//! site. So a process that listens on a name of its own — `SYS_LISTEN` is
//! ungated, so it never has to find a real service to abuse — and then
//! connects to itself without ever accepting pinned 4 MiB per call until
//! `pmm::alloc_page` returned `None` and `pipe::create`'s `.expect` took the
//! kernel down with it.
//!
//! Fail-fast is for kernel bugs, not for untrusted input: this has to be a
//! `ResourceExhausted`, and the machine has to still be running afterwards.

use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::Fd;

const NAME: &str = "abuse-connect-flood";
/// Comfortably past the kernel's burst allowance, and far short of `MAX_FDS`.
const ATTEMPTS: usize = 200;

fn main() {
    let listener = syscall::listen(NAME).expect("listen on a name of our own");

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

    // A depth, not a permanent refusal: draining one must let one more in.
    let accepted = syscall::accept(listener).expect("accept a queued connection");
    syscall::close(accepted.fd);
    let fd = syscall::connect(NAME).expect("a connect must succeed once the server drains one");
    held.push(fd);

    for fd in held {
        syscall::close(fd);
    }
    syscall::close(listener);

    println!("connect refused after {depth} unaccepted, and resumed once one was drained");
}

//! The window `issues/kernel/ring0-jump-to-zero-under-port-polls.md`
//! names: the last `Acceptor` handle going while an `io_uring` poll still
//! references the port.
//!
//! A poll on an acceptor is the one registration that holds its object rather
//! than naming it by number — `Source::Port(Arc<PortShared>)` — so the order in
//! which the acceptor handle and the ring go away decides which of them runs
//! the teardown. Both orders are here, from two threads at once, because the
//! fault this was written for was seen once in a boot with two of them live and
//! has never been seen alone.
//!
//! It is a churn test, not a property test: every arm asserts what it can, but
//! what it is really watching for is a kernel that stops.

use std::thread;

use toyos::poller::{Poller, IORING_POLL_IN};
use toyos::{namespace, port};
use toyos_abi::syscall;

/// Rounds per arm per thread. A round is a port, a ring and a registration
/// created and destroyed, and a ring costs a 2 MiB page the kernel zeroes — so
/// this is bounded by that rather than by how many times the window is worth
/// entering.
const ROUNDS: usize = 250;

fn main() {
    let a = thread::spawn(|| churn("A"));
    let b = thread::spawn(|| churn("B"));
    a.join().expect("thread A");
    b.join().expect("thread B");

    shared_port_race();

    println!("all port_poll_churn tests passed");
}

/// Arm a poll on an acceptor and then take away the two things it references,
/// in both orders and with and without a connection queued behind it.
fn churn(label: &str) {
    for i in 0..ROUNDS {
        acceptor_then_ring(i % 2 == 0);
        ring_then_acceptor();
    }
    println!("  churn {label}: {ROUNDS} rounds of each order: ok");
}

/// Close the acceptor with the poll armed, then drop the ring.
///
/// The close is what cancels the poll, so the ring's own teardown finds the
/// registration already gone.
fn acceptor_then_ring(queue_a_connection: bool) {
    let (acceptor, connector) = port::create().expect("a port");
    let poller = Poller::new(1);
    let acc = acceptor.into_raw();
    poller.poll_add_fd(acc, IORING_POLL_IN, 0);
    // Hand the submission to the kernel without waiting for it: nothing has
    // connected, so the poll stays armed on the other side of this call.
    poller.wait(0, 0, |_| {});

    // A connection completes the poll, so this arm is the one where the
    // acceptor's zero-handle hook has a queued connection to drop — the
    // client's end is still open when it does.
    let client = queue_a_connection.then(|| {
        let ns = namespace::build().add("p", &connector).finish().expect("a namespace");
        ns.open("p").expect("the port is open and its acceptor is alive")
    });

    syscall::close(acc);
    drop(poller);
    drop(client);
    drop(connector);
}

/// Drop the ring with the poll armed, then close the acceptor.
///
/// Here the ring's teardown is what unregisters, and it runs against a port
/// that is still alive.
fn ring_then_acceptor() {
    let (acceptor, connector) = port::create().expect("a port");
    let acc = acceptor.into_raw();
    {
        let poller = Poller::new(1);
        poller.poll_add_fd(acc, IORING_POLL_IN, 0);
        poller.wait(0, 0, |_| {});
    }
    syscall::close(acc);
    drop(connector);
}

/// Two rings watching one port, and one of the two acceptor handles closing
/// under the other's live registration.
///
/// The single-port arms above never have a second watcher, so nothing in them
/// can tell a watcher list cleaned per registration from one cleaned per ring.
/// This can. The close cancels the survivor's poll too — a cancellation is a
/// `-NotFound` completion, and the caller's contract is to look at the handle
/// again — so the property with teeth is the *next* round: the port is still
/// alive through `first`, and a poll armed on it after the sibling's close must
/// still complete when a client connects.
fn shared_port_race() {
    for _ in 0..32 {
        let (acceptor, connector) = port::create().expect("a port");
        let first = acceptor.into_raw();
        let second = syscall::dup(first).expect("an acceptor carries DUP");

        let watcher = Poller::new(1);
        watcher.poll_add_fd(first, IORING_POLL_IN, 0);
        watcher.wait(0, 0, |_| {});

        // A second ring registers on the same `PortShared` and then goes away
        // with its handle, which is where a per-source watcher list with no
        // count loses the first ring's registration.
        {
            let transient = Poller::new(1);
            transient.poll_add_fd(second, IORING_POLL_IN, 0);
            transient.wait(0, 0, |_| {});
        }
        syscall::close(second);

        // Drain whatever the close posted, so what the assertion below sees is
        // the connection and cannot be the cancellation.
        watcher.wait(0, 0, |_| {});
        watcher.poll_add_fd(first, IORING_POLL_IN, 1);

        let ns = namespace::build().add("p", &connector).finish().expect("a namespace");
        let client = ns.open("p").expect("open");

        let mut fired = false;
        watcher.wait(1, 500_000_000, |token| fired |= token == 1);
        assert!(
            fired,
            "a poll armed after a sibling handle's close never fired: the port \
             is still alive and a client connected through it",
        );

        drop(client);
        syscall::close(first);
        drop(watcher);
        drop(connector);
    }
    println!("  shared port, two rings, one handle closed: ok");
}

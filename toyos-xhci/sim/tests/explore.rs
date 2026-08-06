//! Sequences nobody chose.
//!
//! The staged scenarios cover what a person does to a port. This covers what a
//! marginal cable does: attach, detach and replug at instants no test author
//! would pick, with the invariants checked after every step and two properties
//! checked after every sequence.
//!
//! Seeded and reported, so a failure is a number that reproduces it.

use rand::{Rng, SeedableRng};

use toyos_xhci::port::{DEBOUNCE_NS, RESET_DEADLINE_NS};
use toyos_xhci_sim::driver::{Did, Driver};
use toyos_xhci_sim::hub::{FakePort, ResetBehaviour};

const PASS: u64 = 1_000_000;

/// How many sequences one run explores, and how long each is. Small enough to
/// stay inside a `cargo test` nobody is waiting for.
const SEQUENCES: u64 = 2_000;
const EVENTS: usize = 24;

/// One sequence, replayable from its seed.
fn explore(seed: u64) -> Result<(), String> {
    let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
    let behaviour = if rng.random_bool(0.15) {
        ResetBehaviour::Never
    } else {
        ResetBehaviour::Completes { after: rng.random_range(1..=(4 * PASS)) }
    };
    let mut port = FakePort::empty(behaviour);
    let mut driver = Driver::new();
    let mut now: u64 = 0;

    for _ in 0..EVENTS {
        match rng.random_range(0..4) {
            0 => port.attach(),
            1 => port.detach(),
            2 => port.replug(),
            _ => {}
        }
        // Anything from a scheduler pass to well past the reset deadline, so
        // both "the driver looks again immediately" and "nothing looks for
        // seconds" are covered.
        let gap = rng.random_range(1..=(DEBOUNCE_NS + RESET_DEADLINE_NS / 2));
        let until = now + gap;
        driver
            .run_to(&mut port, now, until, PASS)
            .map_err(|e| format!("seed {seed}: {e:?} after {:?}", driver.did))?;
        now = until;
    }

    // **Balance.** The driver takes a port on and gives it back alternately,
    // never twice in a row either way — so it never holds two slots for one
    // port and never leaves one bound to a device that has gone.
    //
    // Giving up counts as taking it on. That is not a technicality: the refusal
    // deliberately marks the port attached so it is not reset again every pass,
    // and the teardown when the device is finally pulled is what lets the next
    // one be tried. A property that paired teardowns with *enumerations* alone
    // called that legitimate sequence a leak.
    let mut held = 0i32;
    for did in &driver.did {
        held += match did {
            Did::Enumerated { .. } | Did::GaveUp(_) => 1,
            Did::ToreDown(_) => -1,
        };
        if !(0..=1).contains(&held) {
            return Err(format!("seed {seed}: port held {held} times over in {:?}", driver.did));
        }
    }

    // **Agreement.** What the driver believes about the port is what its own
    // record of what it did says, which is the property a lost teardown breaks.
    if driver.attached() != (held == 1) {
        return Err(format!(
            "seed {seed}: believes attached={} after {:?}",
            driver.attached(),
            driver.did
        ));
    }
    Ok(())
}

#[test]
fn no_sequence_breaks_an_invariant() {
    let mut failures = Vec::new();
    for seed in 0..SEQUENCES {
        if let Err(why) = explore(seed) {
            failures.push(why);
            if failures.len() >= 3 {
                break;
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    eprintln!("  [xhci-sim] {SEQUENCES} sequences of {EVENTS} events, no violation");
}

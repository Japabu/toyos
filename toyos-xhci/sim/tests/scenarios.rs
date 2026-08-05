//! What the port machine must do, staged as sequences a person can perform.
//!
//! Every timing here is the machine's own constant, never a literal: a test
//! that hard-codes 100 ms is a test that stops meaning anything the day the
//! debounce moves.

use toyos_xhci::port::{Flaw, GaveUp, Gone, DEBOUNCE_NS, RESET_DEADLINE_NS};
use toyos_xhci_sim::driver::{Did, Driver, Stuck};
use toyos_xhci_sim::hub::{FakePort, ResetBehaviour};

/// A reset that finishes quickly, which is every healthy port.
const QUICK: ResetBehaviour = ResetBehaviour::Completes { after: 1_000_000 };
/// How finely the simulated scheduler samples the clock between wakes.
const PASS: u64 = 1_000_000;

#[test]
fn a_device_plugged_in_is_enumerated_once() {
    let mut port = FakePort::empty(QUICK);
    let mut driver = Driver::new();
    driver.pump(&mut port, 0).unwrap();
    assert_eq!(driver.did, [], "an empty port is not work");

    port.attach();
    driver.run_to(&mut port, 0, 4 * DEBOUNCE_NS, PASS).unwrap();
    assert_eq!(driver.enumerations(), 1, "{:?}", driver.did);
    assert!(driver.attached());
    assert!(!driver.outstanding(), "the port should be at rest afterwards");
}

#[test]
fn the_debounce_is_actually_waited_out() {
    let mut port = FakePort::empty(QUICK);
    let mut driver = Driver::new();
    port.attach();
    // One nanosecond short of the interval USB 2.0 §7.1.7.3 requires.
    driver.run_to(&mut port, 0, DEBOUNCE_NS - 1, PASS).unwrap();
    assert_eq!(driver.enumerations(), 0, "enumerated inside the debounce: {:?}", driver.did);
    driver.run_to(&mut port, DEBOUNCE_NS - 1, 4 * DEBOUNCE_NS, PASS).unwrap();
    assert_eq!(driver.enumerations(), 1);
}

#[test]
fn a_device_that_bounces_back_out_is_never_enumerated() {
    let mut port = FakePort::empty(QUICK);
    let mut driver = Driver::new();
    port.attach();
    driver.run_to(&mut port, 0, DEBOUNCE_NS / 2, PASS).unwrap();
    port.detach();
    driver.run_to(&mut port, DEBOUNCE_NS / 2, 8 * DEBOUNCE_NS, PASS).unwrap();
    assert_eq!(driver.did, [], "{:?}", driver.did);
    assert!(!driver.attached());
}

#[test]
fn unplugging_tears_the_device_down() {
    let mut port = FakePort::occupied(QUICK);
    let mut driver = Driver::new();
    driver.run_to(&mut port, 0, 4 * DEBOUNCE_NS, PASS).unwrap();
    assert_eq!(driver.enumerations(), 1);

    port.detach();
    driver.run_to(&mut port, 4 * DEBOUNCE_NS, 12 * DEBOUNCE_NS, PASS).unwrap();
    assert_eq!(driver.teardowns(), 1, "{:?}", driver.did);
    assert!(matches!(driver.did.last(), Some(Did::ToreDown(Gone::Disconnected))));
    assert!(!driver.attached());
}

/// **The case the level cannot report.** The device is pulled and pushed back
/// inside one debounce, so CCS reads the same at both ends and only CSC says
/// anything happened. The old device must come down before the new one is
/// enumerated, or the slot stays bound to something that has gone.
#[test]
fn a_replug_inside_one_debounce_is_seen() {
    let mut port = FakePort::occupied(QUICK);
    let mut driver = Driver::new();
    driver.run_to(&mut port, 0, 4 * DEBOUNCE_NS, PASS).unwrap();
    assert_eq!(driver.enumerations(), 1);
    let settled = 4 * DEBOUNCE_NS;

    port.replug();
    driver.run_to(&mut port, settled, settled + 8 * DEBOUNCE_NS, PASS).unwrap();

    assert_eq!(driver.teardowns(), 1, "the device that left was not taken down: {:?}", driver.did);
    assert!(driver.did.contains(&Did::ToreDown(Gone::Replugged)));
    assert_eq!(driver.enumerations(), 2, "what is in the port now was not brought up");
    assert!(driver.attached());
}

#[test]
fn a_port_that_never_finishes_its_reset_is_given_up_on_and_left_alone() {
    let mut port = FakePort::empty(ResetBehaviour::Never);
    let mut driver = Driver::new();
    port.attach();
    driver
        .run_to(&mut port, 0, DEBOUNCE_NS + RESET_DEADLINE_NS + PASS, PASS)
        .unwrap();
    assert_eq!(driver.did, [Did::GaveUp(GaveUp::ResetNeverFinished)], "{:?}", driver.did);

    // And it stays given up on: a port retried every pass is a port that costs
    // the machine a reset per pass for as long as the device stays in it.
    let before = driver.did.len();
    driver
        .run_to(
            &mut port,
            DEBOUNCE_NS + RESET_DEADLINE_NS + PASS,
            DEBOUNCE_NS + 4 * RESET_DEADLINE_NS,
            PASS,
        )
        .unwrap();
    assert_eq!(driver.did.len(), before, "the port was tried again: {:?}", driver.did);
}

/// Four replugs, which is a person fidgeting with a cable. Each must produce
/// exactly one teardown and one enumeration, and the port must end bound.
#[test]
fn repeated_replugs_stay_balanced() {
    const CYCLES: usize = 4;
    let mut port = FakePort::occupied(QUICK);
    let mut driver = Driver::new();
    let mut now = 0;
    driver.run_to(&mut port, now, 4 * DEBOUNCE_NS, PASS).unwrap();
    now = 4 * DEBOUNCE_NS;

    for _ in 0..CYCLES {
        port.replug();
        let until = now + 6 * DEBOUNCE_NS;
        driver.run_to(&mut port, now, until, PASS).unwrap();
        now = until;
    }
    assert_eq!(driver.teardowns(), CYCLES, "{:?}", driver.did);
    assert_eq!(driver.enumerations(), CYCLES + 1, "{:?}", driver.did);
    assert!(driver.attached());
}

/// A device the controller has no slot for still leaves the port settled, and
/// the driver does not spin trying again.
#[test]
fn a_device_with_no_slot_settles_anyway() {
    let mut port = FakePort::occupied(QUICK);
    let mut driver = Driver::new().without_slot();
    driver.run_to(&mut port, 0, 8 * DEBOUNCE_NS, PASS).unwrap();
    assert_eq!(driver.enumerations(), 1, "{:?}", driver.did);
    assert!(!driver.outstanding());
}

// ---------------------------------------------------------------------------
// The negative gates. Each turns on one deliberate defect and requires the
// suite above to notice. A gate that cannot fail proves nothing, and these are
// what make the claim "the model covers the states QEMU cannot stage" mean
// something.
// ---------------------------------------------------------------------------

/// Run the replug scenario against a flawed machine and say what happened.
fn replug_under(flaw: Flaw) -> Result<(usize, usize), Stuck> {
    let mut port = FakePort::occupied(QUICK);
    let mut driver = Driver::with_flaw(flaw);
    driver.run_to(&mut port, 0, 4 * DEBOUNCE_NS, PASS)?;
    let settled = 4 * DEBOUNCE_NS;
    port.replug();
    driver.run_to(&mut port, settled, settled + 8 * DEBOUNCE_NS, PASS)?;
    Ok((driver.teardowns(), driver.enumerations()))
}

/// Comparing CCS against what the driver believes, with CSC ignored, makes a
/// replug inside one debounce invisible: the port reads connected at both ends
/// and the slot stays bound to a device that has gone.
#[test]
fn gate_ignoring_the_connect_change_hides_a_replug() {
    let (teardowns, enumerations) = replug_under(Flaw::IgnoreConnectChange).unwrap();
    assert_eq!((teardowns, enumerations), (0, 1), "the flaw did not change the outcome");
}

/// Clearing the change flags before deciding what they meant destroys the same
/// evidence one step earlier.
#[test]
fn gate_acknowledging_before_deciding_hides_a_replug() {
    let (teardowns, enumerations) = replug_under(Flaw::AcknowledgeBeforeDeciding).unwrap();
    assert_eq!((teardowns, enumerations), (0, 1), "the flaw did not change the outcome");
}

/// Writing back the whole word carries PED, and PED is write-1-to-clear: the
/// driver disables the port it has just enabled. The invariant catches it
/// before the port does, which is the point of having one.
#[test]
fn gate_writing_back_what_was_read_disables_the_port() {
    let mut port = FakePort::occupied(QUICK);
    let mut driver = Driver::with_flaw(Flaw::WriteBackWhatWasRead);
    let outcome = driver.run_to(&mut port, 0, 8 * DEBOUNCE_NS, PASS);
    assert!(outcome.is_err(), "a write that carries PED was not caught: {:?}", driver.did);
}

/// Restarting the debounce on every observation means it never elapses, so a
/// device sitting perfectly still is never enumerated.
#[test]
fn gate_restarting_the_debounce_never_enumerates() {
    let mut port = FakePort::occupied(QUICK);
    let mut driver = Driver::with_flaw(Flaw::RestartDebounce);
    driver.run_to(&mut port, 0, 100 * DEBOUNCE_NS, PASS).unwrap();
    assert_eq!(driver.enumerations(), 0, "the flaw did not change the outcome");
}

/// Without the deadline a port that never resets is waited on forever, and the
/// machine makes no progress rather than refusing the port by name.
#[test]
fn gate_dropping_the_reset_deadline_wedges_the_port() {
    let mut port = FakePort::empty(ResetBehaviour::Never);
    let mut driver = Driver::with_flaw(Flaw::NoResetDeadline);
    port.attach();
    let outcome = driver.run_to(&mut port, 0, 8 * RESET_DEADLINE_NS, PASS);
    assert!(
        outcome == Err(Stuck::NoProgress) || driver.did.is_empty(),
        "the flaw did not change the outcome: {:?} {:?}",
        outcome,
        driver.did
    );
    assert_eq!(driver.did, [], "a flawed machine still refused the port");
}

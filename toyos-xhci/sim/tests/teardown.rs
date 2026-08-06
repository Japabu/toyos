//! What a teardown and an endpoint recovery must do once neither of them is
//! allowed to wait.
//!
//! **These are the states QEMU cannot stage.** A controller that never answers
//! Disable Slot is not a device or a machine property; the window between a
//! device going and the teardown running is 100 ms wide and a `device_del`
//! cannot be aimed inside it; and an endpoint recovery racing its own
//! disconnect needs both edges landed in one debounce with a command already in
//! flight. Every timing here is a constant of the machine or of the simulator,
//! never a literal.

use toyos_xhci::port::{Gone, Nanos, DEBOUNCE_NS};
use toyos_xhci::recovery::EndpointState;
use toyos_xhci_sim::driver::{Answers, Broke, Did, Driver, Stuck, ANSWER_DEADLINE_NS};
use toyos_xhci_sim::hub::{FakePort, ResetBehaviour};

/// A reset that finishes quickly, which is every healthy port.
const QUICK: ResetBehaviour = ResetBehaviour::Completes { after: 1_000_000 };
/// How finely the simulated scheduler samples the clock between wakes.
const PASS: Nanos = 1_000_000;
/// Long enough for a device to be bound and settled.
const SETTLED: Nanos = 4 * DEBOUNCE_NS;

/// A bound device on a healthy port, with its interrupt endpoint in `state`.
fn bound(state: EndpointState) -> (FakePort, Driver) {
    let mut port = FakePort::occupied(QUICK);
    let mut driver = Driver::new().with_endpoint(state);
    driver.run_to(&mut port, 0, SETTLED, PASS).unwrap();
    assert_eq!(driver.enumerations(), 1, "{:?}", driver.did);
    (port, driver)
}

/// Run until the port is no longer bound, and say when that was.
fn released_at(driver: &mut Driver, port: &mut FakePort, from: Nanos, until: Nanos) -> Option<Nanos> {
    let mut now = from;
    while now < until {
        driver.pump(port, now).unwrap();
        if !driver.attached() {
            return Some(now);
        }
        now += PASS;
    }
    None
}

#[test]
fn a_teardown_gives_the_slot_and_the_block_back() {
    let (mut port, mut driver) = bound(EndpointState::Running);
    assert!(driver.block_held());

    port.detach();
    driver.run_to(&mut port, SETTLED, SETTLED + 4 * DEBOUNCE_NS, PASS).unwrap();

    assert_eq!(driver.did.last(), Some(&Did::ToreDown(Gone::Disconnected)));
    assert_eq!(driver.disabled, 1, "the slot was never given back");
    assert!(!driver.block_held(), "the pool block is still spoken for");
    assert!(!driver.attached());
    assert!(!driver.busy(), "the controller is still owed something");
}

/// The controller stops answering, which is what a machine wedged by the device
/// that just left looks like from here. The port must come back on the
/// deadline — and **the loop must have made a pass out of every instant in
/// between**, which is what `run_to` asserts by construction: a driver that
/// spun inside the teardown could not be written in this file.
#[test]
fn a_controller_that_never_answers_still_releases_the_port() {
    let (mut port, mut driver) = bound(EndpointState::Running);
    driver.answers = Answers::Never;
    port.detach();

    let due = SETTLED + DEBOUNCE_NS + ANSWER_DEADLINE_NS;
    driver.run_to(&mut port, SETTLED, due - PASS, PASS).unwrap();
    assert!(driver.busy(), "the teardown stopped waiting early");
    assert!(driver.attached(), "the port was released before its deadline");

    driver.run_to(&mut port, due - PASS, due + PASS, PASS).unwrap();
    assert_eq!(driver.disabled, 0, "a controller that never answered disabled a slot");
    assert!(!driver.block_held(), "a leaked block is a machine with no disks after two of them");
    assert!(!driver.attached());
}

// ---------------------------------------------------------------------------
// The recovery, driven a step per pass.
// ---------------------------------------------------------------------------

#[test]
fn a_halted_endpoint_is_reset_cleared_and_delivering_again() {
    let (mut port, mut driver) = bound(EndpointState::Halted);
    driver.endpoint_broke();
    driver.run_to(&mut port, SETTLED, SETTLED + DEBOUNCE_NS, PASS).unwrap();

    assert_eq!(
        driver.commands,
        ["Reset Endpoint", "Set TR Dequeue", "CLEAR_FEATURE(ENDPOINT_HALT)"]
    );
    assert_eq!(driver.requeues, 1, "the endpoint was never handed its next transfer");
    assert_eq!(driver.let_go, 0);
    assert!(driver.attached());
}

/// The shape a transfer the driver abandoned on its deadline leaves: the
/// endpoint never halted, so Reset Endpoint would be a Context State Error and
/// CLEAR_FEATURE would ask a device to clear a halt it does not have.
#[test]
fn a_running_endpoint_is_stopped_rather_than_reset() {
    let (mut port, mut driver) = bound(EndpointState::Running);
    driver.endpoint_broke();
    driver.run_to(&mut port, SETTLED, SETTLED + DEBOUNCE_NS, PASS).unwrap();

    assert_eq!(driver.commands, ["Stop Endpoint", "Set TR Dequeue"]);
    assert_eq!(driver.requeues, 1);
}

#[test]
fn a_recovery_the_controller_refuses_lets_the_device_go() {
    let (mut port, mut driver) = bound(EndpointState::Running);
    driver.answers = Answers::Refuses(6);
    driver.endpoint_broke();
    driver.run_to(&mut port, SETTLED, SETTLED + DEBOUNCE_NS, PASS).unwrap();

    assert_eq!(driver.let_go, 1, "{:?}", driver.commands);
    assert_eq!(driver.requeues, 0);
    // The port stays attached, or the same endpoint is enumerated again every
    // debounce for as long as the device stays in it.
    assert!(driver.attached());
}

// ---------------------------------------------------------------------------
// The disconnect against the recovery, which is the T14's own event: a device
// pulled while a transfer on its endpoint has just errored.
// ---------------------------------------------------------------------------

/// The recovery is abandoned and the teardown runs at once, so the port costs
/// one deadline rather than two.
#[test]
fn a_recovery_is_abandoned_when_its_port_goes() {
    let (mut port, mut driver) = bound(EndpointState::Halted);
    driver.answers = Answers::Never;
    driver.endpoint_broke();
    driver.pump(&mut port, SETTLED).unwrap();
    assert!(driver.busy(), "no recovery was started, so this stages nothing");

    port.detach();
    let end = SETTLED + DEBOUNCE_NS + 3 * ANSWER_DEADLINE_NS;
    let at = released_at(&mut driver, &mut port, SETTLED, end).expect("the port was never released");

    assert_eq!(driver.cancelled, 1, "the recovery outlived the port it was for");
    assert!(
        at <= SETTLED + DEBOUNCE_NS + ANSWER_DEADLINE_NS + PASS,
        "the port took {} ms to come back, which is more than the one deadline the dead \
         controller costs",
        (at - SETTLED) / 1_000_000
    );
}

/// **The negative gate for the cancellation.** A recovery left running against
/// a device that has gone holds the one outstanding operation, and the teardown
/// behind it waits out a deadline nothing was ever going to answer.
#[test]
fn gate_a_recovery_that_outlives_its_port_costs_a_second_deadline() {
    let (mut port, mut driver) = bound(EndpointState::Halted);
    driver = driver.never_cancels();
    driver.answers = Answers::Never;
    driver.endpoint_broke();
    driver.pump(&mut port, SETTLED).unwrap();

    port.detach();
    let end = SETTLED + DEBOUNCE_NS + 3 * ANSWER_DEADLINE_NS;
    let at = released_at(&mut driver, &mut port, SETTLED, end).expect("the port was never released");

    assert_eq!(driver.cancelled, 0, "the flaw did not change the outcome");
    assert!(
        at > SETTLED + DEBOUNCE_NS + ANSWER_DEADLINE_NS,
        "the port came back at {} ms, so the flaw cost nothing and this gate proves nothing",
        (at - SETTLED) / 1_000_000
    );
}

/// **The negative gate for the deferral.** A teardown taken while the
/// controller still owes an answer submits a second command over the first, and
/// the slot it is about is one the controller may hand straight back.
#[test]
fn gate_acting_with_an_answer_outstanding_is_caught() {
    let (mut port, mut driver) = bound(EndpointState::Halted);
    driver = driver.never_cancels().never_defers();
    driver.answers = Answers::Never;
    driver.endpoint_broke();
    driver.pump(&mut port, SETTLED).unwrap();

    port.detach();
    let outcome = driver.run_to(&mut port, SETTLED, SETTLED + 4 * DEBOUNCE_NS, PASS);
    assert_eq!(
        outcome,
        Err(Stuck::Order(Broke::ActedWithAnAnswerOutstanding)),
        "the flaw did not change the outcome: {:?}",
        driver.did
    );
}

/// **The negative gate for the teardown's order.** The pool block goes back
/// while the endpoint contexts inside it are still the controller's to write,
/// which is what the next device's transfer ring lands on top of.
#[test]
fn gate_freeing_the_block_before_the_slot_is_caught() {
    let (mut port, mut driver) = bound(EndpointState::Running);
    driver = driver.frees_blocks_early();
    driver.answers = Answers::Never;
    port.detach();

    let outcome = driver.run_to(&mut port, SETTLED, SETTLED + 4 * DEBOUNCE_NS, PASS);
    assert_eq!(
        outcome,
        Err(Stuck::Order(Broke::ReleasedABlockWithItsSlotOutstanding)),
        "the flaw did not change the outcome: {:?}",
        driver.did
    );
}

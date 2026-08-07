//! What an enumeration must do once it is not allowed to wait.
//!
//! **These are the states QEMU cannot stage.** A controller that answers Enable
//! Slot and then stops is not a device or a machine property; a device pulled
//! between two acts of its own enumeration needs the unplug landed inside a
//! window a few microseconds wide on real hardware, and `device_del` cannot be
//! aimed there. Every timing here is a constant of the machine or of the
//! simulator, never a literal.

use toyos_xhci::enumerate::{Act, Command, Function, Request};
use toyos_xhci::port::{Nanos, DEBOUNCE_NS};
use toyos_xhci_sim::driver::{Answers, Did, Driver, ANSWER_DEADLINE_NS};
use toyos_xhci_sim::hub::{FakePort, ResetBehaviour};

/// A reset that finishes quickly, which is every healthy port.
const QUICK: ResetBehaviour = ResetBehaviour::Completes { after: 1_000_000 };
/// How finely the simulated scheduler samples the clock between wakes.
const PASS: Nanos = 1_000_000;
/// Long enough for a device to be bound and settled.
const SETTLED: Nanos = 4 * DEBOUNCE_NS;
/// What an unplug costs when the enumeration in flight is abandoned: a
/// debounce, then the teardown's own Disable Slot against a controller that has
/// stopped answering. Leaving the enumeration running adds a second one, and
/// that gap is what the gate below measures.
const ONE_DEADLINE: Nanos = DEBOUNCE_NS + ANSWER_DEADLINE_NS + DEBOUNCE_NS;

/// Step until the driver has an act outstanding, and say when that was. The
/// enumeration starts a debounce after the connect, so the caller cannot name
/// the instant itself without restating the machine's own constant.
fn until_busy(driver: &mut Driver, port: &mut FakePort, until: Nanos) -> Nanos {
    let mut now = 0;
    while now < until {
        driver.pump(port, now).unwrap();
        if driver.busy() {
            return now;
        }
        now += PASS;
    }
    panic!("the driver never submitted anything: {:?}", driver.did);
}

/// Run until the port has been torn down and the controller owes nothing, and
/// say when that was.
///
/// Not "no longer attached": a port mid-enumeration has not been reported yet,
/// so it reads unattached from the first instant and every measurement against
/// it would be zero.
fn free_at(driver: &mut Driver, port: &mut FakePort, from: Nanos, until: Nanos) -> Option<Nanos> {
    let mut now = from;
    while now < until {
        driver.pump(port, now).unwrap();
        if driver.teardowns() > 0 && !driver.busy() && !driver.attached() {
            return Some(now);
        }
        now += PASS;
    }
    None
}

/// The acts a boot-protocol HID device's enumeration issues, which is the order
/// [`toyos_xhci::enumerate`] decides and this driver performs.
const BOOT_HID: [Act; 8] = [
    Act::Command(Command::EnableSlot),
    Act::Command(Command::AddressDevice),
    Act::Request(Request::DeviceDescriptor { want: 8 }),
    Act::Request(Request::DeviceDescriptor { want: 18 }),
    Act::Request(Request::ConfigDescriptor),
    Act::Request(Request::SetConfiguration),
    Act::Request(Request::SetProtocol),
    Act::Command(Command::ConfigureEndpoint),
];

/// The property the whole conversion is for: no single pass carries an
/// enumeration from a connected port to a bound device. A driver that waited
/// could not be written in this file at all, so what is checked here is that
/// this one does not — and that the port is reported once at the end rather
/// than once per act.
#[test]
fn an_enumeration_takes_one_act_per_pass_and_reports_once() {
    let mut port = FakePort::occupied(QUICK);
    let mut driver = Driver::new();

    let began = until_busy(&mut driver, &mut port, SETTLED);
    assert_eq!(driver.acts, [Act::Command(Command::EnableSlot)]);
    assert_eq!(driver.enumerations(), 0, "a pass bound the device: {:?}", driver.did);

    driver.run_to(&mut port, began, SETTLED, PASS).unwrap();
    assert_eq!(driver.acts, BOOT_HID);
    assert_eq!(driver.did.last(), Some(&Did::Enumerated { slot: Some(1), trained: false }));
    assert!(driver.block_held());
    assert!(!driver.busy(), "the controller is still owed something");
}

/// A disk and a tablet have no boot protocol to select. The order is
/// [`toyos_xhci::enumerate`]'s to decide and this only checks the driver asks
/// for what it is told to.
#[test]
fn a_device_with_no_boot_protocol_is_never_asked_for_one() {
    for function in [Function::Msc, Function::Hid] {
        let mut port = FakePort::occupied(QUICK);
        let mut driver = Driver::new().presenting(function);
        driver.run_to(&mut port, 0, SETTLED, PASS).unwrap();
        assert_eq!(driver.enumerations(), 1, "{function:?}: {:?}", driver.did);
        assert!(
            !driver.acts.contains(&Act::Request(Request::SetProtocol)),
            "{function:?} was asked for a boot protocol it has none of: {:?}",
            driver.acts
        );
    }
}

/// A controller that answers Enable Slot and then stops. The port must still be
/// reported — with the slot, because only Disable Slot gives one back — rather
/// than staying inside an effect nothing will ever finish.
#[test]
fn an_enumeration_that_stops_being_answered_still_reports_its_slot() {
    let mut port = FakePort::occupied(QUICK);
    let mut driver = Driver::new();
    let began = until_busy(&mut driver, &mut port, SETTLED);
    // Past Enable Slot, so the controller has named a slot to be reported.
    driver.pump(&mut port, began + PASS).unwrap();
    assert_eq!(driver.acts.len(), 2, "{:?}", driver.acts);

    driver.answers = Answers::Never;
    let end = began + 2 * ANSWER_DEADLINE_NS;
    driver.run_to(&mut port, began, end, PASS).unwrap();

    assert_eq!(driver.did.last(), Some(&Did::Enumerated { slot: Some(1), trained: false }));
    assert!(!driver.busy());
    // And the slot is still reachable, so the unplug behind it gives it back.
    driver.answers = Answers::After(0);
    port.detach();
    driver.run_to(&mut port, end, end + 4 * DEBOUNCE_NS, PASS).unwrap();
    assert_eq!(driver.disabled, 1, "the slot the enumeration spent was never given back");
}

/// **A transfer error on a port that has gone belongs to the disconnect.** The
/// device will not answer the act outstanding against it, so the teardown
/// behind it would spend a whole deadline finding that out.
#[test]
fn an_enumeration_is_abandoned_when_its_port_goes() {
    let mut port = FakePort::occupied(QUICK);
    let mut driver = Driver::new();
    // Past Enable Slot, which is the one act that is never abandoned.
    let began = until_busy(&mut driver, &mut port, SETTLED);
    driver.pump(&mut port, began + PASS).unwrap();
    assert_eq!(driver.acts.len(), 2, "{:?}", driver.acts);

    driver.answers = Answers::Never;
    port.detach();
    let end = began + 3 * ANSWER_DEADLINE_NS;
    let at = free_at(&mut driver, &mut port, began, end).expect("the port was never freed");

    assert_eq!(driver.abandoned, 1, "the enumeration was waited out instead");
    assert!(
        at < began + ONE_DEADLINE,
        "the port came back at {} ms, which is the enumeration's deadline as well as the \
         teardown's",
        (at - began) / 1_000_000
    );
}

/// **The negative gate for that cancellation.** With it off, the teardown waits
/// out the deadline of an act whose device left the bus — which is the second
/// of the two costs an unplug used to pay, and the one no `device_del` can be
/// aimed at.
#[test]
fn gate_an_enumeration_that_outlives_its_port_costs_a_deadline() {
    let mut port = FakePort::occupied(QUICK);
    let mut driver = Driver::new().never_cancels();
    let began = until_busy(&mut driver, &mut port, SETTLED);
    driver.pump(&mut port, began + PASS).unwrap();

    driver.answers = Answers::Never;
    port.detach();
    let end = began + 4 * ANSWER_DEADLINE_NS;
    let at = free_at(&mut driver, &mut port, began, end).expect("the port was never freed");

    assert_eq!(driver.abandoned, 0, "the flaw did not change the outcome");
    assert!(
        at > began + ONE_DEADLINE,
        "the port came back at {} ms, so the flaw cost nothing and this gate proves nothing",
        (at - began) / 1_000_000
    );
}

/// **Enable Slot is the one act that is never abandoned**, because its answer
/// *is* the slot id: a driver that stopped listening for it would leak a Device
/// Slot the controller has already allocated, and nothing would ever name it
/// again.
#[test]
fn a_slot_the_controller_has_not_named_yet_is_waited_for_even_when_the_port_goes() {
    let mut port = FakePort::occupied(QUICK);
    let mut driver = Driver::new().answering(Answers::Never);
    let began = until_busy(&mut driver, &mut port, SETTLED);
    assert_eq!(driver.acts, [Act::Command(Command::EnableSlot)]);

    port.detach();
    driver.run_to(&mut port, began, began + ANSWER_DEADLINE_NS / 2, PASS).unwrap();

    assert_eq!(driver.abandoned, 0, "the Enable Slot was abandoned and its slot with it");
    assert!(driver.busy(), "nothing is listening for the slot the controller was asked for");
}

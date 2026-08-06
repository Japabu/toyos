//! What a SuperSpeed port needs, which is not what a USB2 port needs.
//!
//! **None of this is reachable in QEMU.** Its xHC has no link training and no
//! Inactive state; its SS ports read Enabled the moment the register is
//! touched, and no device or machine property makes a reset fail. So the guest
//! suite can only certify that nothing regressed, and correctness for these
//! paths lives here — which is why the negative gates beside them matter more
//! than usual.

use toyos_xhci::port::{GaveUp, Reset, DEBOUNCE_NS, RESET_DEADLINE_NS};
use toyos_xhci::Protocol;
use toyos_xhci_sim::driver::{Did, Driver};
use toyos_xhci_sim::hub::{FakePort, ResetBehaviour};

const QUICK: ResetBehaviour = ResetBehaviour::Completes { after: 1_000_000 };
const PASS: u64 = 1_000_000;

/// **The fix, stated as the thing that must not happen.** A SuperSpeed link
/// trains itself and the port reads Enabled when it is up; there is nothing to
/// reset, and resetting anyway is a hot reset of a working link.
#[test]
fn a_trained_superspeed_link_is_not_reset() {
    let mut port = FakePort::superspeed(QUICK);
    let mut driver = Driver::new().speaking(Protocol::Usb3);
    port.attach();
    driver.run_to(&mut port, 0, 4 * DEBOUNCE_NS, PASS).unwrap();

    assert_eq!(driver.resets(), [], "a trained link was reset: {:?}", driver.did);
    assert_eq!(driver.enumerations(), 1, "{:?}", driver.did);
    assert!(
        driver.did.contains(&Did::Enumerated { slot: Some(1), trained: true }),
        "the port was brought up but not reported as already trained: {:?}",
        driver.did
    );
}

/// The same port on a driver that never read the protocol capability: it does
/// not know, so it does what it always did. This is what makes the test above
/// a statement about the *capability* rather than about SuperSpeed in general.
#[test]
fn a_port_of_unknown_protocol_is_still_reset() {
    let mut port = FakePort::superspeed(QUICK);
    let mut driver = Driver::new();
    port.attach();
    driver.run_to(&mut port, 0, 4 * DEBOUNCE_NS, PASS).unwrap();
    assert_eq!(driver.resets(), [Reset::Hot], "{:?}", driver.did);
    assert_eq!(driver.enumerations(), 1);
}

/// A USB2 port still needs its reset — the reset is how a device gets enabled
/// there at all, so the fix must not take it away from the ports that need it.
#[test]
fn a_usb2_port_is_reset_as_before() {
    let mut port = FakePort::empty(QUICK);
    let mut driver = Driver::new().speaking(Protocol::Usb2);
    port.attach();
    driver.run_to(&mut port, 0, 4 * DEBOUNCE_NS, PASS).unwrap();
    assert_eq!(driver.resets(), [Reset::Hot]);
    assert_eq!(driver.enumerations(), 1, "{:?}", driver.did);
    assert!(driver.did.contains(&Did::Enumerated { slot: Some(1), trained: false }));
}

/// **The T14's USB-A ports.** A SuperSpeed port that is *not* enabled when the
/// device appears needs a reset — and the hot one takes the link down into
/// Inactive, which §4.19.1.2.4 says only a warm reset leaves. A driver with no
/// warm reset spends the deadline and refuses the port, which is a USB-A socket
/// that never mounts anything, twice out of two boots.
#[test]
fn a_link_a_hot_reset_kills_is_recovered_warm() {
    let mut port = FakePort::empty(ResetBehaviour::HotResetKillsTheLink { warm_works: true });
    let mut driver = Driver::new().speaking(Protocol::Usb3);
    port.attach();
    driver
        .run_to(&mut port, 0, DEBOUNCE_NS + 2 * RESET_DEADLINE_NS, PASS)
        .unwrap();

    assert_eq!(
        driver.resets(),
        [Reset::Hot, Reset::Warm],
        "the hot reset was not followed by a warm one: {:?}",
        driver.did
    );
    assert_eq!(driver.enumerations(), 1, "the port never came up: {:?}", driver.did);
    assert!(driver.attached());
}

/// Once the link is Inactive the driver goes straight to the warm reset rather
/// than spending the deadline proving a hot one does not work. Same port, but
/// the link is already down when the driver first looks.
#[test]
fn a_link_already_inactive_is_warm_reset_at_once() {
    let mut port = FakePort::empty(ResetBehaviour::HotResetKillsTheLink { warm_works: true });
    let mut driver = Driver::new().speaking(Protocol::Usb3);
    port.attach();
    // Let the first hot reset take the link down, then start over with a fresh
    // driver that finds the port in the state the last one left it.
    driver.run_to(&mut port, 0, DEBOUNCE_NS + PASS, PASS).unwrap();
    let mut fresh = Driver::new().speaking(Protocol::Usb3);
    port.replug();
    fresh
        .run_to(&mut port, DEBOUNCE_NS + PASS, 4 * DEBOUNCE_NS + RESET_DEADLINE_NS, PASS)
        .unwrap();
    assert_eq!(
        fresh.resets(),
        [Reset::Warm],
        "an Inactive link was given a hot reset first: {:?}",
        fresh.did
    );
}

/// And a link that will not come back even warm is refused by a name that says
/// so, rather than by the name a USB2 port's failure would get.
#[test]
fn a_link_that_never_trains_is_refused_by_its_own_name() {
    let mut port = FakePort::empty(ResetBehaviour::HotResetKillsTheLink { warm_works: false });
    let mut driver = Driver::new().speaking(Protocol::Usb3);
    port.attach();
    driver
        .run_to(&mut port, 0, DEBOUNCE_NS + 3 * RESET_DEADLINE_NS, PASS)
        .unwrap();
    assert_eq!(driver.resets(), [Reset::Hot, Reset::Warm], "{:?}", driver.did);
    assert_eq!(
        driver.did.last(),
        Some(&Did::GaveUp(GaveUp::LinkNeverTrained)),
        "{:?}",
        driver.did
    );
}

/// A USB2 port whose reset never finishes is still refused the USB2 way: no
/// warm reset is attempted, because the bit does not exist on that port and
/// writing it would be a driver guessing at hardware it was told about.
#[test]
fn a_usb2_port_is_never_warm_reset() {
    let mut port = FakePort::empty(ResetBehaviour::Never);
    let mut driver = Driver::new().speaking(Protocol::Usb2);
    port.attach();
    driver
        .run_to(&mut port, 0, DEBOUNCE_NS + 3 * RESET_DEADLINE_NS, PASS)
        .unwrap();
    assert_eq!(driver.resets(), [Reset::Hot], "{:?}", driver.did);
    assert_eq!(
        driver.did.last(),
        Some(&Did::GaveUp(GaveUp::ResetNeverFinished(Reset::Hot))),
        "{:?}",
        driver.did
    );
}

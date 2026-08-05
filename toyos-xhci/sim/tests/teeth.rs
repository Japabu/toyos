//! The check the negative gates themselves need.
//!
//! A gate asserts what a *flawed* machine does. If the sound machine did the
//! same thing the gate would pass either way and prove nothing — so every flaw
//! is required here to change an outcome this simulator can see. Refactor the
//! machine until a flaw stops being reachable and this goes red, which is what
//! it is for.

use toyos_xhci::port::{Flaw, DEBOUNCE_NS, RESET_DEADLINE_NS};
use toyos_xhci_sim::driver::Driver;
use toyos_xhci_sim::hub::{FakePort, ResetBehaviour};

const QUICK: ResetBehaviour = ResetBehaviour::Completes { after: 1_000_000 };
const PASS: u64 = 1_000_000;

/// Everything one machine does across the two scenarios that between them
/// touch every decision a flaw can spoil.
fn fingerprint(flaw: Flaw) -> String {
    let mut out = String::new();

    // A healthy port: plug, settle, replug inside a debounce, unplug.
    let mut port = FakePort::occupied(QUICK);
    let mut driver = Driver::with_flaw(flaw);
    let r = driver.run_to(&mut port, 0, 4 * DEBOUNCE_NS, PASS);
    let mut now = 4 * DEBOUNCE_NS;
    if r.is_ok() {
        port.replug();
        if driver.run_to(&mut port, now, now + 6 * DEBOUNCE_NS, PASS).is_ok() {
            now += 6 * DEBOUNCE_NS;
            port.detach();
            let _ = driver.run_to(&mut port, now, now + 6 * DEBOUNCE_NS, PASS);
        }
    }
    out.push_str(&format!("healthy={:?} err={:?};", driver.did, r.err()));

    // A port whose reset never finishes.
    let mut port = FakePort::empty(ResetBehaviour::Never);
    let mut driver = Driver::with_flaw(flaw);
    port.attach();
    let r = driver.run_to(&mut port, 0, DEBOUNCE_NS + 4 * RESET_DEADLINE_NS, PASS);
    out.push_str(&format!("dead={:?} err={:?}", driver.did, r.err()));
    out
}

#[test]
fn every_flaw_changes_an_outcome() {
    let sound = fingerprint(Flaw::None);
    for flaw in [
        Flaw::IgnoreConnectChange,
        Flaw::AcknowledgeBeforeDeciding,
        Flaw::WriteBackWhatWasRead,
        Flaw::RestartDebounce,
        Flaw::NoResetDeadline,
    ] {
        assert_ne!(
            fingerprint(flaw),
            sound,
            "{flaw:?} is not a defect this simulator can see, so the gate that stages it \
             would pass against a machine with the defect in it"
        );
    }
}

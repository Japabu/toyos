//! What must be true of a port after every step, whatever sequence produced it.
//!
//! Separate from the tests because the simulator checks these after *every*
//! step of every generated sequence, where a test checks an outcome after a
//! sequence it chose. `specs/xhci-port-machine-plan.md` §3 X0 lists them.

use crate::port::{PortState, Step};
use crate::portsc::{self, Portsc};

/// An invariant that did not hold, named so a shrunk trace says which.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Violation {
    /// A write carried PED, which disables the port it was meant to enable.
    WriteWouldDisable,
    /// A write set PED and PR together, which §5.4.8 note 82 calls undefined.
    WriteSetsPedAndReset,
    /// A write cleared a change flag that was not set in the word it was built
    /// from — a change nobody looked at, lost.
    WriteClearsAnUnseenChange,
    /// The machine asked for a device to be enumerated on a port it does not
    /// believe anything is attached to.
    EnumeratedNothing,
    /// The machine asked for a teardown of a port it holds no slot for and
    /// believes nothing is attached to.
    ToreDownNothing,
    /// A wait was issued for an instant that has already passed, so the caller
    /// is asked to come back at once and the machine makes no progress.
    WaitIsNotInTheFuture,
    /// The port was stepped from inside an effect it had already been told to
    /// perform — the re-entrancy the old copy-and-write-back shape allowed
    /// silently.
    SteppedWhileWorking,
}

/// Check one step against the word that produced it and the clock that timed
/// it. `None` when the step is sound.
pub fn check(before: &PortState, step: &Step<'_>, read: Portsc, now: u64) -> Option<Violation> {
    if before.working().is_some() {
        return Some(Violation::SteppedWhileWorking);
    }
    match step {
        Step::Write(write) | Step::Reset(write) => write_is_sound(*write, read),
        Step::Enumerate(_) => {
            // The port is being brought up because its reset finished, so the
            // register is the authority on whether there is anything there.
            (!read.connected()).then_some(Violation::EnumeratedNothing)
        }
        Step::Teardown(..) => (!before.attached() && before.slot().is_none())
            .then_some(Violation::ToreDownNothing),
        Step::Wait(at) => (*at <= now).then_some(Violation::WaitIsNotInTheFuture),
        Step::Idle | Step::GaveUp(_) => None,
    }
}

fn write_is_sound(write: portsc::Write, read: Portsc) -> Option<Violation> {
    // Mirrors the register's own bit positions rather than importing private
    // masks: an invariant that reads the world through the same accessor as the
    // code it checks cannot catch that accessor being wrong.
    const PED: u32 = 1 << 1;
    const PR: u32 = 1 << 4;
    const CHANGES: u32 = 0x7F << 17;

    let raw = write.raw();
    if raw & PED != 0 {
        return Some(if raw & PR != 0 {
            Violation::WriteSetsPedAndReset
        } else {
            Violation::WriteWouldDisable
        });
    }
    if raw & CHANGES & !(read.raw() & CHANGES) != 0 {
        return Some(Violation::WriteClearsAnUnseenChange);
    }
    None
}

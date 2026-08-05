//! The loop the kernel will run, with the effects replaced by a record of them.
//!
//! Deliberately the *shape* the kernel takes and not a convenience: read the
//! register, ask the machine, do the one thing it says, read again. A simulator
//! whose loop differs from the driver's tests a driver nobody ships.

use toyos_xhci::invariants::{self, Violation};
use toyos_xhci::port::{Flaw, GaveUp, Gone, Nanos, PortState, Step};

use crate::hub::FakePort;

/// An effect the driver performed, in the order it performed them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Did {
    Enumerated { slot: Option<u8> },
    ToreDown(Gone),
    GaveUp(GaveUp),
}

/// Why a pump stopped without the port going quiet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stuck {
    /// An invariant did not hold.
    Broke(Violation),
    /// The machine kept asking for work without ever going idle or waiting for
    /// a future instant. A live-lock is a failure whatever it looks like from
    /// inside.
    NoProgress,
}

/// How many steps one observation may produce before the machine is declared
/// stuck. Generous: the longest legitimate run is teardown, acknowledge,
/// debounce — four.
const STEP_BUDGET: usize = 16;

pub struct Driver {
    state: PortState,
    /// What the enumeration answers, so a scenario can stage a device the
    /// controller has no slot for.
    slot: Option<u8>,
    /// Ask the machine what to do from *inside* an effect it is having
    /// performed, which is what a driver that polled its own ports from within
    /// an enumeration would do. The enumeration drains the event ring, so this
    /// is reachable rather than hypothetical.
    reenter: bool,
    pub did: Vec<Did>,
    pub wake_at: Option<Nanos>,
}

impl Driver {
    pub fn new() -> Self {
        Self {
            state: PortState::EMPTY,
            slot: Some(1),
            reenter: false,
            did: Vec::new(),
            wake_at: None,
        }
    }

    pub fn with_flaw(flaw: Flaw) -> Self {
        Self { state: PortState::with_flaw(flaw), ..Self::new() }
    }

    /// Stage an enumeration that produces no slot.
    pub fn without_slot(mut self) -> Self {
        self.slot = None;
        self
    }

    /// Stage a caller that re-enters the machine from inside an effect.
    pub fn reentrant(mut self) -> Self {
        self.reenter = true;
        self
    }

    pub fn attached(&self) -> bool {
        self.state.attached()
    }

    pub fn outstanding(&self) -> bool {
        self.state.outstanding()
    }

    /// Everything the driver has to do for this port at `now`, with every step
    /// checked against the word that produced it.
    pub fn pump(&mut self, port: &mut FakePort, now: Nanos) -> Result<(), Stuck> {
        port.tick(now);
        for _ in 0..STEP_BUDGET {
            let read = port.read();
            let before = self.state;
            let step = self.state.step(read, now);
            if let Some(bad) = invariants::check(&before, &step, read, now) {
                return Err(Stuck::Broke(bad));
            }
            match step {
                Step::Idle => {
                    self.wake_at = None;
                    return Ok(());
                }
                Step::Wait(at) => {
                    self.wake_at = Some(at);
                    return Ok(());
                }
                Step::GaveUp(why) => {
                    self.did.push(Did::GaveUp(why));
                    self.wake_at = None;
                    return Ok(());
                }
                Step::Write(write) | Step::Reset(write) => port.write(write.raw(), now),
                Step::Teardown(why, pending) => {
                    pending.running();
                    // Between here and the report, the port is inside an
                    // effect. A step taken now is the re-entrancy the
                    // invariant names, and the simulator stages exactly that
                    // below.
                    if self.reenter {
                        let read = port.read();
                        let before = self.state;
                        let step = self.state.step(read, now);
                        if let Some(bad) = invariants::check(&before, &step, read, now) {
                            return Err(Stuck::Broke(bad));
                        }
                    }
                    self.did.push(Did::ToreDown(why));
                    self.state.torn_down();
                }
                Step::Enumerate(pending) => {
                    pending.running();
                    if self.reenter {
                        let read = port.read();
                        let before = self.state;
                        let step = self.state.step(read, now);
                        if let Some(bad) = invariants::check(&before, &step, read, now) {
                            return Err(Stuck::Broke(bad));
                        }
                    }
                    self.did.push(Did::Enumerated { slot: self.slot });
                    self.state.enumerated(self.slot.and_then(core::num::NonZeroU8::new));
                }
            }
            port.tick(now);
        }
        Err(Stuck::NoProgress)
    }

    /// Run the port forward to `deadline`, waking whenever the machine asked to
    /// be woken. `step` is how finely the clock is sampled between wakes, which
    /// is what a scheduler pass on a busy machine looks like.
    pub fn run_to(
        &mut self,
        port: &mut FakePort,
        from: Nanos,
        deadline: Nanos,
        step: Nanos,
    ) -> Result<Nanos, Stuck> {
        let mut now = from;
        while now < deadline {
            self.pump(port, now)?;
            now = match self.wake_at {
                Some(at) if at <= now => return Err(Stuck::NoProgress),
                Some(at) => at.min(now + step),
                None => now + step,
            };
        }
        self.pump(port, deadline)?;
        Ok(deadline)
    }

    pub fn enumerations(&self) -> usize {
        self.did.iter().filter(|d| matches!(d, Did::Enumerated { .. })).count()
    }

    pub fn teardowns(&self) -> usize {
        self.did.iter().filter(|d| matches!(d, Did::ToreDown(_))).count()
    }
}

impl Default for Driver {
    fn default() -> Self {
        Self::new()
    }
}

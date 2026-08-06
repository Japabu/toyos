//! The loop the kernel runs, with the effects replaced by a record of them.
//!
//! Deliberately the *shape* the kernel takes and not a convenience: drain the
//! controller's answers, act on whatever is finished, read the register, ask the
//! machine, do the one thing it says, read again. A simulator whose loop differs
//! from the driver's tests a driver nobody ships.
//!
//! **Nothing here can wait**, and that is the property under test as much as any
//! assertion: there is no primitive in this file that spins on a clock, so a
//! teardown or a recovery that could only be expressed by waiting could not be
//! written here at all.

use core::num::NonZeroU8;

use toyos_xhci::invariants::{self, Violation};
use toyos_xhci::job::{Await, Outcome, Outstanding};
use toyos_xhci::port::{Flaw, GaveUp, Gone, Nanos, PortState, Reset, Step};
use toyos_xhci::recovery::{Act, EndpointState, NeedsConfigure, Recovery};
use toyos_xhci::Protocol;

use crate::hub::FakePort;

/// An effect the driver performed on the *port*, in the order it performed
/// them. The slot, the pool block and the endpoint are asked about separately,
/// because they move on the controller's clock rather than the port's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Did {
    Enumerated { slot: Option<u8>, trained: bool },
    Reset(Reset),
    ToreDown(Gone),
    GaveUp(GaveUp),
}

/// Why a pump stopped without the port going quiet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stuck {
    /// An invariant of the port machine did not hold.
    Broke(Violation),
    /// An invariant of the teardown-and-recovery loop did not hold.
    Order(Broke),
    /// The machine kept asking for work without ever going idle or waiting for
    /// a future instant. A live-lock is a failure whatever it looks like from
    /// inside.
    NoProgress,
}

/// What the loop must never do, whatever sequence produced it.
///
/// Checked here rather than in [`invariants`] because the resources these are
/// about — the controller's slot and the driver's pool block — belong to the
/// driver and not to the port machine, which knows only what a register reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Broke {
    /// A port was enumerated or torn down while the controller still owed an
    /// answer about the last thing it was given. The Enable Slot that follows
    /// may be handed the very slot whose Disable Slot has not completed, and
    /// the driver is then about to zero the device-context pointer the new
    /// device is using.
    ActedWithAnAnswerOutstanding,
    /// A pool block went back to the pool while the controller still owed an
    /// answer about the slot whose endpoint contexts name that memory.
    ReleasedABlockWithItsSlotOutstanding,
}

/// How the controller answers the commands it is given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answers {
    /// Success, this long after the command was submitted.
    After(Nanos),
    /// Nothing ever comes back. A controller wedged by the device that left,
    /// which is the state no `device_del` can stage and the reason the deadline
    /// exists at all.
    Never,
    /// It answers, with a completion code that is not Success.
    Refuses(u32),
}

/// xHCI 1.2 Table 6-90's Success.
const SUCCESS: u32 = 1;

/// How long the simulated driver waits for an answer before calling the
/// controller silent. It stands for the kernel's own transfer budget and is not
/// read from it — nothing here is testing what that number *is*, only what
/// happens on both sides of it.
pub const ANSWER_DEADLINE_NS: Nanos = 2_000_000_000;

/// What the outstanding operation is for. The kernel's own enum, with the
/// fields it carries only to name things in a log dropped.
enum What {
    /// The port's device has gone: its blocks are the next device's the moment
    /// its slot is, and the port becomes one the machine may enumerate again.
    SlotGone,
    /// A device given up on while it is still in its port, which is why the
    /// port stays marked attached and its blocks stay claimed.
    LetGo,
    Recovering(Recovery),
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
    /// Enumerate and tear down without asking whether the controller still owes
    /// an answer, which is the negative gate for the deferral.
    never_defers: bool,
    /// Keep an endpoint recovery running after its port has gone, which is the
    /// negative gate for the cancellation.
    never_cancels: bool,
    /// Give the pool block back the instant Disable Slot is submitted, which is
    /// the negative gate for the teardown's order.
    frees_blocks_early: bool,

    outstanding: Outstanding<What>,
    /// Completions the controller owes: where the command was, what it will say
    /// and when. A `Vec` because a command submitted while another is owed is
    /// exactly what the deferral is supposed to make impossible.
    owed: Vec<(u64, u32, Nanos)>,
    next_trb: u64,
    pub answers: Answers,

    /// Slots the controller took back, which is what a replug that leaks one
    /// would leave short.
    pub disabled: usize,
    /// Whether the pool block this port's device claimed is still spoken for.
    block_held: bool,
    /// What the controller publishes for the bound device's interrupt
    /// endpoint, and `None` where there is no bound device.
    endpoint: Option<EndpointState>,
    /// The endpoint delivered a completion this driver did not expect.
    broke: bool,

    pub did: Vec<Did>,
    /// Every command and control request the recovery issued, by the name the
    /// driver logs it under.
    pub commands: Vec<&'static str>,
    /// Recoveries abandoned because the port they were for has gone.
    pub cancelled: usize,
    /// Times the endpoint was handed its next transfer, which is what "the
    /// device is delivering again" means.
    pub requeues: usize,
    /// Devices let go because their endpoint could not be restarted.
    pub let_go: usize,
    pub wake_at: Option<Nanos>,
}

impl Driver {
    pub fn new() -> Self {
        Self {
            state: PortState::EMPTY,
            slot: Some(1),
            reenter: false,
            never_defers: false,
            never_cancels: false,
            frees_blocks_early: false,
            outstanding: Outstanding::EMPTY,
            owed: Vec::new(),
            next_trb: 0x1000,
            answers: Answers::After(0),
            disabled: 0,
            block_held: false,
            endpoint: None,
            broke: false,
            did: Vec::new(),
            commands: Vec::new(),
            cancelled: 0,
            requeues: 0,
            let_go: 0,
            wake_at: None,
        }
    }

    pub fn with_flaw(flaw: Flaw) -> Self {
        Self { state: PortState::with_flaw(flaw), ..Self::new() }
    }

    /// A driver that read the controller's Supported Protocol capability and so
    /// knows what this port speaks.
    pub fn speaking(mut self, protocol: Protocol) -> Self {
        self.state.speaks(Some(protocol));
        self
    }

    /// Every reset the driver issued, in order — the assertion for a port that
    /// should have needed none.
    pub fn resets(&self) -> Vec<Reset> {
        self.did
            .iter()
            .filter_map(|d| match d {
                Did::Reset(kind) => Some(*kind),
                _ => None,
            })
            .collect()
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

    /// Stage a controller that answers this way.
    pub fn answering(mut self, answers: Answers) -> Self {
        self.answers = answers;
        self
    }

    /// Stage the driver that acts on a port without asking whether the
    /// controller still owes it an answer.
    pub fn never_defers(mut self) -> Self {
        self.never_defers = true;
        self
    }

    /// Stage the driver that keeps a recovery running after its port has gone.
    pub fn never_cancels(mut self) -> Self {
        self.never_cancels = true;
        self
    }

    /// Stage the driver that gives a pool block back the instant Disable Slot
    /// is submitted, while the endpoint contexts in it are still the
    /// controller's to write.
    pub fn frees_blocks_early(mut self) -> Self {
        self.frees_blocks_early = true;
        self
    }

    /// Bind a device with an interrupt endpoint in this state, as
    /// [`Did::Enumerated`] would have left it.
    pub fn with_endpoint(mut self, state: EndpointState) -> Self {
        self.endpoint = Some(state);
        self
    }

    pub fn attached(&self) -> bool {
        self.state.attached()
    }

    /// Whether the port machine has something in hand. Not the same question as
    /// [`Self::busy`]: a port at rest can still be waiting on a slot.
    pub fn outstanding(&self) -> bool {
        self.state.outstanding()
    }

    /// Whether the controller owes this driver an answer.
    pub fn busy(&self) -> bool {
        self.outstanding.busy()
    }

    pub fn block_held(&self) -> bool {
        self.block_held
    }

    /// The endpoint's state as the controller publishes it.
    pub fn endpoint(&self) -> Option<EndpointState> {
        self.endpoint
    }

    /// The endpoint delivered a completion the driver did not expect, which is
    /// what starts a recovery.
    pub fn endpoint_broke(&mut self) {
        self.broke = true;
    }

    /// Everything the driver has to do at `now`, with every step checked
    /// against the word that produced it.
    pub fn pump(&mut self, port: &mut FakePort, now: Nanos) -> Result<(), Stuck> {
        port.tick(now);
        self.collect(now);
        self.advance(now)?;
        self.recover(port, now);
        for _ in 0..STEP_BUDGET {
            let read = port.read();
            if !read.connected() || read.connect_changed() {
                self.cancel_recovery();
            }
            // A port inside an effect a previous pass began is not decided
            // about until the controller has answered for it.
            if self.state.working().is_some() {
                self.wake_at = self.outstanding.wake_at();
                return Ok(());
            }
            let busy = if self.never_defers { None } else { self.outstanding.wake_at() };
            let before = self.state;
            let step = self.state.step(read, now);
            if let Some(bad) = invariants::check(&before, &step, read, now) {
                return Err(Stuck::Broke(bad));
            }
            match step {
                Step::Idle => {
                    self.wake_at = self.outstanding.wake_at();
                    return Ok(());
                }
                Step::Wait(at) => {
                    self.wake_at = Some(at);
                    return Ok(());
                }
                Step::GaveUp(why) => {
                    self.did.push(Did::GaveUp(why));
                    self.wake_at = self.outstanding.wake_at();
                    return Ok(());
                }
                Step::Write(write) => port.write(write.raw(), now),
                Step::Reset(kind, write) => {
                    self.did.push(Did::Reset(kind));
                    port.write(write.raw(), now);
                }
                Step::Teardown(why, pending) => {
                    if let Some(at) = busy {
                        self.wake_at = Some(at);
                        return Ok(());
                    }
                    pending.running();
                    // Between here and the report, the port is inside an
                    // effect. A step taken now is the re-entrancy the invariant
                    // names, and the simulator stages exactly that below.
                    if self.reenter {
                        let read = port.read();
                        let before = self.state;
                        let step = self.state.step(read, now);
                        if let Some(bad) = invariants::check(&before, &step, read, now) {
                            return Err(Stuck::Broke(bad));
                        }
                    }
                    self.did.push(Did::ToreDown(why));
                    if self.tear_down(now)? {
                        self.state.torn_down();
                    } else {
                        self.wake_at = self.outstanding.wake_at();
                        return Ok(());
                    }
                }
                Step::Enumerate { trained, pending } => {
                    if let Some(at) = busy {
                        self.wake_at = Some(at);
                        return Ok(());
                    }
                    pending.running();
                    if self.reenter {
                        let read = port.read();
                        let before = self.state;
                        let step = self.state.step(read, now);
                        if let Some(bad) = invariants::check(&before, &step, read, now) {
                            return Err(Stuck::Broke(bad));
                        }
                    }
                    if self.outstanding.busy() {
                        return Err(Stuck::Order(Broke::ActedWithAnAnswerOutstanding));
                    }
                    self.did.push(Did::Enumerated { slot: self.slot, trained });
                    self.block_held = self.slot.is_some();
                    self.state.enumerated(self.slot.and_then(NonZeroU8::new));
                }
            }
            port.tick(now);
        }
        Err(Stuck::NoProgress)
    }

    /// Everything a device that is no longer on the bus leaves behind, in the
    /// order the pieces stop being reachable. `true` when nothing is owed.
    fn tear_down(&mut self, now: Nanos) -> Result<bool, Stuck> {
        self.endpoint = None;
        self.broke = false;
        if self.outstanding.busy() {
            return Err(Stuck::Order(Broke::ActedWithAnAnswerOutstanding));
        }
        if self.state.take_slot().is_none() {
            self.release_block()?;
            return Ok(true);
        }
        self.submit(What::SlotGone, now);
        if self.frees_blocks_early {
            self.release_block()?;
        }
        Ok(false)
    }

    fn release_block(&mut self) -> Result<(), Stuck> {
        if self.outstanding.busy() {
            return Err(Stuck::Order(Broke::ReleasedABlockWithItsSlotOutstanding));
        }
        self.block_held = false;
        Ok(())
    }

    /// Submit a command and record what its answer is owed to.
    fn submit(&mut self, what: What, now: Nanos) {
        let trb = self.next_trb;
        self.next_trb += 16;
        let answer = match self.answers {
            Answers::After(delay) => Some((SUCCESS, now + delay)),
            Answers::Refuses(code) => Some((code, now)),
            Answers::Never => None,
        };
        if let Some((code, at)) = answer {
            self.owed.push((trb, code, at));
        }
        self.outstanding.submit(what, Await::Command { trb }, now + ANSWER_DEADLINE_NS);
    }

    /// Hand the controller's answers to whatever is waiting for them.
    fn collect(&mut self, now: Nanos) {
        let mut still_owed = Vec::new();
        for (trb, code, at) in core::mem::take(&mut self.owed) {
            if at > now {
                still_owed.push((trb, code, at));
                continue;
            }
            self.outstanding.answered(Await::Command { trb }, code);
        }
        self.owed = still_owed;
    }

    /// Act on whatever the controller has answered.
    fn advance(&mut self, now: Nanos) -> Result<(), Stuck> {
        while let Some((what, outcome)) = self.outstanding.finished(now) {
            match what {
                What::SlotGone => {
                    if outcome == Outcome::Answered(SUCCESS) {
                        self.disabled += 1;
                    }
                    // The block goes back whatever the controller said: a port
                    // whose device has left holding one for the life of the
                    // boot is the worse failure, and a controller that will not
                    // disable a slot is already past what a driver repairs.
                    self.release_block()?;
                    self.state.torn_down();
                }
                What::LetGo => {
                    if outcome == Outcome::Answered(SUCCESS) {
                        self.disabled += 1;
                    }
                }
                What::Recovering(mut seq) => {
                    if outcome != Outcome::Answered(SUCCESS) {
                        self.give_up(now);
                        continue;
                    }
                    let act = seq.completed();
                    self.issue(seq, act, now);
                }
            }
        }
        Ok(())
    }

    /// Start the bound endpoint's recovery, if one is owed and the controller
    /// is not already answering something else.
    fn recover(&mut self, port: &FakePort, now: Nanos) {
        if !self.broke || self.outstanding.busy() {
            return;
        }
        self.broke = false;
        let read = port.read();
        // **When the disconnect and the transfer error race, the disconnect
        // wins.** CSC as well as CCS: a device replugged between two looks
        // reads connected again, and the transfer that died still died with the
        // old one.
        if !read.connected() || read.connect_changed() {
            self.commands.push("left to the disconnect");
            return;
        }
        let Some(state) = self.endpoint else { return };
        match Recovery::begin(state) {
            Ok((seq, act)) => self.issue(seq, act, now),
            Err(NeedsConfigure(_)) => self.give_up(now),
        }
    }

    /// A device whose endpoint could not be restarted, off the bus — but with
    /// its port still marked attached, because a port whose belief goes empty
    /// with the device still in it reads as a fresh connect and gets the same
    /// endpoint enumerated again every debounce.
    fn give_up(&mut self, now: Nanos) {
        self.let_go += 1;
        self.endpoint = None;
        if self.state.take_slot().is_some() {
            self.submit(What::LetGo, now);
        }
    }

    fn issue(&mut self, seq: Recovery, act: Act, now: Nanos) {
        match act {
            Act::Command(cmd) => {
                self.commands.push(cmd.name());
                self.submit(What::Recovering(seq), now);
            }
            Act::ClearHalt => {
                self.commands.push("CLEAR_FEATURE(ENDPOINT_HALT)");
                self.submit(What::Recovering(seq), now);
            }
            Act::Running => {
                self.requeues += 1;
                self.endpoint = Some(EndpointState::Running);
            }
        }
    }

    /// Drop a recovery outstanding for a device whose port has gone. The
    /// command it waits for will not be answered by anything still on the bus,
    /// and the teardown behind it would spend the whole deadline finding out.
    fn cancel_recovery(&mut self) {
        if self.never_cancels {
            return;
        }
        if !matches!(self.outstanding.what(), Some(What::Recovering(_))) {
            return;
        }
        self.outstanding.cancel();
        self.cancelled += 1;
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

//! One endpoint's way back to a state that runs TRBs.
//!
//! **Which command is legal is a property of the endpoint's state, not of
//! whatever ended the transfer.** Reset Endpoint is defined only for a Halted
//! endpoint (xHCI 1.2 §4.6.8), Stop Endpoint only for a Running one (§4.6.9),
//! and Set TR Dequeue Pointer for an endpoint already Stopped or in Error
//! (§4.6.10). A recovery that opens with Reset Endpoint every time gets Context
//! State Error whenever the break was not a halt, which the T14 answered twice
//! before calling its own boot disk offline.
//!
//! The sequence is here and its effects are the driver's, because it has two
//! drivers: a blocking loop for a disk's bulk pair, which runs on a faulting
//! thread where waiting is somebody's own time, and a stepped one for a HID
//! interrupt endpoint, which runs at the top of a scheduler pass where it is
//! everybody's.

/// What the controller believes about one endpoint, out of the Endpoint State
/// field of its *output* context (xHCI 1.2 Table 6-8, dword 0 bits 2:0).
///
/// Read rather than inferred from whatever ended the transfer, because the two
/// disagree exactly where it matters: a transfer the driver abandoned on its
/// deadline leaves no completion code at all and an endpoint that is still
/// Running.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EndpointState {
    Disabled,
    Running,
    Halted,
    Stopped,
    /// 4 is Error, 5-7 are reserved and no xHC should report one. Neither has a
    /// way back that does not re-run Configure Endpoint, so they are one case
    /// here — and the number is carried because it is what the refusal names.
    Unusable(u8),
}

impl EndpointState {
    pub fn decode(raw: u32) -> Self {
        match raw & 0x7 {
            0 => Self::Disabled,
            1 => Self::Running,
            2 => Self::Halted,
            3 => Self::Stopped,
            other => Self::Unusable(other as u8),
        }
    }
}

impl core::fmt::Display for EndpointState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Disabled => f.write_str("Disabled"),
            Self::Running => f.write_str("Running"),
            Self::Halted => f.write_str("Halted"),
            Self::Stopped => f.write_str("Stopped"),
            Self::Unusable(n) => write!(f, "endpoint state {n}"),
        }
    }
}

/// A command a recovery issues against (slot, dci).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    ResetEndpoint,
    StopEndpoint,
    /// Issued against a **freshly built ring**, never the one that broke: the
    /// TRBs behind the transfer that failed belong to nobody, and this command
    /// is what tells the controller so. Rebuilding without it leaves the
    /// controller's dequeue pointer in the middle of memory the driver has
    /// since zeroed.
    SetDequeue,
}

impl Command {
    /// What the line about a failure of this command calls it.
    pub fn name(self) -> &'static str {
        match self {
            Self::ResetEndpoint => "Reset Endpoint",
            Self::StopEndpoint => "Stop Endpoint",
            Self::SetDequeue => "Set TR Dequeue",
        }
    }
}

/// What the driver does next for the endpoint being recovered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Act {
    Command(Command),
    /// CLEAR_FEATURE(ENDPOINT_HALT) on EP0, naming the address the *device*
    /// knows this endpoint by. It clears the condition at the device, so it
    /// goes out only after a halt: an endpoint that never halted has nothing to
    /// clear and may stall the request for asking.
    ///
    /// **The only act of a recovery that puts a packet on the bus**, which is
    /// the line a class driver has to cut its own device reset in at. Bulk-Only
    /// Transport's Reset Recovery (BOT §5.3.4) is a class request followed by a
    /// CLEAR_FEATURE on each bulk endpoint, and that class request may not go
    /// out while either endpoint still holds a transfer the driver stopped
    /// waiting for: the device answers that transfer afterwards, and the answer
    /// lands on a state machine the reset has already rewound, so the transfer
    /// being recovered from is what undoes the recovery. A command changes
    /// nothing on the bus, so both endpoints' commands run first and the reset
    /// goes between the halves — a split of this sequence rather than a
    /// reordering of it, which is `the_bus_is_reached_only_after_every_command`.
    ClearHalt,
    /// The endpoint runs again. The driver queues its next transfer.
    Running,
}

/// An endpoint no sequence of commands takes back to Running.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NeedsConfigure(pub EndpointState);

/// Which operation of the sequence is outstanding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum At {
    /// Whatever takes the endpoint out of the state it broke in.
    Quiesce,
    /// Set TR Dequeue, which every route passes through.
    Dequeue,
    /// The device's own halt, which only a halted endpoint has.
    Halt,
}

/// One endpoint's recovery, as the operations still owed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Recovery {
    /// Whether the endpoint was Halted when this began, which is the only thing
    /// the later steps still need to know about where it started.
    halted: bool,
    at: At,
}

impl Recovery {
    /// The recovery an endpoint in `state` needs, and its first operation.
    pub fn begin(state: EndpointState) -> Result<(Self, Act), NeedsConfigure> {
        Ok(match state {
            EndpointState::Halted => (
                Self { halted: true, at: At::Quiesce },
                Act::Command(Command::ResetEndpoint),
            ),
            EndpointState::Running => (
                Self { halted: false, at: At::Quiesce },
                Act::Command(Command::StopEndpoint),
            ),
            // Already out of the way, so the ring is all that is left. Nothing
            // is issued to move it, because Stop Endpoint against a Stopped
            // endpoint is the Context State Error this type exists to avoid.
            EndpointState::Stopped => (
                Self { halted: false, at: At::Dequeue },
                Act::Command(Command::SetDequeue),
            ),
            state @ (EndpointState::Disabled | EndpointState::Unusable(_)) => {
                return Err(NeedsConfigure(state))
            }
        })
    }

    /// The operation just issued completed. What is owed next.
    pub fn completed(&mut self) -> Act {
        match self.at {
            At::Quiesce => {
                self.at = At::Dequeue;
                Act::Command(Command::SetDequeue)
            }
            At::Dequeue if self.halted => {
                self.at = At::Halt;
                Act::ClearHalt
            }
            At::Dequeue | At::Halt => Act::Running,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every operation a recovery from `state` issues, in order, ending at the
    /// act that says the endpoint runs again. The array is sized past the
    /// longest route so a sequence that grew one would run off it rather than
    /// being silently truncated into a passing comparison.
    const LONGEST: usize = 6;

    fn route(state: EndpointState) -> Result<([Act; LONGEST], usize), NeedsConfigure> {
        let (mut seq, first) = Recovery::begin(state)?;
        let mut acts = [first; LONGEST];
        let mut n = 1;
        while acts[n - 1] != Act::Running {
            assert!(n < LONGEST, "a recovery that does not terminate: {acts:?}");
            acts[n] = seq.completed();
            n += 1;
        }
        Ok((acts, n))
    }

    #[test]
    fn a_halted_endpoint_is_reset_and_then_cleared_at_the_device() {
        let (acts, n) = route(EndpointState::Halted).expect("recoverable");
        assert_eq!(
            &acts[..n],
            [
                Act::Command(Command::ResetEndpoint),
                Act::Command(Command::SetDequeue),
                Act::ClearHalt,
                Act::Running,
            ]
        );
    }

    /// The route a transfer the driver abandoned on its deadline leaves: the
    /// endpoint never halted, so Reset Endpoint would be a Context State Error
    /// and CLEAR_FEATURE would ask a device to clear a halt it does not have.
    #[test]
    fn a_running_endpoint_is_stopped_and_never_cleared() {
        let (acts, n) = route(EndpointState::Running).expect("recoverable");
        assert_eq!(
            &acts[..n],
            [
                Act::Command(Command::StopEndpoint),
                Act::Command(Command::SetDequeue),
                Act::Running,
            ]
        );
    }

    #[test]
    fn a_stopped_endpoint_only_needs_its_ring_back() {
        let (acts, n) = route(EndpointState::Stopped).expect("recoverable");
        assert_eq!(&acts[..n], [Act::Command(Command::SetDequeue), Act::Running]);
    }

    /// Nothing short of Configure Endpoint takes an endpoint out of either, and
    /// this driver does not re-configure a bound device — so the refusal has to
    /// name the state rather than being a silent give-up.
    #[test]
    fn disabled_and_reserved_states_are_refused_by_name() {
        assert_eq!(
            Recovery::begin(EndpointState::Disabled).err(),
            Some(NeedsConfigure(EndpointState::Disabled))
        );
        for n in 4..=7 {
            assert_eq!(
                Recovery::begin(EndpointState::Unusable(n)).err(),
                Some(NeedsConfigure(EndpointState::Unusable(n)))
            );
        }
    }

    /// Every route that reaches Running passes through Set TR Dequeue exactly
    /// once. Skipping it resumes the endpoint on the TRBs behind the transfer
    /// that broke, which is the failure no completion code reports.
    #[test]
    fn every_recovery_rebuilds_the_ring_exactly_once() {
        for state in [EndpointState::Halted, EndpointState::Running, EndpointState::Stopped] {
            let (acts, n) = route(state).expect("recoverable");
            let dequeues =
                acts[..n].iter().filter(|a| **a == Act::Command(Command::SetDequeue)).count();
            assert_eq!(dequeues, 1, "{state:?} issued {dequeues} Set TR Dequeue: {acts:?}");
        }
    }

    /// A class driver with a device-level reset of its own runs the commands
    /// both its endpoints owe, then that reset, then whatever is left — and
    /// that is a *split* of this sequence rather than a reordering of it only
    /// while no command follows an act that is on the bus. Bulk-Only
    /// Transport's `reset_recovery` is the driver of it, and if this ever grew
    /// a command after `ClearHalt` that driver would issue it before the
    /// CLEAR_FEATURE without saying so.
    #[test]
    fn the_bus_is_reached_only_after_every_command() {
        for state in [EndpointState::Halted, EndpointState::Running, EndpointState::Stopped] {
            let (acts, n) = route(state).expect("recoverable");
            let Some(bus) = acts[..n].iter().position(|a| *a == Act::ClearHalt) else {
                continue;
            };
            let last_command =
                acts[..n].iter().rposition(|a| matches!(a, Act::Command(_))).expect("a command");
            assert!(
                bus > last_command,
                "{state:?} owes a command after it has spoken to the device: {acts:?}"
            );
        }
    }

    #[test]
    fn only_a_halted_endpoint_is_cleared_at_the_device() {
        for state in [EndpointState::Running, EndpointState::Stopped] {
            let (acts, n) = route(state).expect("recoverable");
            assert!(!acts[..n].contains(&Act::ClearHalt), "{state:?} cleared a halt it never had");
        }
    }

    #[test]
    fn the_endpoint_state_field_decodes_to_every_value_three_bits_can_hold() {
        let want = [
            EndpointState::Disabled,
            EndpointState::Running,
            EndpointState::Halted,
            EndpointState::Stopped,
            EndpointState::Unusable(4),
            EndpointState::Unusable(5),
            EndpointState::Unusable(6),
            EndpointState::Unusable(7),
        ];
        for (raw, state) in want.iter().enumerate() {
            assert_eq!(EndpointState::decode(raw as u32), *state);
        }
        // The field is three bits of a dword whose other 29 carry the dequeue
        // pointer and the interval, so a decode that read more than its own
        // field would answer differently for the same endpoint.
        assert_eq!(EndpointState::decode(0xFFFF_FFF9), EndpointState::Running);
    }
}

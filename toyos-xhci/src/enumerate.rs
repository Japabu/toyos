//! Everything between a port that has finished its reset and a device the
//! driver can bind, as an order and the two places it branches.
//!
//! The shape is [`crate::recovery`]'s and for the same reason: the sequence has
//! two drivers. A boot scan runs it to the end in place, because there is no
//! scheduler yet to give a pass back to; a hot plug runs one act per scheduler
//! pass, because there is, and `poll_if_pending` is at the top of every one of
//! them. A second implementation for the second driver would be two orders to
//! keep in step, and the one that only runs on hardware nobody has plugged into
//! is the one that would rot.
//!
//! **What is here is the order and nothing else.** Which endpoints an interface
//! offered, what a slot id is, where a ring lives — none of that decides what
//! comes next, so none of it is here. `specs/plans/xhci-port-machine-plan.md` X2b is
//! the design.

/// EP0's Max Packet Size before the device has been asked, and `None` for a
/// speed this driver has no encoding for.
///
/// Low, High and SuperSpeed each fix it — 8, 64 and 512 — and a device of that
/// speed may report nothing else. **Full Speed does not.** `bMaxPacketSize0` is
/// 8, 16, 32 or 64 there, and it is not known until the first eight bytes of
/// the device descriptor have been read, which is itself a transfer over EP0.
/// 8 is the size every full-speed device can answer at, so it is what Address
/// Device carries and what Evaluate Context replaces once the device has said
/// (xHCI 1.2 §4.3.4; Linux does the same in `xhci_setup_addressable_virt_dev`).
///
/// The old table answered 64 for Full Speed and **8 for everything it did not
/// recognise**, so a SuperSpeedPlus port — speed 5, 6 or 7, which is every
/// Gen 2 and every two-lane link — was addressed with a 64-fold undersized
/// control endpoint and no line said so.
pub fn initial_ep0_packet(speed: u8) -> Option<u16> {
    match speed {
        1 | 2 => Some(8),
        3 => Some(64),
        4..=7 => Some(512),
        _ => None,
    }
}

/// What `bMaxPacketSize0` means at this speed, or `None` when the device named
/// a size a device of that speed does not have.
///
/// SuperSpeed states the *exponent*: the byte is 9 and the size is 512 (USB 3.2
/// §9.6.1). Everything below it states the size itself, and only Full Speed has
/// a choice to state.
pub fn ep0_packet_from_descriptor(speed: u8, stated: u8) -> Option<u16> {
    match (speed, stated) {
        (1, 8 | 16 | 32 | 64) => Some(stated as u16),
        (2, 8) => Some(8),
        (3, 64) => Some(64),
        (4..=7, 9) => Some(512),
        _ => None,
    }
}

/// A command the enumeration issues on the command ring.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    EnableSlot,
    AddressDevice,
    /// The command that changes exactly EP0's Max Packet Size on a device that
    /// is already addressed (xHCI 1.2 §4.6.7). Configure Endpoint would work on
    /// a device that is not yet configured and says far more than is meant.
    EvaluateEp0,
    ConfigureEndpoint,
}

/// A control request the enumeration issues on EP0.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request {
    /// GET_DESCRIPTOR(Device) for `want` bytes. **Two sizes and not one**: the
    /// eighth byte is `bMaxPacketSize0`, and on a full-speed device it is what
    /// decides whether a longer read can be transferred at all — so the prefix
    /// is read at a size every device of the speed can answer at.
    DeviceDescriptor { want: u16 },
    ConfigDescriptor,
    SetConfiguration,
    /// SET_PROTOCOL(boot), which only an interface that has a boot protocol
    /// has: asking a tablet for one is a request it may stall for.
    SetProtocol,
}

/// What the driver does next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Act {
    Command(Command),
    Request(Request),
}

/// What a configuration descriptor offered, as far as *the order of what is
/// left* depends on it.
///
/// Which endpoints, which interface number and which packet sizes are the
/// driver's, because nothing here decides anything from them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Function {
    /// A HID interface with a boot protocol to select.
    BootHid,
    /// A HID interface with none — a tablet, which reports in its own format.
    Hid,
    Msc,
}

/// What the driver learnt from the act it just performed, where the order of
/// what is left depends on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Learnt {
    /// Nothing the order depends on. Every act but two produces this.
    Nothing,
    /// The device states an EP0 packet size other than the one Address Device
    /// carried, so the controller has to be told.
    Ep0PacketWrong,
    /// The configuration descriptor named a function this driver can bind.
    Function(Function),
}

/// Where the sequence goes after the act that has just completed.
///
/// A sum and not another [`Act`] variant, because the two ends are terminal and
/// this is what makes asking a finished sequence for another act unrepresentable
/// rather than a state to be handled. [`crate::recovery`] needs no such thing:
/// its terminal is a state the endpoint is *in*, so answering it twice says the
/// same true thing, and answering `Bind` twice would bind a device twice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Next {
    /// Do this, then come back with what it produced.
    Act(Enumeration, Act),
    /// Everything every device needs is done. What is left is the class bind,
    /// which is the driver's.
    Bind,
    /// The configuration held no interface this driver can drive. The slot is
    /// still the controller's and only Disable Slot gives it back.
    Refuse,
}

/// Which act of the sequence is outstanding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum At {
    Slot,
    Address,
    Prefix,
    Ep0,
    Descriptor,
    Config,
    Configuration,
    Protocol,
    Endpoints,
}

/// One device's enumeration, as the acts still owed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Enumeration {
    at: At,
    /// Whether this device has a boot protocol to select, which is the only
    /// thing the later acts still need to know about what the configuration
    /// said.
    boot_protocol: bool,
}

impl Enumeration {
    /// The first act every device needs, whatever it turns out to be.
    pub fn begin() -> (Self, Act) {
        (
            Self { at: At::Slot, boot_protocol: false },
            Act::Command(Command::EnableSlot),
        )
    }

    /// The act just performed succeeded, and this is what it taught. What is
    /// owed next.
    pub fn completed(mut self, learnt: Learnt) -> Next {
        let (at, act) = match self.at {
            At::Slot => (At::Address, Act::Command(Command::AddressDevice)),
            At::Address => {
                (At::Prefix, Act::Request(Request::DeviceDescriptor { want: 8 }))
            }
            At::Prefix if learnt == Learnt::Ep0PacketWrong => {
                (At::Ep0, Act::Command(Command::EvaluateEp0))
            }
            At::Prefix | At::Ep0 => {
                (At::Descriptor, Act::Request(Request::DeviceDescriptor { want: 18 }))
            }
            At::Descriptor => (At::Config, Act::Request(Request::ConfigDescriptor)),
            At::Config => {
                let Learnt::Function(function) = learnt else { return Next::Refuse };
                self.boot_protocol = function == Function::BootHid;
                (At::Configuration, Act::Request(Request::SetConfiguration))
            }
            At::Configuration if self.boot_protocol => {
                (At::Protocol, Act::Request(Request::SetProtocol))
            }
            At::Configuration | At::Protocol => {
                (At::Endpoints, Act::Command(Command::ConfigureEndpoint))
            }
            At::Endpoints => return Next::Bind,
        };
        Next::Act(Self { at, ..self }, act)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sized past the longest route so a sequence that grew one runs off it
    /// rather than being silently truncated into a passing comparison.
    const LONGEST: usize = 12;

    /// A route, as the acts it produced and how it ended. `Copy` so the
    /// assertions below can compare slices of it without a heap this crate does
    /// not have.
    struct Route {
        acts: [Act; LONGEST],
        n: usize,
        end: Next,
    }

    impl Route {
        fn acts(&self) -> &[Act] {
            &self.acts[..self.n]
        }

        fn count(&self, act: Act) -> usize {
            self.acts().iter().filter(|a| **a == act).count()
        }
    }

    /// Every act a device that answers `learnt` at each branch produces, in
    /// order, and how the sequence ended.
    fn route(mut learnt: impl FnMut(Act) -> Learnt) -> Route {
        let (mut seq, first) = Enumeration::begin();
        let mut acts = [first; LONGEST];
        let mut n = 1;
        loop {
            match seq.completed(learnt(acts[n - 1])) {
                Next::Act(next, act) => {
                    assert!(n < LONGEST, "a sequence that does not terminate: {acts:?}");
                    seq = next;
                    acts[n] = act;
                    n += 1;
                }
                end => return Route { acts, n, end },
            }
        }
    }

    /// Answer every branch the way a keyboard whose EP0 packet size the
    /// controller was told correctly does.
    fn keyboard(act: Act) -> Learnt {
        match act {
            Act::Request(Request::ConfigDescriptor) => Learnt::Function(Function::BootHid),
            _ => Learnt::Nothing,
        }
    }

    #[test]
    fn a_boot_keyboard_is_addressed_described_configured_and_bound() {
        let route = route(keyboard);
        assert_eq!(route.end, Next::Bind);
        assert_eq!(
            route.acts(),
            [
                Act::Command(Command::EnableSlot),
                Act::Command(Command::AddressDevice),
                Act::Request(Request::DeviceDescriptor { want: 8 }),
                Act::Request(Request::DeviceDescriptor { want: 18 }),
                Act::Request(Request::ConfigDescriptor),
                Act::Request(Request::SetConfiguration),
                Act::Request(Request::SetProtocol),
                Act::Command(Command::ConfigureEndpoint),
            ]
        );
    }

    /// The full-speed case Evaluate Context exists for: Address Device carried
    /// 8 because that is all a full-speed device is guaranteed to answer at,
    /// and the device states 64.
    #[test]
    fn a_device_that_states_another_packet_size_has_its_ep0_evaluated() {
        let route = route(|act| match act {
            Act::Request(Request::DeviceDescriptor { want: 8 }) => Learnt::Ep0PacketWrong,
            other => keyboard(other),
        });
        assert_eq!(route.end, Next::Bind);
        assert_eq!(route.acts()[2..5], [
            Act::Request(Request::DeviceDescriptor { want: 8 }),
            Act::Command(Command::EvaluateEp0),
            Act::Request(Request::DeviceDescriptor { want: 18 }),
        ]);
        // And exactly once: a second Evaluate Context would be asked from the
        // state the first one left.
        assert_eq!(route.count(Act::Command(Command::EvaluateEp0)), 1);
    }

    /// A disk and a tablet have no boot protocol to select, and asking for one
    /// is a request the device may stall for.
    #[test]
    fn only_an_interface_with_a_boot_protocol_is_asked_for_one() {
        for function in [Function::Msc, Function::Hid] {
            let route = route(|act| match act {
                Act::Request(Request::ConfigDescriptor) => Learnt::Function(function),
                _ => Learnt::Nothing,
            });
            assert_eq!(route.end, Next::Bind);
            assert_eq!(
                route.count(Act::Request(Request::SetProtocol)),
                0,
                "{function:?} was asked for a boot protocol it has none of: {:?}",
                route.acts()
            );
        }
    }

    /// Every route configures the endpoints exactly once and immediately before
    /// the bind. A bind that ran first would put a device's transfer rings in
    /// pool memory no endpoint context names yet.
    #[test]
    fn every_route_configures_its_endpoints_last() {
        for function in [Function::BootHid, Function::Hid, Function::Msc] {
            let route = route(|act| match act {
                Act::Request(Request::ConfigDescriptor) => Learnt::Function(function),
                _ => Learnt::Nothing,
            });
            assert_eq!(route.acts().last(), Some(&Act::Command(Command::ConfigureEndpoint)));
            let configures = route.count(Act::Command(Command::ConfigureEndpoint));
            assert_eq!(configures, 1, "{function:?} issued {configures}: {:?}", route.acts());
        }
    }

    /// A device offering nothing this driver binds — a hub, a camera — stops
    /// the sequence where the answer is known, rather than configuring a
    /// device with no interface behind it.
    #[test]
    fn a_configuration_with_nothing_to_bind_is_refused_where_it_is_read() {
        let route = route(|_| Learnt::Nothing);
        assert_eq!(route.end, Next::Refuse);
        assert_eq!(route.acts().last(), Some(&Act::Request(Request::ConfigDescriptor)));
    }

    /// Address Device carries a packet size and only these speeds have one, so
    /// a port that comes up at any other is refused before a slot is spent.
    #[test]
    fn every_speed_has_one_control_packet_size_or_none() {
        let want = [None, Some(8), Some(8), Some(64), Some(512), Some(512), Some(512), Some(512)];
        for (speed, packet) in want.iter().enumerate() {
            assert_eq!(initial_ep0_packet(speed as u8), *packet, "speed {speed}");
        }
        for speed in 8..=15u8 {
            assert_eq!(initial_ep0_packet(speed), None, "speed {speed}");
        }
    }

    /// The byte is the *size* below SuperSpeed and the *exponent* at it, and
    /// only Full Speed has a choice to state.
    #[test]
    fn a_stated_packet_size_a_device_of_that_speed_cannot_have_is_refused() {
        for stated in [8u8, 16, 32, 64] {
            assert_eq!(ep0_packet_from_descriptor(1, stated), Some(stated as u16));
        }
        assert_eq!(ep0_packet_from_descriptor(1, 9), None);
        assert_eq!(ep0_packet_from_descriptor(2, 8), Some(8));
        assert_eq!(ep0_packet_from_descriptor(2, 64), None);
        assert_eq!(ep0_packet_from_descriptor(3, 64), Some(64));
        assert_eq!(ep0_packet_from_descriptor(3, 8), None);
        for speed in 4..=7u8 {
            assert_eq!(ep0_packet_from_descriptor(speed, 9), Some(512));
            assert_eq!(ep0_packet_from_descriptor(speed, 512u16 as u8), None);
        }
        assert_eq!(ep0_packet_from_descriptor(0, 8), None);
    }
}

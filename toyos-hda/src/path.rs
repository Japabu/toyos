//! Choosing the converter and pins that carry sound to a speaker.
//!
//! `specs/hda-driver-plan.md` §2.3 is the algorithm and §5.2 is why it lives
//! here: it is the least-covered code in that plan, and the one real machine
//! walks only its shallowest case. Everything below the depth-1 case is
//! covered by synthetic graphs and by nothing else.

use alloc::vec::Vec;

use crate::caps::{AmpCaps, DefaultDevice};
use crate::graph::{Codec, FunctionGroup, FunctionKind, MAX_WIDGETS};
use crate::verb::{Address, Node};

/// A widget that has to be told which of its inputs to take.
///
/// Only widgets with more than one connection appear: one connection has no
/// Connection Select to write, and a driver that wrote one anyway would be
/// setting a control the specification does not give that widget.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hop {
    pub node: Node,
    pub select: u8,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PinSetup {
    pub node: Node,
    /// From the pin inward to the converter. Empty when every widget on the
    /// way has a single connection, which is both of the T14's output paths.
    pub route: Vec<Hop>,
    pub amp: Option<AmpCaps>,
    /// This pin powers an external amplifier and **must be told to**: a
    /// speaker pin left with EAPD clear is a correctly configured path that
    /// makes no sound.
    pub eapd: bool,
    pub headphone_drive: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OutputPath {
    pub codec: Address,
    pub group: Node,
    pub converter: Node,
    pub speaker: PinSetup,
    /// A headphone pin the same converter feeds, when the codec offers one.
    /// §2.5's routing wants both ends of one converter rather than two
    /// converters that would have to be kept in step.
    pub headphone: Option<PinSetup>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PathError {
    /// No codec on the link had an audio function group with a speaker pin
    /// sound can leave by. Names every codec that answered, because "no
    /// audio" without the list is a report nobody can act on.
    NoSpeakerPin { codecs: Vec<Address> },
    /// A connection list named a node the function group did not declare.
    OutsideGroup { pin: Node, named: Node },
    /// A connection list leads back to a node already on the path.
    Cycle { pin: Node, at: Node },
    /// A pin sound cannot be traced from to any converter.
    NoConverter { pin: Node },
}

/// Every codec, every audio function group, and the speaker pin one of them
/// yields — never the first codec that answers.
///
/// Display audio is a codec on the same controller with a perfectly valid
/// output path and no speaker, so "the first one" is a driver that configures
/// a working path and produces silence.
pub fn find_output_path(codecs: &[Codec]) -> Result<OutputPath, PathError> {
    let mut fault: Option<PathError> = None;
    for codec in codecs {
        for group in codec.groups.iter().filter(|g| g.kind == FunctionKind::Audio) {
            match group_output(codec.address, group) {
                Ok(path) => return Ok(path),
                Err(Some(e)) => {
                    fault.get_or_insert(e);
                }
                Err(None) => {}
            }
        }
    }
    // Any fault outranks "nothing found": a machine whose speaker pin leads
    // nowhere, or whose codec contradicted itself, is a different report from
    // one that has no speaker at all — and only the first two can be a bug
    // here rather than a property of the hardware.
    Err(fault.unwrap_or_else(|| PathError::NoSpeakerPin {
        codecs: codecs.iter().map(|c| c.address).collect(),
    }))
}

/// `Err(None)` is a group with no wired speaker pin to try, which is not a
/// fault — display audio is exactly that and it is the ordinary case on a
/// machine with two codecs.
fn group_output(codec: Address, group: &FunctionGroup) -> Result<OutputPath, Option<PathError>> {
    let mut fault: Option<PathError> = None;
    for pin in outputs(group, DefaultDevice::Speaker) {
        let (converter, route) = match trace(group, pin) {
            Ok(found) => found,
            Err(e) => {
                fault.get_or_insert(e);
                continue;
            }
        };
        let speaker = setup(group, pin, route);
        let headphone = outputs(group, DefaultDevice::HeadphoneOut)
            .filter_map(|hp| {
                let (found, route) = trace(group, hp).ok()?;
                (found == converter).then(|| setup(group, hp, route))
            })
            // The codec's own statement that two pins are one output. Where it
            // offers several, the one it grouped with the speaker is meant.
            .min_by_key(|hp| {
                let same = association(group, hp.node) == association(group, pin);
                (!same, hp.node.0)
            });
        return Ok(OutputPath { codec, group: group.node, converter, speaker, headphone });
    }
    Err(fault)
}

fn association(group: &FunctionGroup, node: Node) -> Option<u8> {
    Some(group.widget(node)?.pin.as_ref()?.config.association)
}

/// Pins of one default device that sound can actually leave by.
///
/// The connectivity check is the load-bearing one and the default device is
/// not: four pins on the T14's codec call themselves speakers with no physical
/// connection, one of them with a valid connection list and an output
/// amplifier behind it.
pub fn outputs(group: &FunctionGroup, device: DefaultDevice) -> impl Iterator<Item = Node> + '_ {
    group.widgets.iter().filter_map(move |w| {
        let pin = w.pin.as_ref()?;
        (pin.config.is_physical() && pin.config.device == device && pin.caps.output)
            .then_some(w.node)
    })
}

fn setup(group: &FunctionGroup, node: Node, route: Vec<Hop>) -> PinSetup {
    let widget = group.widget(node).expect("traced through this widget");
    let pin = widget.pin.as_ref().expect("outputs() yielded a pin complex");
    PinSetup {
        node,
        route,
        amp: widget.amp_out,
        eapd: pin.caps.eapd,
        headphone_drive: pin.caps.headphone_drive,
    }
}

/// Walk backwards from a pin to a converter, depth first.
///
/// Bounded by the widget count and refusing a node already on the current
/// path: a codec's connection list is untrusted, and a cycle in it has to be a
/// refusal rather than a hang. A branch that faults is remembered and the
/// search continues, so one bad entry does not cost a graph that has a good
/// route — but the fault is what the caller is told about if nothing works.
fn trace(group: &FunctionGroup, pin: Node) -> Result<(Node, Vec<Hop>), PathError> {
    let mut seen = Vec::new();
    let mut fault = None;
    let found = descend(group, pin, pin, &mut seen, &mut fault);
    found.ok_or_else(|| fault.unwrap_or(PathError::NoConverter { pin }))
}

fn descend(
    group: &FunctionGroup,
    pin: Node,
    node: Node,
    seen: &mut Vec<Node>,
    fault: &mut Option<PathError>,
) -> Option<(Node, Vec<Hop>)> {
    if seen.contains(&node) {
        fault.get_or_insert(PathError::Cycle { pin, at: node });
        return None;
    }
    if seen.len() >= MAX_WIDGETS {
        return None;
    }
    let Some(widget) = group.widget(node) else {
        fault.get_or_insert(PathError::OutsideGroup { pin, named: node });
        return None;
    };
    if widget.is_converter() {
        return Some((node, Vec::new()));
    }

    seen.push(node);
    let mut found = None;
    for (index, &next) in widget.connections.iter().enumerate() {
        if let Some((converter, mut route)) = descend(group, pin, next, seen, fault) {
            if widget.connections.len() > 1 {
                route.insert(0, Hop { node, select: index as u8 });
            }
            found = Some((converter, route));
            break;
        }
    }
    seen.pop();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    #[test]
    fn the_t14_picks_its_internal_speaker_and_the_jack_that_shares_a_converter() {
        let codecs = fixture::t14();
        let path = find_output_path(&codecs).expect("the T14 has a speaker");

        assert_eq!(path.codec, Address::new(0).unwrap());
        assert_eq!(path.converter, Node(0x02));
        assert_eq!(path.speaker.node, Node(0x14));
        // One connection, so there is no Connection Select to write.
        assert!(path.speaker.route.is_empty());
        // Both output pins power an external amplifier, and neither is on.
        assert!(path.speaker.eapd);
        assert!(!path.speaker.headphone_drive);

        let headphone = path.headphone.expect("the T14 has a headphone jack");
        assert_eq!(headphone.node, Node(0x21));
        // Two connections, and the converter is the first.
        assert_eq!(headphone.route, [Hop { node: Node(0x21), select: 0 }]);
        assert!(headphone.headphone_drive);
        assert!(headphone.eapd);
    }

    #[test]
    fn the_pin_amp_the_t14_offers_is_the_one_that_can_mute() {
        let path = find_output_path(&fixture::t14()).unwrap();
        let amp = path.speaker.amp.expect("the speaker pin has an output amp");
        assert!(amp.mute);
        assert_eq!(amp.gain, None);
    }

    #[test]
    fn display_audio_alone_is_a_refusal_naming_it() {
        // The T14's second codec on its own: a valid output path, a digital
        // pin, no speaker. This is the machine a first-match driver configures
        // perfectly and hears nothing from.
        let display: Vec<Codec> =
            fixture::t14().into_iter().filter(|c| c.address.raw() == 2).collect();
        match find_output_path(&display) {
            Err(PathError::NoSpeakerPin { codecs }) => {
                assert_eq!(codecs, [Address::new(2).unwrap()]);
            }
            other => panic!("display audio must be refused by name, got {other:?}"),
        }
    }

    #[test]
    fn the_display_codec_does_not_win_when_it_comes_first() {
        // Order reversed, so a walk that stopped at the first codec would bind
        // the wrong one. The T14 puts the analogue codec at the lower address,
        // so this is the arm the machine itself cannot exercise.
        let mut codecs = fixture::t14();
        codecs.reverse();
        let path = find_output_path(&codecs).unwrap();
        assert_eq!(path.codec, Address::new(0).unwrap());
        assert_eq!(path.speaker.node, Node(0x14));
    }

    #[test]
    fn only_one_of_the_t14_s_five_speaker_labelled_pins_is_a_candidate() {
        // Asserted on the candidate *set* and not on the chosen pin: the real
        // speaker is 0x14 and the unwired ones are 0x12, 0x18, 0x1a and 0x1b,
        // so the pin that wins comes first in node order anyway and deleting
        // the connectivity check leaves the choice unchanged. This is what
        // makes the check tested rather than merely present.
        let codecs = fixture::t14();
        let group = &codecs[0].groups[0];
        let candidates: Vec<Node> = outputs(group, DefaultDevice::Speaker).collect();
        assert_eq!(candidates, [Node(0x14)]);

        // 0x1b is the sharp one: it lists [2, 3], carries an output amp, and
        // traces to a converter perfectly well.
        let unwired = group.widget(Node(0x1b)).unwrap();
        assert_eq!(unwired.pin.as_ref().unwrap().config.device, DefaultDevice::Speaker);
        assert!(trace(group, Node(0x1b)).is_ok());
    }

    #[test]
    fn an_unwired_speaker_pin_does_not_win_by_coming_first() {
        // The ordering the T14 does not have. Without the connectivity check
        // the unwired pin is reached first and configured, and the machine is
        // silent with nothing in the log to say why.
        let codecs = fixture::synthetic_unwired_first();
        let path = find_output_path(&codecs).unwrap();
        assert_eq!(path.speaker.node, Node(0x21));
    }

    #[test]
    fn qemu_offers_no_speaker_pin_and_is_refused_by_name() {
        // **A finding, not a defect here.** Both of QEMU's codec models fix
        // their configuration default at line-out and no device property
        // changes it, so §2.3's speaker rule refuses the harness's own
        // machine. H4's QEMU arm therefore cannot bind an output with this
        // rule as written; the choice between widening it to line-out and
        // testing only the refusal belongs to that stage.
        match find_output_path(&fixture::qemu()) {
            Err(PathError::NoSpeakerPin { codecs }) => {
                assert_eq!(codecs, [Address::new(0).unwrap(), Address::new(1).unwrap()]);
            }
            other => panic!("QEMU has no speaker pin to find, got {other:?}"),
        }
    }

    #[test]
    fn qemu_s_line_out_pin_traces_to_a_converter_all_the_same() {
        // So the refusal above is about what the pin is *for* and not about a
        // graph this crate cannot walk — which is the difference between a
        // policy H4 may widen and a bug it would inherit.
        let codecs = fixture::qemu();
        let group = &codecs[0].groups[0];
        let line_out: Vec<Node> = outputs(group, DefaultDevice::LineOut).collect();
        assert_eq!(line_out, [Node(0x03)]);
        let (converter, route) = trace(group, Node(0x03)).unwrap();
        assert_eq!(converter, Node(0x02));
        assert!(route.is_empty());
    }

    #[test]
    fn a_cycle_is_a_refusal_and_not_a_hang() {
        let codecs = fixture::synthetic_cycle();
        match find_output_path(&codecs) {
            Err(PathError::Cycle { .. }) => {}
            other => panic!("a cyclic connection list must be refused, got {other:?}"),
        }
    }

    #[test]
    fn a_connection_naming_a_node_outside_the_group_is_refused_by_name() {
        match find_output_path(&fixture::synthetic_outside_group()) {
            Err(PathError::OutsideGroup { named, .. }) => assert_eq!(named, Node(0x7e)),
            other => panic!("an out-of-range connection must be named, got {other:?}"),
        }
    }

    #[test]
    fn a_selector_on_the_way_is_told_which_input_to_take() {
        // The shape no real machine here has: pin -> selector -> converter,
        // with the converter second so an implementation that always wrote 0
        // would route silence.
        let path = find_output_path(&fixture::synthetic_selector()).unwrap();
        assert_eq!(path.converter, Node(0x10));
        assert_eq!(
            path.speaker.route,
            [Hop { node: Node(0x20), select: 0 }, Hop { node: Node(0x30), select: 1 }]
        );
    }

    #[test]
    fn a_speaker_pin_with_no_converter_behind_it_is_refused() {
        match find_output_path(&fixture::synthetic_dead_end()) {
            Err(PathError::NoConverter { pin }) => assert_eq!(pin, Node(0x20)),
            other => panic!("a pin with no converter must be refused, got {other:?}"),
        }
    }
}

//! Walking a live codec into a [`Codec`].
//!
//! The traversal `specs/hda-driver-plan.md` §2.3 steps 1 and 2 describe, with
//! every device-supplied count bounded and every contradiction a refusal. It is
//! here rather than in the driver for the reason §5.2 gives about the path
//! search: what a codec is asked, in what order, and which answers are refused
//! are decisions, and a decision that can be a pure function is one a host test
//! runs in milliseconds.
//!
//! The I/O is the caller's, as one method. Everything below it is arithmetic.

use alloc::vec::Vec;

use crate::caps::{AmpCaps, ConfigDefault, PcmCaps, PinCaps, WidgetCaps, WidgetKind};
use crate::graph::{
    decode_connections, Codec, ConnectionListLen, FunctionGroup, FunctionKind, Pin, Widget,
    MAX_CONNECTIONS, MAX_FUNCTION_GROUPS, MAX_WIDGETS,
};
use crate::verb::{self, Address, NoSubordinates, Node, Response, Subordinates};

/// One verb out and one answer back.
///
/// `None` covers both ways an answer can fail to be one — a controller that
/// never completed the command, and the all-ones a link with nothing on it
/// reads as. Neither is a value to decode, and a walk that told them apart
/// would be deciding what to do about the difference, which is the driver's.
pub trait Verbs {
    fn get(&mut self, codec: Address, node: Node, verb: u16, payload: u8) -> Option<Response>;
}

/// Why a codec answered nothing usable. Carried per codec so a machine with a
/// wedged link beside a working one is still driven, and still says so.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodecFault {
    /// No answer to its vendor id: `STATESTS` named a codec the link does not
    /// carry, or the controller has no working verb interface.
    Silent,
    NoFunctionGroup,
    /// The codec's own subordinate-node range runs off the node id space, so a
    /// walk of it would wrap and read the root as a widget.
    RangePastNodeSpace,
}

/// Every codec `statests` reports, walked in address order.
///
/// A codec that answers nothing is a `Err` in the result rather than a gap:
/// "no audio" without the list of what answered is a report nobody can act on
/// (`path::PathError::NoOutputPin` says the same thing one layer up).
pub fn enumerate(
    verbs: &mut impl Verbs,
    statests: u16,
) -> Vec<Result<Codec, (Address, CodecFault)>> {
    verb::present(statests).map(|address| codec(verbs, address).map_err(|f| (address, f))).collect()
}

fn param(verbs: &mut impl Verbs, codec: Address, node: Node, which: u8) -> Option<Response> {
    verbs.get(codec, node, verb::GET_PARAMETER, which)
}

/// The subordinate range a node declares, clamped to what this walk visits.
///
/// The clamp is policy and the refusal is not: a count past the bound loses the
/// nodes past it, and a range running off the node id space is a codec
/// contradicting itself and yields nothing at all.
fn subordinates(
    verbs: &mut impl Verbs,
    codec: Address,
    node: Node,
    limit: usize,
) -> Result<Subordinates, CodecFault> {
    let response = param(verbs, codec, node, verb::PARAM_SUB_NODE_COUNT)
        .ok_or(CodecFault::NoFunctionGroup)?;
    match Subordinates::decode(response) {
        Ok(range) => Ok(Subordinates {
            first: range.first,
            count: (range.count as usize).min(limit) as u8,
        }),
        Err(NoSubordinates::Leaf) => Err(CodecFault::NoFunctionGroup),
        Err(NoSubordinates::PastNodeSpace { .. }) => Err(CodecFault::RangePastNodeSpace),
    }
}

fn codec(verbs: &mut impl Verbs, address: Address) -> Result<Codec, CodecFault> {
    let id = param(verbs, address, Node::ROOT, verb::PARAM_VENDOR_ID)
        .ok_or(CodecFault::Silent)?
        .raw();
    let groups = subordinates(verbs, address, Node::ROOT, MAX_FUNCTION_GROUPS)?;
    Ok(Codec {
        address,
        vendor: (id >> 16) as u16,
        device: id as u16,
        groups: groups.nodes().filter_map(|node| group(verbs, address, node)).collect(),
    })
}

/// A modem or vendor function group is kept with its kind and not walked: §2.3
/// step 1 says log and skip, and a group that vanished from the model could not
/// be named in a refusal.
fn group(verbs: &mut impl Verbs, codec: Address, node: Node) -> Option<FunctionGroup> {
    let kind = FunctionKind::decode(param(verbs, codec, node, verb::PARAM_FUNCTION_TYPE)?);
    if kind != FunctionKind::Audio {
        return Some(FunctionGroup {
            node,
            kind,
            range: Subordinates { first: node, count: 1 },
            widgets: Vec::new(),
        });
    }
    let range = subordinates(verbs, codec, node, MAX_WIDGETS).ok()?;
    Some(FunctionGroup {
        node,
        kind,
        range,
        widgets: range.nodes().filter_map(|w| widget(verbs, codec, w)).collect(),
    })
}

fn widget(verbs: &mut impl Verbs, codec: Address, node: Node) -> Option<Widget> {
    let caps = WidgetCaps::decode(param(verbs, codec, node, verb::PARAM_WIDGET_CAPS)?);
    // An amplifier a widget does not override is the function group's, and
    // asking the widget answers the group's word — which would put a gain range
    // on a pin that has none. `amp_override` is what says the answer is its own.
    let amp = |verbs: &mut _, present: bool, which: u8| {
        (present && caps.amp_override)
            .then(|| param(verbs, codec, node, which).map(AmpCaps::decode))
            .flatten()
    };
    Some(Widget {
        node,
        connections: caps.connection_list.then(|| connections(verbs, codec, node)).flatten()
            .unwrap_or_default(),
        amp_out: amp(verbs, caps.output_amp, verb::PARAM_AMP_OUT_CAPS),
        amp_in: amp(verbs, caps.input_amp, verb::PARAM_AMP_IN_CAPS),
        pcm: matches!(caps.kind, WidgetKind::AudioOutput | WidgetKind::AudioInput)
            .then(|| param(verbs, codec, node, verb::PARAM_PCM).map(PcmCaps::decode))
            .flatten(),
        pin: (caps.kind == WidgetKind::PinComplex).then(|| pin(verbs, codec, node)).flatten(),
        caps,
    })
}

/// A pin with no configuration default is no pin at all.
///
/// All ones decodes to a perfectly plausible pin — "jack, other, wired" — which
/// is a name for a read that did not happen, and §2.3 chooses a pin off exactly
/// this field.
fn pin(verbs: &mut impl Verbs, codec: Address, node: Node) -> Option<Pin> {
    Some(Pin {
        caps: PinCaps::decode(param(verbs, codec, node, verb::PARAM_PIN_CAPS)?),
        config: ConfigDefault::decode(verbs.get(codec, node, verb::GET_CONFIG_DEFAULT, 0)?),
    })
}

fn connections(verbs: &mut impl Verbs, codec: Address, node: Node) -> Option<Vec<Node>> {
    let declared = ConnectionListLen::decode(param(
        verbs,
        codec,
        node,
        verb::PARAM_CONNECTION_LENGTH,
    )?);
    if declared.count == 0 {
        return None;
    }
    let len = ConnectionListLen {
        count: (declared.count as usize).min(MAX_CONNECTIONS) as u8,
        long: declared.long,
    };
    let mut responses = Vec::with_capacity(len.responses());
    for index in 0..len.responses() {
        responses.push(verbs.get(
            codec,
            node,
            verb::GET_CONNECTION_LIST,
            (index * len.per_response()) as u8,
        )?);
    }
    decode_connections(len, &responses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::path::find_output_path;

    /// A codec that answers out of one of this crate's fixture logs.
    ///
    /// The point of driving the walk from the log rather than from a
    /// hand-written table is that every word below is one a real machine sent,
    /// so a wrong verb identifier or a wrong parameter number fails against the
    /// T14 rather than against prose.
    struct Fake {
        text: &'static str,
    }

    impl Fake {
        /// The `hda:` line for this node that carries `key=`.
        fn line(&self, codec: Address, node: Option<Node>, key: &str) -> Option<&'static str> {
            let head = match node {
                Some(node) => alloc::format!("hda: codec{codec} node={:#04x} ", node.0),
                None => alloc::format!("hda: codec{codec} "),
            };
            self.text
                .lines()
                .map(str::trim)
                .filter(|l| l.starts_with(&head))
                .find(|l| l.contains(&alloc::format!(" {key}=")))
        }

        fn word(&self, codec: Address, node: Option<Node>, key: &str) -> Option<u32> {
            let line = self.line(codec, node, key)?;
            let at = line.find(&alloc::format!(" {key}="))? + key.len() + 2;
            let text = line[at..].split(' ').next()?;
            u32::from_str_radix(text.strip_prefix("0x").unwrap_or(text), 16).ok()
        }

        /// The probe prints a decoded range; a codec answers the raw word.
        fn sub_nodes(&self, codec: Address, node: Node) -> Option<u32> {
            if node == Node::ROOT {
                let fgs: Vec<u8> = self
                    .text
                    .lines()
                    .map(str::trim)
                    .filter(|l| l.starts_with(&alloc::format!("hda: codec{codec} fg=")))
                    .filter_map(|l| {
                        let at = l.find(" fg=")? + 4;
                        u8::from_str_radix(l[at..].split(' ').next()?.trim_start_matches("0x"), 16)
                            .ok()
                    })
                    .collect();
                let first = *fgs.first()?;
                return Some(((first as u32) << 16) | fgs.len() as u32 / 2);
            }
            let line = self
                .text
                .lines()
                .map(str::trim)
                .find(|l| {
                    l.starts_with(&alloc::format!("hda: codec{codec} fg={:#04x} ", node.0))
                        && l.contains(" widgets=")
                })?;
            let at = line.find(" widgets=")? + 9;
            let (first, last) = line[at..].split(' ').next()?.split_once("..")?;
            let first = u8::from_str_radix(first.trim_start_matches("0x"), 16).ok()?;
            let last = u8::from_str_radix(last.trim_start_matches("0x"), 16).ok()?;
            Some(((first as u32) << 16) | (last - first + 1) as u32)
        }

        /// Re-encode the probe's decoded list into the wire's short form.
        fn connection(&self, codec: Address, node: Node, index: u8) -> Option<u32> {
            let line = self.line(codec, Some(node), "list")?;
            let list = line.split_once(" list=[")?.1.split(']').next()?;
            let nodes: Vec<u32> =
                list.split(',').filter_map(|n| n.trim().parse::<u32>().ok()).collect();
            let mut word = 0u32;
            for slot in 0..4 {
                if let Some(id) = nodes.get(index as usize + slot) {
                    word |= id << (slot * 8);
                }
            }
            Some(word)
        }
    }

    impl Verbs for Fake {
        fn get(
            &mut self,
            codec: Address,
            node: Node,
            command: u16,
            payload: u8,
        ) -> Option<Response> {
            let raw = match (command, payload) {
                (verb::GET_PARAMETER, verb::PARAM_VENDOR_ID) => {
                    let vendor = self.word(codec, None, "vendor")?;
                    let device = self.word(codec, None, "device")?;
                    (vendor << 16) | device
                }
                (verb::GET_PARAMETER, verb::PARAM_SUB_NODE_COUNT) => {
                    self.sub_nodes(codec, node)?
                }
                (verb::GET_PARAMETER, verb::PARAM_FUNCTION_TYPE) => {
                    let line = self
                        .text
                        .lines()
                        .map(str::trim)
                        .find(|l| {
                            l.starts_with(&alloc::format!("hda: codec{codec} fg={:#04x} ", node.0))
                                && l.contains(" type=")
                        })?;
                    let at = line.find(" type=")? + 6;
                    u32::from_str_radix(
                        line[at..].split(' ').next()?.trim_start_matches("0x"),
                        16,
                    )
                    .ok()?
                }
                (verb::GET_PARAMETER, verb::PARAM_WIDGET_CAPS) => {
                    self.word(codec, Some(node), "caps")?
                }
                (verb::GET_PARAMETER, verb::PARAM_AMP_OUT_CAPS) => {
                    self.word(codec, Some(node), "amp-out-caps")?
                }
                (verb::GET_PARAMETER, verb::PARAM_AMP_IN_CAPS) => {
                    self.word(codec, Some(node), "amp-in-caps")?
                }
                (verb::GET_PARAMETER, verb::PARAM_PCM) => self.word(codec, Some(node), "pcm")?,
                (verb::GET_PARAMETER, verb::PARAM_PIN_CAPS) => {
                    self.word(codec, Some(node), "pin-caps")?
                }
                (verb::GET_PARAMETER, verb::PARAM_CONNECTION_LENGTH) => {
                    self.word(codec, Some(node), "conn-len")?
                }
                (verb::GET_CONFIG_DEFAULT, _) => self.word(codec, Some(node), "cfgdef")?,
                (verb::GET_CONNECTION_LIST, index) => self.connection(codec, node, index)?,
                _ => return None,
            };
            Response::new(raw)
        }
    }

    fn walk(text: &'static str, statests: u16) -> Vec<Codec> {
        enumerate(&mut Fake { text }, statests).into_iter().map(Result::unwrap).collect()
    }

    #[test]
    fn walking_the_t14_reaches_the_graph_its_own_log_describes() {
        // `STATESTS=0x0005`: the ALC257 at 0 and Intel display audio at 2.
        let walked = walk(include_str!("../fixtures/alc257-t14.txt"), 0x0005);
        let parsed = fixture::t14();
        assert_eq!(walked.len(), parsed.len());
        for (walked, parsed) in walked.iter().zip(&parsed) {
            assert_eq!(walked.address, parsed.address);
            assert_eq!(walked.vendor, parsed.vendor);
            assert_eq!(walked.device, parsed.device);
            assert_eq!(walked.groups.len(), parsed.groups.len());
            assert_eq!(walked.groups[0].widgets.len(), parsed.groups[0].widgets.len());
        }
    }

    #[test]
    fn the_walked_t14_binds_the_same_speaker_the_parsed_one_does() {
        // The whole point of the walk: what it produces has to be the input
        // `find_output_path` was verified against, or the crate's coverage
        // describes a graph no driver ever builds.
        let walked = walk(include_str!("../fixtures/alc257-t14.txt"), 0x0005);
        let path = find_output_path(&walked).expect("the T14 has a speaker");
        assert_eq!(path.output.node, Node(0x14));
        assert_eq!(path.converter, Node(0x02));
        assert_eq!(path.headphone.expect("the jack").node, Node(0x21));
        // The pin amplifier the driver mutes through, read off the walk rather
        // than off the fixture parser.
        assert!(path.output.amp.expect("the speaker pin has an output amp").mute);
        assert!(path.output.eapd);
    }

    #[test]
    fn walking_qemus_two_codecs_binds_the_line_out() {
        let walked = walk(include_str!("../fixtures/qemu-intel-hda.txt"), 0x0003);
        assert_eq!(walked.len(), 2);
        let path = find_output_path(&walked).expect("line-out is an output");
        assert_eq!(path.converter, Node(0x02));
        // The other arrangement of §6.4 item 2: this pin has no amplifier at
        // all and the converter carries both halves, where the T14 splits them.
        assert_eq!(path.output.amp, None);
        let amp = path.converter_amp.expect("QEMU's converter has an output amp");
        assert!(amp.mute);
        assert!(amp.gain.is_some_and(|gain| gain.steps > 1));
    }

    #[test]
    fn a_codec_statests_named_and_the_link_does_not_carry_is_named_not_skipped() {
        // Bit 3 of `STATESTS` with nothing behind it. A walk that dropped it
        // would report a machine with fewer codecs than the register named,
        // which is the report §2.3 forbids.
        let found = enumerate(&mut Fake { text: include_str!("../fixtures/alc257-t14.txt") }, 0x000d);
        assert_eq!(found.len(), 3);
        assert_eq!(found[2].as_ref().err(), Some(&(Address::new(3).unwrap(), CodecFault::Silent)));
    }

    #[test]
    fn a_group_that_is_not_audio_is_kept_and_not_walked() {
        // §2.3 step 1: modem groups are named, never silently dropped — a
        // driver has to be able to say what it found and did not use.
        struct Modem;
        impl Verbs for Modem {
            fn get(
                &mut self,
                _: Address,
                node: Node,
                command: u16,
                payload: u8,
            ) -> Option<Response> {
                Response::new(match (node, command, payload) {
                    (Node::ROOT, verb::GET_PARAMETER, verb::PARAM_VENDOR_ID) => 0x1234_5678,
                    (Node::ROOT, verb::GET_PARAMETER, verb::PARAM_SUB_NODE_COUNT) => 0x0001_0001,
                    (Node(1), verb::GET_PARAMETER, verb::PARAM_FUNCTION_TYPE) => 0x0000_0002,
                    _ => return None,
                })
            }
        }
        let found = enumerate(&mut Modem, 0x0001);
        let codec = found[0].as_ref().unwrap();
        assert_eq!(codec.groups[0].kind, FunctionKind::Modem);
        assert!(codec.groups[0].widgets.is_empty());
        assert!(find_output_path(core::slice::from_ref(codec)).is_err());
    }

    #[test]
    fn a_subordinate_range_running_off_the_node_space_is_refused_by_name() {
        struct Wrapped;
        impl Verbs for Wrapped {
            fn get(&mut self, _: Address, _: Node, command: u16, payload: u8) -> Option<Response> {
                Response::new(match (command, payload) {
                    (verb::GET_PARAMETER, verb::PARAM_VENDOR_ID) => 0x1234_5678,
                    // 200 nodes from 200: a walk of it wraps and reads the root
                    // as a widget.
                    (verb::GET_PARAMETER, verb::PARAM_SUB_NODE_COUNT) => 0x00c8_00c8,
                    _ => return None,
                })
            }
        }
        assert_eq!(
            enumerate(&mut Wrapped, 0x0001)[0].as_ref().err(),
            Some(&(Address::new(0).unwrap(), CodecFault::RangePastNodeSpace))
        );
    }

    #[test]
    fn a_widget_count_past_the_bound_costs_the_widgets_past_it_and_nothing_else() {
        struct Crowded;
        impl Verbs for Crowded {
            fn get(&mut self, _: Address, node: Node, command: u16, payload: u8) -> Option<Response> {
                Response::new(match (node, command, payload) {
                    (Node::ROOT, verb::GET_PARAMETER, verb::PARAM_VENDOR_ID) => 0x1234_5678,
                    (Node::ROOT, verb::GET_PARAMETER, verb::PARAM_SUB_NODE_COUNT) => 0x0001_0001,
                    (Node(1), verb::GET_PARAMETER, verb::PARAM_FUNCTION_TYPE) => 0x0000_0001,
                    // 250 widgets from node 2, well past MAX_WIDGETS.
                    (Node(1), verb::GET_PARAMETER, verb::PARAM_SUB_NODE_COUNT) => 0x0002_00fa,
                    (_, verb::GET_PARAMETER, verb::PARAM_WIDGET_CAPS) => 0x0000_041d,
                    (_, verb::GET_PARAMETER, verb::PARAM_PCM) => 0x000e_0060,
                    _ => return None,
                })
            }
        }
        let found = enumerate(&mut Crowded, 0x0001);
        assert_eq!(found[0].as_ref().unwrap().groups[0].widgets.len(), MAX_WIDGETS);
    }
}

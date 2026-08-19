//! Graphs to test against: one real machine's, and the states no real machine
//! in reach constructs.
//!
//! The real ones are the boot-time codec probe's own `hda:` lines, committed
//! verbatim — the second life that log format was designed for, and it outlives
//! the probe: the diagnostic has since been deleted and the fixtures stay as
//! artifacts.

use alloc::vec;
use alloc::vec::Vec;

use crate::caps::{AmpCaps, ConfigDefault, PcmCaps, PinCaps, WidgetCaps};
use crate::graph::{Codec, FunctionGroup, FunctionKind, Pin, Widget};
use crate::verb::{Address, Node, Response, Subordinates};

/// The ThinkPad T14 Gen 2's ALC257 and its display audio, as the probe read
/// them on 2026-08-05.
pub fn t14() -> Vec<Codec> {
    parse(include_str!("../fixtures/alc257-t14.txt"))
}

/// QEMU's `intel-hda` with an `hda-output` and an `hda-duplex` behind it, as
/// the same probe read them. The harness's own machine, so what the suite
/// exercises is a graph and not a hand-written expectation of one.
pub fn qemu() -> Vec<Codec> {
    parse(include_str!("../fixtures/qemu-intel-hda.txt"))
}

fn hex(text: &str) -> Option<u32> {
    u32::from_str_radix(text.strip_prefix("0x").unwrap_or(text), 16).ok()
}

/// The value of `key=` on this line, up to the next space.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let at = line.find(&alloc::format!(" {key}="))? + key.len() + 2;
    let rest = &line[at..];
    Some(rest.split(' ').next().unwrap_or(rest))
}

fn hex_field(line: &str, key: &str) -> Option<u32> {
    hex(field(line, key)?)
}

fn response(line: &str, key: &str) -> Option<Response> {
    Response::new(hex_field(line, key)?)
}

/// Every `hda:` line the probe prints for a codec, back into a graph.
///
/// Only the lines that carry a codec's own words are read; the verdict lines
/// are the probe's opinion and this crate forms its own.
pub fn parse(text: &str) -> Vec<Codec> {
    let mut codecs: Vec<Codec> = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("hda: codec") else { continue };
        let (address, rest) = rest.split_at(rest.find(' ').unwrap_or(rest.len()));
        let Some(address) = address.parse::<u8>().ok().and_then(Address::new) else { continue };

        if let (Some(vendor), Some(device)) = (hex_field(rest, "vendor"), hex_field(rest, "device"))
        {
            codecs.push(Codec {
                address,
                vendor: vendor as u16,
                device: device as u16,
                groups: Vec::new(),
            });
            continue;
        }
        let Some(codec) = codecs.iter_mut().find(|c| c.address == address) else { continue };

        if let Some(group) = hex_field(rest, "fg") {
            let node = Node(group as u8);
            if let Some(kind) = hex_field(rest, "type") {
                codec.groups.push(FunctionGroup {
                    node,
                    kind: FunctionKind::decode(Response::new(kind).unwrap()),
                    range: Subordinates { first: Node(0), count: 1 },
                    widgets: Vec::new(),
                });
            } else if let Some(range) = field(rest, "widgets") {
                let (first, last) = range.split_once("..").unwrap_or((range, range));
                if let (Some(first), Some(last), Some(group)) =
                    (hex(first), hex(last), codec.groups.last_mut())
                {
                    group.range =
                        Subordinates { first: Node(first as u8), count: (last - first + 1) as u8 };
                }
            }
            continue;
        }

        let Some(node) = hex_field(rest, "node").map(|n| Node(n as u8)) else { continue };
        let Some(group) = codec.groups.last_mut() else { continue };

        if let Some(caps) = response(rest, "caps") {
            group.widgets.push(Widget {
                node,
                caps: WidgetCaps::decode(caps),
                connections: Vec::new(),
                amp_out: None,
                amp_in: None,
                pcm: None,
                pin: None,
            });
            continue;
        }
        let Some(widget) = group.widgets.iter_mut().find(|w| w.node == node) else { continue };

        if let Some(amp) = response(rest, "amp-out-caps") {
            widget.amp_out = Some(AmpCaps::decode(amp));
        }
        if let Some(amp) = response(rest, "amp-in-caps") {
            widget.amp_in = Some(AmpCaps::decode(amp));
        }
        if let Some(pcm) = response(rest, "pcm") {
            widget.pcm = Some(PcmCaps::decode(pcm));
        }
        // Not `field`: the probe prints the list with `{:?}`, so it carries
        // spaces and stops at `]` rather than at the next space. Taking the
        // first word truncated every multi-entry list to one node — a graph
        // that still traverses, still finds a converter, and has lost every
        // route but the first.
        if let Some(list) = rest.split_once(" list=[").map(|(_, l)| l.split(']').next().unwrap_or(l))
        {
            // Already expanded by the probe; `graph::decode_connections` is
            // what turns wire form into this, and it is tested on wire words.
            widget.connections = list
                .split(',')
                .filter_map(|n| n.trim().parse::<u8>().ok())
                .map(Node)
                .collect();
        }
        if let Some(caps) = response(rest, "pin-caps") {
            widget.pin = Some(Pin {
                caps: PinCaps::decode(caps),
                config: ConfigDefault::decode(Response::new(0).unwrap()),
            });
        }
        if let (Some(config), Some(pin)) = (response(rest, "cfgdef"), widget.pin.as_mut()) {
            pin.config = ConfigDefault::decode(config);
        }
    }
    codecs
}

// --- graphs no machine in reach produces ---

fn widget(node: u8, caps: u32, connections: &[u8]) -> Widget {
    Widget {
        node: Node(node),
        caps: WidgetCaps::decode(Response::new(caps).unwrap()),
        connections: connections.iter().copied().map(Node).collect(),
        amp_out: None,
        amp_in: None,
        pcm: None,
        pin: None,
    }
}

/// A pin complex that says "internal speaker, wired".
fn speaker_pin(node: u8, connections: &[u8]) -> Widget {
    let mut w = widget(node, 0x0040_058d, connections);
    w.pin = Some(Pin {
        caps: PinCaps::decode(Response::new(0x0001_0014).unwrap()),
        config: ConfigDefault::decode(Response::new(0x9017_0110).unwrap()),
    });
    w
}

fn codec(widgets: Vec<Widget>) -> Vec<Codec> {
    let first = widgets.iter().map(|w| w.node.0).min().unwrap_or(0);
    let last = widgets.iter().map(|w| w.node.0).max().unwrap_or(0);
    vec![Codec {
        address: Address::new(0).unwrap(),
        vendor: 0x0000,
        device: 0x0000,
        groups: vec![FunctionGroup {
            node: Node(1),
            kind: FunctionKind::Audio,
            range: Subordinates { first: Node(first), count: last - first + 1 },
            widgets,
        }],
    }]
}

/// Two mixers that name each other, with a converter nowhere behind them.
pub fn synthetic_cycle() -> Vec<Codec> {
    codec(vec![
        speaker_pin(0x20, &[0x30]),
        widget(0x30, 0x0020_010b, &[0x40]),
        widget(0x40, 0x0020_010b, &[0x30]),
    ])
}

/// A connection naming a node the function group never declared.
pub fn synthetic_outside_group() -> Vec<Codec> {
    codec(vec![speaker_pin(0x20, &[0x7e]), widget(0x30, 0x0000_041d, &[])])
}

/// Pin → selector → converter, with the converter the *second* input, so an
/// implementation that always selected input 0 would route silence.
pub fn synthetic_selector() -> Vec<Codec> {
    codec(vec![
        widget(0x10, 0x0000_041d, &[]),
        speaker_pin(0x20, &[0x30, 0x31]),
        widget(0x30, 0x0030_0101, &[0x40, 0x10]),
        widget(0x31, 0x0030_0101, &[]),
        widget(0x40, 0x0020_010b, &[]),
    ])
}

/// Two speaker-labelled pins with a converter behind each, the *unwired* one
/// first — the ordering the T14 happens not to have, and the only arrangement
/// in which the port-connectivity check changes which pin is chosen.
pub fn synthetic_unwired_first() -> Vec<Codec> {
    let mut unwired = speaker_pin(0x20, &[0x10]);
    // `0x411111f0`: the value a codec writes for a pin nobody soldered.
    unwired.pin.as_mut().unwrap().config =
        ConfigDefault::decode(Response::new(0x4111_11f0).unwrap());
    codec(vec![widget(0x10, 0x0000_041d, &[]), unwired, speaker_pin(0x21, &[0x10])])
}

/// A wired line-out at a *lower* node than a wired speaker, both traceable —
/// the only arrangement in which walking `OUTPUT_PREFERENCE` differs from
/// taking the first wired output.
pub fn synthetic_line_out_first() -> Vec<Codec> {
    let mut line_out = speaker_pin(0x20, &[0x10]);
    // Jack, line-out, wired.
    line_out.pin.as_mut().unwrap().config =
        ConfigDefault::decode(Response::new(0x0101_1010).unwrap());
    codec(vec![widget(0x10, 0x0000_041d, &[]), line_out, speaker_pin(0x21, &[0x10])])
}

/// A wired speaker pin whose connection list leads only to a mixer with
/// nothing behind it.
pub fn synthetic_dead_end() -> Vec<Codec> {
    codec(vec![speaker_pin(0x20, &[0x30]), widget(0x30, 0x0020_010b, &[])])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{Connectivity, DefaultDevice, WidgetKind};

    #[test]
    fn the_t14_fixture_carries_both_codecs_and_every_widget() {
        let codecs = t14();
        assert_eq!(codecs.len(), 2);

        let alc = &codecs[0];
        assert_eq!(alc.vendor, 0x10ec);
        assert_eq!(alc.device, 0x0257);
        assert_eq!(alc.groups.len(), 1);
        let group = &alc.groups[0];
        assert_eq!(group.kind, FunctionKind::Audio);
        // `widgets=0x02..0x24` is 35 nodes, and every one is on a line.
        assert_eq!(group.range.count, 35);
        assert_eq!(group.widgets.len(), 35);

        let display = &codecs[1];
        assert_eq!(display.address, Address::new(2).unwrap());
        assert_eq!(display.vendor, 0x8086);
        assert_eq!(display.device, 0x2812);
    }

    #[test]
    fn the_speaker_pin_round_trips_through_the_log_format() {
        let codecs = t14();
        let group = &codecs[0].groups[0];
        let speaker = group.widget(Node(0x14)).unwrap();
        assert_eq!(speaker.caps.kind, WidgetKind::PinComplex);
        assert_eq!(speaker.connections, [Node(0x02)]);
        let pin = speaker.pin.as_ref().unwrap();
        assert_eq!(pin.config.device, DefaultDevice::Speaker);
        assert_eq!(pin.config.connectivity, Connectivity::FixedFunction);
        assert!(pin.caps.eapd);
        assert!(speaker.amp_out.unwrap().mute);
    }

    #[test]
    fn the_converters_keep_their_rates() {
        let codecs = t14();
        let dac = codecs[0].groups[0].widget(Node(0x02)).unwrap();
        assert!(dac.pcm.unwrap().supports(44100, 16));
    }

    #[test]
    fn every_pin_in_the_fixture_has_a_configuration_default() {
        // A pin whose `cfgdef` line failed to parse would decode as
        // connectivity `Jack` and device `LineOut` — a plausible pin nobody
        // wired, which is exactly the value this crate must never invent.
        for codec in t14() {
            for group in &codec.groups {
                for widget in &group.widgets {
                    if widget.caps.kind == WidgetKind::PinComplex {
                        assert!(widget.pin.is_some(), "node {:#04x}", widget.node.0);
                    }
                }
            }
        }
    }
}

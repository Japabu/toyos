//! The verbs that turn a chosen path into a path that makes sound.
//!
//! `specs/plans/hda-driver-plan.md` §2.3 step 6, as one pure function: power, the
//! connection selects, pin control, EAPD, the amplifiers, the format and the
//! stream tag. The driver sends what comes back and decides nothing on the way.
//!
//! Boundary contract: every payload layout below is the Intel High Definition
//! Audio specification's. They are here rather than in the driver because a
//! wrong bit is a path that configures perfectly and plays silence, and here it
//! is a host test against the graph of the machine that has to work.

use alloc::vec::Vec;

use crate::caps::AmpCaps;
use crate::graph::{Codec, FunctionGroup};
use crate::path::{OutputPath, PinSetup};
use crate::verb::{self, Address, Node, Verb};

/// `Set Power State`'s fully-on value.
const POWER_D0: u8 = 0x00;

/// `Set Pin Widget Control`: drive the pin as an output, and drive it hard
/// enough for headphones.
const PIN_OUT_ENABLE: u8 = 1 << 6;
const PIN_HEADPHONE_ENABLE: u8 = 1 << 7;

/// `Set EAPD/BTL Enable`: the external amplifier bit, and nothing else. A
/// speaker pin that has this and is not told to use it is a correctly
/// configured path that makes no sound.
const EAPD_ENABLE: u8 = 1 << 1;

/// `Set Amplifier Gain/Mute`: which amplifier and which channels the payload
/// applies to. Output, left and right — this driver sets both channels to one
/// value, so there is no per-channel index to carry.
const AMP_SET_OUTPUT: u16 = 1 << 15;
const AMP_SET_LEFT: u16 = 1 << 13;
const AMP_SET_RIGHT: u16 = 1 << 12;

/// The whole configuration sequence for one output path, in the order it has to
/// be sent.
///
/// `None` when the codec or the function group the path names is not in
/// `codecs` — which cannot happen for a path this crate produced, and is a
/// refusal rather than a panic because the caller is holding untrusted device
/// answers either way.
pub fn verbs(
    codecs: &[Codec],
    path: &OutputPath,
    format: u16,
    stream_tag: u8,
) -> Option<Vec<Verb>> {
    let group = codecs
        .iter()
        .find(|c| c.address == path.codec)?
        .groups
        .iter()
        .find(|g| g.node == path.group)?;
    let codec = path.codec;
    let mut out = Vec::new();

    // Power first: a widget still in D3 answers every later verb and acts on
    // none of them.
    out.push(power(codec, group.node));
    for node in nodes(path) {
        if group.widget(node).is_some_and(|w| w.caps.power_control) {
            out.push(power(codec, node));
        }
    }

    // Then the route, so the converter is reaching the pin before the pin is
    // told to drive.
    for pin in pins(path) {
        for hop in &pin.route {
            out.push(Verb::short(codec, hop.node, verb::SET_CONNECTION_SELECT, hop.select));
        }
    }

    for pin in pins(path) {
        let mut control = PIN_OUT_ENABLE;
        if pin.headphone_drive {
            control |= PIN_HEADPHONE_ENABLE;
        }
        out.push(Verb::short(codec, pin.node, verb::SET_PIN_CONTROL, control));
        if pin.eapd {
            out.push(Verb::short(codec, pin.node, verb::SET_EAPD, EAPD_ENABLE));
        }
        if let Some(amp) = pin.amp {
            out.push(amp_verb(codec, pin.node, amp));
        }
    }

    if let Some(amp) = path.converter_amp {
        out.push(amp_verb(codec, path.converter, amp));
    }
    out.push(Verb::long(codec, path.converter, verb::SET_CONVERTER_FORMAT as u8, format));
    // Channel 0 of the stream the controller was given. The tag is the
    // controller's and this verb is the only place the codec learns it.
    out.push(Verb::short(codec, path.converter, verb::SET_CONVERTER_STREAM, stream_tag << 4));
    Some(out)
}

/// Unmuted, and at the amplifier's own 0 dB index where it has one.
///
/// The two halves are independent because the codec makes them independent:
/// the T14's pin amplifier is mute-only with no gain field to write, and its
/// converter has 88 steps and no mute bit. Writing the absent half is a store
/// that succeeds and does nothing, which is worse than not writing it.
fn amp_verb(codec: Address, node: Node, amp: AmpCaps) -> Verb {
    let gain = amp.gain.map_or(0, |range| range.zero_db as u16);
    Verb::long(
        codec,
        node,
        verb::SET_AMP_GAIN_MUTE as u8,
        AMP_SET_OUTPUT | AMP_SET_LEFT | AMP_SET_RIGHT | gain,
    )
}

fn power(codec: Address, node: Node) -> Verb {
    Verb::short(codec, node, verb::SET_POWER_STATE, POWER_D0)
}

fn pins(path: &OutputPath) -> impl Iterator<Item = &PinSetup> {
    core::iter::once(&path.output).chain(path.headphone.iter())
}

/// Every node on the path, converter included, without repeats: a hop shared by
/// the speaker and the jack would otherwise be powered twice.
fn nodes(path: &OutputPath) -> Vec<Node> {
    let mut out = alloc::vec![path.converter];
    for pin in pins(path) {
        for hop in &pin.route {
            if !out.contains(&hop.node) {
                out.push(hop.node);
            }
        }
        if !out.contains(&pin.node) {
            out.push(pin.node);
        }
    }
    out
}

/// The rate and width this driver asks a converter for, and whether the
/// converter offers them.
///
/// One rate, because soundd's mixer, its resampler and gate A's recorded
/// counters are all sized against it, and a converter that cannot play it is a
/// refusal the driver reports rather than a rate it substitutes. Both machines
/// in reach offer it: the T14's converter does 44.1 and 48 kHz at 16/20/24, and
/// QEMU's does 16 k–96 k at 16.
pub const RATE: u32 = 44_100;
pub const WIDTH: u8 = 16;

/// `SDnFMT` and the codec's `Set Converter Format` payload for this path, or
/// `None` where the converter does not offer [`RATE`] at [`WIDTH`].
pub fn format(codecs: &[Codec], path: &OutputPath) -> Option<(u16, u8)> {
    let group = group_of(codecs, path)?;
    let converter = group.widget(path.converter)?;
    let pcm = converter.pcm?;
    if !pcm.supports(RATE, WIDTH) {
        return None;
    }
    let channels = converter.caps.channels.min(2);
    Some((crate::stream::stream_format(RATE, WIDTH, channels)?, channels))
}

fn group_of<'a>(codecs: &'a [Codec], path: &OutputPath) -> Option<&'a FunctionGroup> {
    codecs
        .iter()
        .find(|c| c.address == path.codec)?
        .groups
        .iter()
        .find(|g| g.node == path.group)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::path::find_output_path;

    /// The bit this sequence never sets, which is why it lives only here.
    const AMP_MUTE: u16 = 1 << 7;

    fn t14() -> (Vec<Codec>, OutputPath) {
        let codecs = fixture::t14();
        let path = find_output_path(&codecs).unwrap();
        (codecs, path)
    }

    #[test]
    fn the_t14_s_speaker_is_told_to_power_its_external_amplifier() {
        // §6.4 item 1: both output pins report EAPD capable and read the bit
        // back clear at boot, so a path configured without this is correct and
        // silent. Removing the `pin.eapd` arm reds here and nowhere else.
        let (codecs, path) = t14();
        let sent = verbs(&codecs, &path, 0x4011, 1).unwrap();
        let eapd: Vec<u32> = sent
            .iter()
            .map(|v| v.raw())
            .filter(|v| (v >> 8) & 0xFFF == verb::SET_EAPD as u32)
            .collect();
        // The speaker at 0x14 and the jack at 0x21, both of them.
        assert_eq!(eapd, [0x0147_0C02, 0x0217_0C02]);
    }

    #[test]
    fn the_two_halves_of_the_t14_s_volume_control_are_written_where_they_exist() {
        // §6.4 item 2: mute on the pin, gain on the converter, and neither
        // widget implements the other's field.
        let (codecs, path) = t14();
        let sent = verbs(&codecs, &path, 0x4011, 1).unwrap();
        let amps: Vec<u32> = sent
            .iter()
            .map(|v| v.raw())
            .filter(|v| (v >> 16) & 0xF == verb::SET_AMP_GAIN_MUTE as u32 && v >> 28 == 0)
            .collect();
        // Pin 0x14 and pin 0x21: unmuted, gain field zero because there is no
        // gain range to index. Converter 0x02: the codec's own 0 dB index, 87.
        assert_eq!(amps, [0x0143_B000, 0x0213_B000, 0x0023_B057]);
        assert_eq!(path.converter_amp.unwrap().gain.unwrap().zero_db, 0x57);
    }

    #[test]
    fn nothing_in_the_sequence_ever_sets_the_mute_bit() {
        // The whole sequence exists to make a path audible. A mute here is the
        // defect that looks exactly like a driver that never ran.
        let (codecs, path) = t14();
        for verb in verbs(&codecs, &path, 0x4011, 1).unwrap() {
            let raw = verb.raw();
            if (raw >> 16) & 0xF == verb::SET_AMP_GAIN_MUTE as u32 {
                assert_eq!(raw as u16 & AMP_MUTE, 0, "{raw:#010x} mutes an amplifier");
            }
        }
    }

    #[test]
    fn the_jack_is_told_to_drive_headphones_and_the_speaker_is_not() {
        let (codecs, path) = t14();
        let sent = verbs(&codecs, &path, 0x4011, 1).unwrap();
        let control: Vec<u32> = sent
            .iter()
            .map(|v| v.raw())
            .filter(|v| (v >> 8) & 0xFFF == verb::SET_PIN_CONTROL as u32)
            .collect();
        assert_eq!(control, [0x0147_0740, 0x0217_07C0]);
    }

    #[test]
    fn the_converter_learns_the_format_and_the_tag_last() {
        let (codecs, path) = t14();
        let sent = verbs(&codecs, &path, 0x4011, 3).unwrap();
        let tail: Vec<u32> = sent[sent.len() - 2..].iter().map(|v| v.raw()).collect();
        // Set Converter Format on node 0x02 with 44.1 kHz S16 stereo, then
        // stream tag 3 channel 0.
        assert_eq!(tail, [0x0022_4011, 0x0027_0630]);
    }

    #[test]
    fn every_widget_on_the_path_that_has_a_power_state_is_told_d0() {
        let (codecs, path) = t14();
        let sent = verbs(&codecs, &path, 0x4011, 1).unwrap();
        let powered: Vec<u8> = sent
            .iter()
            .map(|v| v.raw())
            .filter(|v| (v >> 8) & 0xFFF == verb::SET_POWER_STATE as u32)
            .map(|v| ((v >> 20) & 0xFF) as u8)
            .collect();
        // The function group, the converter, the speaker and the jack. Both
        // pins carry a power state on this codec and both routes are depth 1,
        // so there is no hop between them.
        assert_eq!(powered, [0x01, 0x02, 0x14, 0x21]);
    }

    #[test]
    fn a_hop_shared_by_two_pins_is_powered_once() {
        // The T14 cannot produce this: both its routes are empty. The synthetic
        // selector graph is the only place a hop exists at all.
        let codecs = fixture::synthetic_selector();
        let path = find_output_path(&codecs).unwrap();
        let sent = verbs(&codecs, &path, 0x4011, 1).unwrap();
        let selects: Vec<u32> = sent
            .iter()
            .map(|v| v.raw())
            .filter(|v| (v >> 8) & 0xFFF == verb::SET_CONNECTION_SELECT as u32)
            .collect();
        assert_eq!(selects, [0x0207_0100, 0x0307_0101]);
    }

    #[test]
    fn both_machines_offer_the_one_rate_this_driver_asks_for() {
        let (codecs, path) = t14();
        assert_eq!(format(&codecs, &path), Some((0x4011, 2)));

        let codecs = fixture::qemu();
        let path = find_output_path(&codecs).unwrap();
        assert_eq!(format(&codecs, &path), Some((0x4011, 2)));
    }

    #[test]
    fn a_converter_that_does_not_offer_the_rate_is_a_refusal_and_not_a_substitution() {
        let mut codecs = fixture::t14();
        // 48 kHz only, at 16 bits: a converter that exists and cannot play the
        // one rate this pipeline runs at.
        let group = &mut codecs[0].groups[0];
        let dac = group.widgets.iter_mut().find(|w| w.node == Node(0x02)).unwrap();
        dac.pcm = Some(crate::caps::PcmCaps::decode(
            crate::verb::Response::new(0x0002_0040).unwrap(),
        ));
        let path = find_output_path(&codecs).unwrap();
        assert_eq!(format(&codecs, &path), None);
    }

    #[test]
    fn qemus_path_writes_its_amplifier_on_the_converter_and_none_on_the_pin() {
        let codecs = fixture::qemu();
        let path = find_output_path(&codecs).unwrap();
        let sent = verbs(&codecs, &path, 0x4011, 1).unwrap();
        let amps: Vec<u32> = sent
            .iter()
            .map(|v| v.raw())
            .filter(|v| (v >> 16) & 0xF == verb::SET_AMP_GAIN_MUTE as u32)
            .collect();
        assert_eq!(amps, [0x0023_B04A]);
    }
}

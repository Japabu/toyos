//! What a codec says about one widget, decoded.
//!
//! Boundary contract: every function here takes a raw parameter response and
//! returns a value whose every field the specification defines. Bit positions
//! come from the Intel High Definition Audio specification's parameter tables;
//! the test vectors are the words a real codec answered, so a wrong shift
//! fails against a machine rather than against prose.

use crate::verb::Response;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WidgetKind {
    AudioOutput,
    AudioInput,
    Mixer,
    Selector,
    PinComplex,
    Power,
    VolumeKnob,
    BeepGenerator,
    VendorDefined,
    /// A type this specification revision does not define. Carried rather than
    /// rejected: a widget nothing walks is not a reason to refuse a codec.
    Reserved(u8),
}

impl WidgetKind {
    fn decode(kind: u8) -> Self {
        match kind {
            0x0 => Self::AudioOutput,
            0x1 => Self::AudioInput,
            0x2 => Self::Mixer,
            0x3 => Self::Selector,
            0x4 => Self::PinComplex,
            0x5 => Self::Power,
            0x6 => Self::VolumeKnob,
            0x7 => Self::BeepGenerator,
            0xF => Self::VendorDefined,
            other => Self::Reserved(other),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WidgetCaps {
    pub kind: WidgetKind,
    /// Already one-based: the wire carries `Chan Count Ext` beside a `Stereo`
    /// bit, both one pair short of the count.
    pub channels: u8,
    pub input_amp: bool,
    pub output_amp: bool,
    /// This widget's amplifier capabilities are its own; without it they are
    /// the function group's and asking the widget answers the group's word.
    pub amp_override: bool,
    pub format_override: bool,
    pub connection_list: bool,
    pub digital: bool,
    pub power_control: bool,
    pub unsolicited: bool,
}

impl WidgetCaps {
    pub fn decode(response: Response) -> Self {
        let raw = response.raw();
        Self {
            kind: WidgetKind::decode(((raw >> 20) & 0xF) as u8),
            channels: ((((raw >> 13) & 0x7) << 1 | (raw & 1)) + 1) as u8,
            input_amp: raw & (1 << 1) != 0,
            output_amp: raw & (1 << 2) != 0,
            amp_override: raw & (1 << 3) != 0,
            format_override: raw & (1 << 4) != 0,
            connection_list: raw & (1 << 8) != 0,
            digital: raw & (1 << 9) != 0,
            power_control: raw & (1 << 10) != 0,
            unsolicited: raw & (1 << 7) != 0,
        }
    }
}

/// A gain range an amplifier actually has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GainRange {
    /// One-based, so a range that exists has at least two.
    pub steps: u8,
    /// Quarter-decibels per step, one-based as the wire carries it.
    pub step_size_quarter_db: u8,
    /// The index that is 0 dB, which is where a path is set before anything
    /// applies a volume.
    pub zero_db: u8,
}

/// What an amplifier can be told to do.
///
/// `gain` is an `Option` because the absence is the point: the T14's pin
/// amplifiers report one step, so there is no 0 dB index to write and a driver
/// that computed one would be writing a field the codec does not implement.
/// Its converter amplifier is the other half — 88 steps and **no mute bit** —
/// so a driver that muted there would set a bit that does not exist, and the
/// store would succeed and do nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AmpCaps {
    pub gain: Option<GainRange>,
    pub mute: bool,
}

impl AmpCaps {
    pub fn decode(response: Response) -> Self {
        let raw = response.raw();
        let steps = ((raw >> 8) & 0x7F) as u8;
        Self {
            gain: (steps > 0).then(|| GainRange {
                steps: steps + 1,
                step_size_quarter_db: (((raw >> 16) & 0x7F) + 1) as u8,
                zero_db: (raw & 0x7F) as u8,
            }),
            mute: raw & (1 << 31) != 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PinCaps {
    pub output: bool,
    pub input: bool,
    pub headphone_drive: bool,
    pub presence_detect: bool,
    /// The pin can power an external amplifier. **A speaker pin that has this
    /// and is not told to use it stays silent**: the T14 reports EAPD capable
    /// on both its output pins and reads the bit back clear at boot.
    pub eapd: bool,
    pub high_bit_rate: bool,
}

impl PinCaps {
    pub fn decode(response: Response) -> Self {
        let raw = response.raw();
        Self {
            output: raw & (1 << 4) != 0,
            input: raw & (1 << 5) != 0,
            headphone_drive: raw & (1 << 3) != 0,
            presence_detect: raw & (1 << 2) != 0,
            eapd: raw & (1 << 16) != 0,
            high_bit_rate: raw & (1 << 27) != 0,
        }
    }
}

/// Whether a pin goes anywhere physical.
///
/// The one field that decides whether a pin is a candidate at all, and it is
/// not the default device: four pins on the T14's codec call themselves
/// speakers and are `NoPhysicalConnection`, one of them with a valid
/// connection list and an output amplifier behind it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Connectivity {
    Jack,
    NoPhysicalConnection,
    FixedFunction,
    JackAndFixed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DefaultDevice {
    LineOut,
    Speaker,
    HeadphoneOut,
    Cd,
    SpdifOut,
    DigitalOtherOut,
    ModemLine,
    ModemHandset,
    LineIn,
    Aux,
    MicIn,
    Telephony,
    SpdifIn,
    DigitalOtherIn,
    Other,
    Reserved(u8),
}

impl DefaultDevice {
    fn decode(device: u8) -> Self {
        match device {
            0x0 => Self::LineOut,
            0x1 => Self::Speaker,
            0x2 => Self::HeadphoneOut,
            0x3 => Self::Cd,
            0x4 => Self::SpdifOut,
            0x5 => Self::DigitalOtherOut,
            0x6 => Self::ModemLine,
            0x7 => Self::ModemHandset,
            0x8 => Self::LineIn,
            0x9 => Self::Aux,
            0xA => Self::MicIn,
            0xB => Self::Telephony,
            0xC => Self::SpdifIn,
            0xD => Self::DigitalOtherIn,
            0xF => Self::Other,
            other => Self::Reserved(other),
        }
    }
}

/// What firmware said this pin is wired to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConfigDefault {
    pub connectivity: Connectivity,
    pub device: DefaultDevice,
    /// The codec's own statement that two pins are one output: the T14 puts
    /// its speaker and its headphone jack in association 1, the jack last by
    /// sequence, which is how a graph says "these share a converter and the
    /// jack wins when something is plugged into it".
    pub association: u8,
    pub sequence: u8,
    pub location: u8,
    pub colour: u8,
}

impl ConfigDefault {
    pub fn decode(response: Response) -> Self {
        let raw = response.raw();
        Self {
            connectivity: match (raw >> 30) & 0x3 {
                0 => Connectivity::Jack,
                1 => Connectivity::NoPhysicalConnection,
                2 => Connectivity::FixedFunction,
                _ => Connectivity::JackAndFixed,
            },
            device: DefaultDevice::decode(((raw >> 20) & 0xF) as u8),
            association: ((raw >> 4) & 0xF) as u8,
            sequence: (raw & 0xF) as u8,
            location: ((raw >> 24) & 0x3F) as u8,
            colour: ((raw >> 12) & 0xF) as u8,
        }
    }

    /// Whether sound put on this pin can leave the machine.
    pub fn is_physical(self) -> bool {
        !matches!(self.connectivity, Connectivity::NoPhysicalConnection)
    }
}

/// The rates a converter offers, in the order the parameter's bits are defined.
const RATES: [u32; 12] =
    [8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000, 384000];

/// The sample widths, likewise.
const WIDTHS: [u8; 5] = [8, 16, 20, 24, 32];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PcmCaps(u32);

impl PcmCaps {
    pub fn decode(response: Response) -> Self {
        Self(response.raw())
    }

    pub fn rates(self) -> impl Iterator<Item = u32> {
        let raw = self.0;
        (0..RATES.len()).filter(move |bit| raw & (1 << bit) != 0).map(|bit| RATES[bit])
    }

    pub fn widths(self) -> impl Iterator<Item = u8> {
        let raw = self.0 >> 16;
        (0..WIDTHS.len()).filter(move |bit| raw & (1 << bit) != 0).map(|bit| WIDTHS[bit])
    }

    pub fn supports(self, rate: u32, width: u8) -> bool {
        self.rates().any(|r| r == rate) && self.widths().any(|w| w == width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn response(raw: u32) -> Response {
        Response::new(raw).unwrap()
    }

    #[test]
    fn the_t14_s_speaker_dac_decodes_as_a_stereo_converter() {
        let caps = WidgetCaps::decode(response(0x0000_041d));
        assert_eq!(caps.kind, WidgetKind::AudioOutput);
        assert_eq!(caps.channels, 2);
        assert!(caps.output_amp && !caps.input_amp);
        assert!(caps.power_control);
        assert!(!caps.connection_list);
    }

    #[test]
    fn the_t14_s_speaker_pin_has_a_connection_list_and_an_output_amp() {
        let caps = WidgetCaps::decode(response(0x0040_058d));
        assert_eq!(caps.kind, WidgetKind::PinComplex);
        assert!(caps.connection_list);
        assert!(caps.output_amp);
    }

    #[test]
    fn eight_channel_display_audio_decodes_its_channel_count() {
        // The T14's display codec, node 0x03: `channels=8`.
        assert_eq!(WidgetCaps::decode(response(0x0000_6611)).channels, 8);
    }

    #[test]
    fn the_converter_amp_has_gain_and_cannot_mute() {
        // node 0x02 amp-out-caps=0x00025757. 88 steps of 0.75 dB, 0 dB at 87,
        // and bit 31 clear — a mute written here does nothing at all.
        let caps = AmpCaps::decode(response(0x0002_5757));
        assert!(!caps.mute);
        let gain = caps.gain.expect("the converter has a gain range");
        assert_eq!(gain.steps, 88);
        assert_eq!(gain.step_size_quarter_db, 3);
        assert_eq!(gain.zero_db, 87);
    }

    #[test]
    fn the_pin_amp_can_mute_and_has_no_gain() {
        // Both output pins: 0x80000000. One step, so there is no 0 dB index.
        let caps = AmpCaps::decode(response(0x8000_0000));
        assert!(caps.mute);
        assert_eq!(caps.gain, None);
    }

    #[test]
    fn both_t14_output_pins_can_power_an_external_amplifier() {
        let speaker = PinCaps::decode(response(0x0001_0014));
        assert!(speaker.output && speaker.eapd && speaker.presence_detect);
        assert!(!speaker.headphone_drive);

        let headphone = PinCaps::decode(response(0x0001_001c));
        assert!(headphone.output && headphone.eapd);
        assert!(headphone.headphone_drive);
    }

    #[test]
    fn the_internal_speaker_is_fixed_and_the_jack_is_a_jack() {
        let speaker = ConfigDefault::decode(response(0x9017_0110));
        assert_eq!(speaker.connectivity, Connectivity::FixedFunction);
        assert_eq!(speaker.device, DefaultDevice::Speaker);
        assert_eq!(speaker.association, 1);
        assert_eq!(speaker.sequence, 0);
        assert!(speaker.is_physical());

        let headphone = ConfigDefault::decode(response(0x0421_101f));
        assert_eq!(headphone.connectivity, Connectivity::Jack);
        assert_eq!(headphone.device, DefaultDevice::HeadphoneOut);
        assert_eq!(headphone.association, 1);
        assert_eq!(headphone.sequence, 15);
    }

    #[test]
    fn an_unsoldered_pin_still_calls_itself_a_speaker() {
        // node 0x1b: the trap. Says speaker, has a connection list of [2, 3]
        // and an output amp, and goes nowhere. Only connectivity says so.
        let unused = ConfigDefault::decode(response(0x4111_11f0));
        assert_eq!(unused.device, DefaultDevice::Speaker);
        assert_eq!(unused.connectivity, Connectivity::NoPhysicalConnection);
        assert!(!unused.is_physical());
    }

    #[test]
    fn the_t14_s_converter_offers_what_this_pipeline_plays() {
        // node 0x02 pcm=0x000e0060, and soundd's period grid is 44.1 kHz S16.
        let pcm = PcmCaps::decode(response(0x000e_0060));
        let rates: Vec<u32> = pcm.rates().collect();
        assert_eq!(rates, [44100, 48000]);
        let widths: Vec<u8> = pcm.widths().collect();
        assert_eq!(widths, [16, 20, 24]);
        assert!(pcm.supports(44100, 16));
        assert!(!pcm.supports(96000, 16));
    }

    #[test]
    fn qemu_s_converter_offers_it_too() {
        // `hda-output`, so one instrument's physical scale serves both arms.
        let pcm = PcmCaps::decode(response(0x0002_01fc));
        assert!(pcm.supports(44100, 16));
    }
}

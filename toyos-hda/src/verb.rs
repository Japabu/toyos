//! The command a codec answers and the answer it gives.
//!
//! Boundary contract: a verb is 32 bits — codec address, node id, and either a
//! 12-bit verb with an 8-bit payload or a 4-bit verb with a 16-bit payload.
//! Identifiers come from the Intel High Definition Audio specification.

/// A codec's address on the link: which `STATESTS` bit answered.
///
/// Four bits on the wire. Constructed only from a `STATESTS` scan or from a
/// response's own address field, so no value exists that the link did not
/// produce.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Address(u8);

impl Address {
    /// `STATESTS` has fifteen bits and the address field is four, so every bit
    /// index the register can set is an address.
    pub const MAX: u8 = 14;

    pub fn new(address: u8) -> Option<Self> {
        (address <= Self::MAX).then_some(Self(address))
    }

    pub const fn raw(self) -> u8 {
        self.0
    }
}

impl core::fmt::Display for Address {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

/// Every codec address `statests` reports, lowest first.
///
/// The whole of codec presence detection. Returned as an iterator over *all*
/// of them because `specs/hda-driver-plan.md` §2.3 forbids taking the first:
/// display audio answers here beside the analogue codec.
pub fn present(statests: u16) -> impl Iterator<Item = Address> {
    (0..=Address::MAX).filter(move |bit| statests & (1 << bit) != 0).map(Address)
}

/// A node inside one codec. Eight bits on the wire; node 0 is the root.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Node(pub u8);

impl Node {
    pub const ROOT: Self = Self(0);
}

impl core::fmt::LowerHex for Node {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::LowerHex::fmt(&self.0, f)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Verb(u32);

impl Verb {
    /// A 12-bit verb with an 8-bit payload.
    ///
    /// The two forms are separate constructors rather than one function with a
    /// width argument, because the widths are not a caller's choice: each verb
    /// identifier is defined in one form and encoding it in the other names a
    /// different verb.
    pub const fn short(codec: Address, node: Node, verb: u16, payload: u8) -> Self {
        Self(
            ((codec.0 as u32) << 28)
                | ((node.0 as u32) << 20)
                | (((verb as u32) & 0xFFF) << 8)
                | payload as u32,
        )
    }

    /// A 4-bit verb with a 16-bit payload.
    pub const fn long(codec: Address, node: Node, verb: u8, payload: u16) -> Self {
        Self(
            ((codec.0 as u32) << 28)
                | ((node.0 as u32) << 20)
                | (((verb as u32) & 0xF) << 16)
                | payload as u32,
        )
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

pub const GET_PARAMETER: u16 = 0xF00;
pub const GET_CONNECTION_LIST: u16 = 0xF02;
pub const GET_POWER_STATE: u16 = 0xF05;
pub const SET_POWER_STATE: u16 = 0x705;
pub const GET_CONVERTER_FORMAT: u16 = 0xA;
pub const GET_CONVERTER_STREAM: u16 = 0xF06;
pub const SET_CONVERTER_STREAM: u16 = 0x706;
pub const GET_PIN_CONTROL: u16 = 0xF07;
pub const SET_PIN_CONTROL: u16 = 0x707;
pub const GET_EAPD: u16 = 0xF0C;
pub const SET_EAPD: u16 = 0x70C;
pub const GET_CONNECTION_SELECT: u16 = 0xF01;
pub const SET_CONNECTION_SELECT: u16 = 0x701;
pub const GET_CONFIG_DEFAULT: u16 = 0xF1C;
pub const GET_PIN_SENSE: u16 = 0xF09;
pub const SET_AMP_GAIN_MUTE: u16 = 0x3;

pub const PARAM_VENDOR_ID: u8 = 0x00;
pub const PARAM_REVISION_ID: u8 = 0x02;
pub const PARAM_SUB_NODE_COUNT: u8 = 0x04;
pub const PARAM_FUNCTION_TYPE: u8 = 0x05;
pub const PARAM_FUNCTION_CAPS: u8 = 0x08;
pub const PARAM_WIDGET_CAPS: u8 = 0x09;
pub const PARAM_PCM: u8 = 0x0A;
pub const PARAM_STREAM_FORMATS: u8 = 0x0B;
pub const PARAM_PIN_CAPS: u8 = 0x0C;
pub const PARAM_AMP_IN_CAPS: u8 = 0x0D;
pub const PARAM_CONNECTION_LENGTH: u8 = 0x0E;
pub const PARAM_POWER_STATES: u8 = 0x0F;
pub const PARAM_PROCESSING_CAPS: u8 = 0x10;
pub const PARAM_GPIO_COUNT: u8 = 0x11;
pub const PARAM_AMP_OUT_CAPS: u8 = 0x12;
pub const PARAM_VOLUME_KNOB_CAPS: u8 = 0x13;

/// A codec's 32-bit answer.
///
/// A wedged link and an absent codec both read as all ones, which is not a
/// value to decode — so it is refused here, at the boundary, rather than
/// carried inward as a plausible capability word.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Response(u32);

impl Response {
    pub fn new(raw: u32) -> Option<Self> {
        (raw != u32::MAX).then_some(Self(raw))
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// A subordinate node range, as a codec declares it.
///
/// The two fields are checked together against the node id space: a codec
/// claiming a range that runs past 255 is a codec whose walk would wrap, and a
/// wrapped walk re-reads node 0 and calls it a widget.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Subordinates {
    pub first: Node,
    pub count: u8,
}

/// Why a subordinate-node count named no range.
///
/// Two facts and not one: a codec declaring none is a leaf, a codec declaring a
/// range that runs off the node id space is a codec contradicting itself, and
/// nothing that reports the second can afford to have them folded together.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NoSubordinates {
    Leaf,
    PastNodeSpace { first: Node, count: u8 },
}

impl Subordinates {
    pub fn decode(response: Response) -> Result<Self, NoSubordinates> {
        let first = Node(((response.0 >> 16) & 0xFF) as u8);
        let count = (response.0 & 0xFF) as u8;
        if count == 0 {
            return Err(NoSubordinates::Leaf);
        }
        if first.0 as u16 + count as u16 > 256 {
            return Err(NoSubordinates::PastNodeSpace { first, count });
        }
        Ok(Self { first, count })
    }

    pub fn nodes(self) -> impl Iterator<Item = Node> {
        let first = self.first.0 as u16;
        (first..first + self.count as u16).map(|node| Node(node as u8))
    }

    /// The highest node id in the range. There is always one: a range with no
    /// node in it is not a `Subordinates`.
    pub fn last(self) -> Node {
        Node((self.first.0 as u16 + self.count as u16 - 1) as u8)
    }

    pub fn contains(self, node: Node) -> bool {
        let first = self.first.0 as u16;
        (first..first + self.count as u16).contains(&(node.0 as u16))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn short_verb_is_the_probe_s_encoding() {
        // The encoding H0 sent to the T14 and got answers from, so this is
        // checked against a machine rather than against the specification's
        // prose: codec 0, node 0x14, GET_PARAMETER, widget caps.
        let verb = Verb::short(Address::new(0).unwrap(), Node(0x14), GET_PARAMETER, PARAM_WIDGET_CAPS);
        assert_eq!(verb.raw(), 0x014F_0009);
    }

    #[test]
    fn long_verb_puts_its_payload_under_a_four_bit_verb() {
        let verb = Verb::long(Address::new(2).unwrap(), Node(0x03), SET_AMP_GAIN_MUTE as u8, 0xB05F);
        assert_eq!(verb.raw(), 0x2033_B05F);
    }

    #[test]
    fn all_ones_is_not_a_response() {
        assert_eq!(Response::new(u32::MAX), None);
        assert!(Response::new(0xFFFF_FFFE).is_some());
    }

    #[test]
    fn present_reports_every_codec_and_not_the_first() {
        // The T14's own STATESTS: the analogue codec at 0 and display audio at
        // 2. A `first()` here is the defect §2.3 exists to prevent.
        let found: Vec<u8> = present(0x0005).map(Address::raw).collect();
        assert_eq!(found, [0, 2]);
    }

    #[test]
    fn an_address_and_a_node_print_the_way_a_dump_names_them() {
        assert_eq!(alloc::format!("codec{}", Address::new(2).unwrap()), "codec2");
        assert_eq!(alloc::format!("node={:#04x}", Node(0x02)), "node=0x02");
        assert_eq!(alloc::format!("{:#04x}", Node(0xFF)), "0xff");
    }

    #[test]
    fn statests_bit_15_is_not_an_address() {
        assert_eq!(present(0x8000).count(), 0);
        assert_eq!(Address::new(15), None);
    }

    #[test]
    fn a_range_running_past_the_node_space_is_refused() {
        // 200 nodes from 200 is 400, and a u8 walk would wrap to node 0 and
        // report the root as a widget.
        assert_eq!(
            Subordinates::decode(Response::new(0x00C8_00C8).unwrap()),
            Err(NoSubordinates::PastNodeSpace { first: Node(200), count: 200 })
        );
        // Ending exactly at 256 is the last legal range.
        let last = Subordinates::decode(Response::new(0x0080_0080).unwrap()).unwrap();
        assert_eq!(last.nodes().last(), Some(Node(255)));
        assert_eq!(last.last(), Node(255));
        assert_eq!(last.nodes().count(), 128);
    }

    #[test]
    fn zero_subordinates_is_no_range_at_all() {
        assert_eq!(
            Subordinates::decode(Response::new(0x0002_0000).unwrap()),
            Err(NoSubordinates::Leaf)
        );
    }

    #[test]
    fn the_t14_s_widget_range_decodes_to_its_own_nodes() {
        // `hda: codec0 fg=0x01 widgets=0x02..0x24`, from the boot.
        let range = Subordinates::decode(Response::new(0x0002_0023).unwrap()).unwrap();
        assert_eq!(range.first, Node(0x02));
        assert_eq!(range.last(), Node(0x24));
        assert_eq!(range.nodes().last(), Some(Node(0x24)));
        assert!(range.contains(Node(0x14)));
        assert!(!range.contains(Node(0x01)));
    }
}

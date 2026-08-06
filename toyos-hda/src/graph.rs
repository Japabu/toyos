//! The codec as a graph of widgets, and the bounds that make walking it safe.
//!
//! Boundary contract: everything here is built from responses a codec gave.
//! The `MAX_*` bounds are policy, not physics — a walk over a list a device
//! wrote needs a ceiling that is not the device's — and what a codec past one
//! loses is the part past it.

use alloc::vec::Vec;

use crate::caps::{AmpCaps, ConfigDefault, PcmCaps, PinCaps, WidgetCaps, WidgetKind};
use crate::verb::{Address, Node, Response, Subordinates};

pub const MAX_FUNCTION_GROUPS: usize = 8;
pub const MAX_WIDGETS: usize = 128;
pub const MAX_CONNECTIONS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FunctionKind {
    Audio,
    Modem,
    Other(u8),
}

impl FunctionKind {
    pub fn decode(response: Response) -> Self {
        match (response.raw() & 0xFF) as u8 {
            0x01 => Self::Audio,
            0x02 => Self::Modem,
            other => Self::Other(other),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Pin {
    pub caps: PinCaps,
    pub config: ConfigDefault,
}

#[derive(Clone, Debug)]
pub struct Widget {
    pub node: Node,
    pub caps: WidgetCaps,
    /// Already decoded from the wire's short or long form and already
    /// range-expanded, so nothing downstream sees a range marker.
    pub connections: Vec<Node>,
    pub amp_out: Option<AmpCaps>,
    pub amp_in: Option<AmpCaps>,
    pub pcm: Option<PcmCaps>,
    /// `Some` exactly when this is a pin complex.
    pub pin: Option<Pin>,
}

impl Widget {
    pub fn is_converter(&self) -> bool {
        self.caps.kind == WidgetKind::AudioOutput
    }
}

#[derive(Clone, Debug)]
pub struct FunctionGroup {
    pub node: Node,
    pub kind: FunctionKind,
    /// The range the group declared, kept so a connection naming a node
    /// outside it can be refused rather than looked up and missed.
    pub range: Subordinates,
    pub widgets: Vec<Widget>,
}

impl FunctionGroup {
    pub fn widget(&self, node: Node) -> Option<&Widget> {
        self.widgets.iter().find(|w| w.node == node)
    }
}

#[derive(Clone, Debug)]
pub struct Codec {
    pub address: Address,
    pub vendor: u16,
    pub device: u16,
    pub groups: Vec<FunctionGroup>,
}

/// How many entries a connection list has, and in which form the codec will
/// hand them over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConnectionListLen {
    pub count: u8,
    /// Two 16-bit entries per response instead of four 8-bit ones.
    pub long: bool,
}

impl ConnectionListLen {
    pub fn decode(response: Response) -> Self {
        let raw = response.raw();
        Self { count: (raw & 0x7F) as u8, long: raw & (1 << 7) != 0 }
    }

    /// How many responses it takes to read the whole list.
    pub fn responses(self) -> usize {
        let per = if self.long { 2 } else { 4 };
        (self.count as usize).div_ceil(per)
    }
}

/// Expand a connection list into the nodes it names.
///
/// A range entry — the top bit of an entry, at 7 or 15 depending on the form —
/// means "every node from the one before this up to this one". Two things it
/// is not allowed to do: run backwards, and run longer than [`MAX_CONNECTIONS`]
/// leaves room for. Both are refusals of the whole list rather than a truncated
/// answer, because a list this function had to guess at is a route to a
/// converter nobody asked for.
pub fn decode_connections(len: ConnectionListLen, responses: &[Response]) -> Option<Vec<Node>> {
    let wanted = (len.count as usize).min(MAX_CONNECTIONS);
    let mut raw: Vec<u16> = Vec::with_capacity(wanted);
    let per = if len.long { 2 } else { 4 };
    for response in responses {
        for slot in 0..per {
            if raw.len() == wanted {
                break;
            }
            raw.push(if len.long {
                ((response.raw() >> (slot * 16)) & 0xFFFF) as u16
            } else {
                ((response.raw() >> (slot * 8)) & 0xFF) as u16
            });
        }
    }
    if raw.len() < wanted {
        return None;
    }

    let range_bit: u16 = if len.long { 1 << 15 } else { 1 << 7 };
    let mut out: Vec<Node> = Vec::new();
    let mut previous: Option<u16> = None;
    for entry in raw {
        let id = entry & !range_bit;
        if id > u8::MAX as u16 {
            return None;
        }
        if entry & range_bit != 0 {
            let from = previous?;
            if id <= from || (id - from) as usize + out.len() > MAX_CONNECTIONS {
                return None;
            }
            out.extend((from + 1..=id).map(|node| Node(node as u8)));
        } else {
            if out.len() == MAX_CONNECTIONS {
                return None;
            }
            out.push(Node(id as u8));
        }
        previous = Some(id);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(raw: u32) -> Response {
        Response::new(raw).unwrap()
    }

    fn short(count: u8) -> ConnectionListLen {
        ConnectionListLen { count, long: false }
    }

    #[test]
    fn the_t14_s_headphone_pin_names_two_converters() {
        // node 0x21 conn-len=0x00000002, list [2, 3].
        let len = ConnectionListLen::decode(response(0x0000_0002));
        assert_eq!(len, short(2));
        assert_eq!(len.responses(), 1);
        let list = decode_connections(len, &[response(0x0000_0302)]).unwrap();
        assert_eq!(list, [Node(2), Node(3)]);
    }

    #[test]
    fn the_t14_s_speaker_pin_names_one() {
        let len = ConnectionListLen::decode(response(0x0000_0001));
        let list = decode_connections(len, &[response(0x0000_0002)]).unwrap();
        assert_eq!(list, [Node(2)]);
    }

    #[test]
    fn a_six_entry_mixer_list_spans_two_responses() {
        // node 0x22 conn-len=6, list [24, 25, 26, 27, 29, 19].
        let len = ConnectionListLen::decode(response(0x0000_0006));
        assert_eq!(len.responses(), 2);
        let list =
            decode_connections(len, &[response(0x1B1A_1918), response(0x0000_131D)]).unwrap();
        assert_eq!(list, [Node(24), Node(25), Node(26), Node(27), Node(29), Node(19)]);
    }

    #[test]
    fn a_range_entry_expands_from_the_entry_before_it() {
        let list = decode_connections(short(2), &[response(0x0000_8502)]).unwrap();
        assert_eq!(list, [Node(2), Node(3), Node(4), Node(5)]);
    }

    #[test]
    fn a_range_that_runs_backwards_is_not_a_range() {
        assert_eq!(decode_connections(short(2), &[response(0x0000_8205)]), None);
    }

    #[test]
    fn a_range_with_nothing_before_it_is_refused() {
        assert_eq!(decode_connections(short(1), &[response(0x0000_0085)]), None);
    }

    #[test]
    fn a_range_longer_than_the_bound_is_refused_rather_than_truncated() {
        // 1 -> 254 is 253 entries, far past MAX_CONNECTIONS, and a truncation
        // would hand back a list the codec never described.
        assert_eq!(decode_connections(short(2), &[response(0x0000_FE01)]), None);
    }

    #[test]
    fn a_long_form_entry_carries_sixteen_bits() {
        let len = ConnectionListLen { count: 2, long: true };
        assert_eq!(len.responses(), 1);
        let list = decode_connections(len, &[response(0x0003_0002)]).unwrap();
        assert_eq!(list, [Node(2), Node(3)]);
    }

    #[test]
    fn a_long_form_node_past_the_node_space_is_refused() {
        let len = ConnectionListLen { count: 1, long: true };
        assert_eq!(decode_connections(len, &[response(0x0000_0100)]), None);
    }

    #[test]
    fn a_list_shorter_than_it_declared_is_refused() {
        assert_eq!(decode_connections(short(8), &[response(0x0403_0201)]), None);
    }
}

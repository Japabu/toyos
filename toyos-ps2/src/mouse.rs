//! The standard 3-byte PS/2 mouse packet.
//!
//! ```text
//! byte 0: [yovf][xovf][ysign][xsign][ 1 ][mid][right][left]
//! byte 1: dx, low 8 bits — the sign is in byte 0
//! byte 2: dy, low 8 bits — the sign is in byte 0, and POSITIVE MEANS UP
//! ```
//!
//! No IntelliMouse extension: the TrackPoint has no wheel, and a fixed frame
//! is what makes resync possible at all. Three things here are one line away
//! from a bug that no "the cursor moved" test would catch — the deltas are
//! **9-bit**, so `byte as i8` reverses direction on a fast flick; dy points the
//! opposite way from the screen; and the frame has no start marker, so what
//! resyncs it is the idle gap between packets and nothing in the bytes.

/// Motion is reported only when neither overflow bit is set; the field is
/// meaningless when they are, but the buttons in the same byte are not.
///
/// `Pending` and `Discarded` were one `None` variant, and collapsing them cost
/// a field investigation: two of every three bytes of a *healthy* stream are
/// mid-packet, so a driver reporting "bytes that produced no event" named them,
/// and a log line reading `no event from [aux 0x08, aux 0x06, …]` is
/// indistinguishable from a decoder that has lost the frame. Only the byte the
/// framer actually threw away is evidence of anything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseOutcome {
    /// Taken into a packet still being assembled. Whether these bytes produced
    /// anything is not known until the packet ends.
    Pending,
    /// Thrown away at a packet boundary: bit 3 says it cannot be a head. The
    /// count of these is how far a desync ran, and the fact that it is bounded
    /// is the whole of the byte-level resync.
    Discarded,
    /// Buttons in HID boot-mouse order (bit 0 left, 1 right, 2 middle),
    /// which is the PS/2 order unchanged. `dy` is already screen-oriented.
    Packet { buttons: u8, dx: i32, dy: i32 },
    /// `0xAA 0x00`: the device reset itself. Data reporting is off again,
    /// so the driver has to re-enable it or the pointer goes silent.
    Reset,
}

/// A byte this long after the previous one starts a packet, whatever the
/// decoder was waiting for.
///
/// This is the only real resync this wire format has. Bit 3 is set in *every*
/// legal head byte but also in plenty of legal body bytes — `dx = 10` is a
/// perfectly good head — so discarding bytes without it cannot bound anything:
/// a stream misaligned by one byte completes a bogus 3-byte group every time,
/// returns to `Head`, and stays misaligned for as long as the motion lasts.
///
/// Timing settles it. A PS/2 byte is 11 bits at 10–16.7 kHz, so a 3-byte packet
/// occupies at most ~3.3 ms; the driver programs 100 samples/s, so the line is
/// then idle for ~6.7 ms. 5 ms falls in that trough and nowhere else, which
/// makes an idle gap the frame delimiter the format itself lacks.
pub const PACKET_GAP_NS: u64 = 5_000_000;

const ALWAYS_ONE: u8 = 0x08;
const X_SIGN: u8 = 0x10;
const Y_SIGN: u8 = 0x20;
const OVERFLOW: u8 = 0xC0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Waiting for a byte that could be the head of a packet.
    Head,
    /// Saw `0xAA` at a packet boundary. It is both the reset announcement and
    /// a legal head byte, so the next byte decides which.
    MaybeReset,
    /// Byte 0 accepted; `count` bytes of the body collected so far.
    Body { head: u8, count: u8, byte1: u8 },
}

pub struct MouseDecoder {
    state: State,
    /// Arrival time of the previous byte, not of the packet's head: the gap
    /// that delimits packets is between two adjacent bytes.
    last_ns: u64,
}

impl Default for MouseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl MouseDecoder {
    pub const fn new() -> Self {
        Self { state: State::Head, last_ns: 0 }
    }

    /// Abandon any partial packet. The response to a hole in the byte stream.
    pub fn reset(&mut self) {
        self.state = State::Head;
    }

    /// `now_ns` is when this byte *arrived*, not when the batch it came in was
    /// drained: the gap between adjacent bytes is the whole resync mechanism,
    /// and one timestamp per drain would flatten every gap to zero.
    pub fn feed(&mut self, byte: u8, now_ns: u64) -> MouseOutcome {
        if now_ns.saturating_sub(self.last_ns) > PACKET_GAP_NS {
            self.state = State::Head;
        }
        self.last_ns = now_ns;

        match self.state {
            State::Head => {
                if byte == 0xAA {
                    self.state = State::MaybeReset;
                    return MouseOutcome::Pending;
                }
                // Bit 3 reads 1 in every legal head byte, so a byte without it
                // cannot be one. It rules bytes out, never in — the gap above
                // is what rules one in.
                if byte & ALWAYS_ONE == 0 {
                    return MouseOutcome::Discarded;
                }
                self.state = State::Body { head: byte, count: 0, byte1: 0 };
                MouseOutcome::Pending
            }
            State::MaybeReset => {
                self.state = State::Head;
                if byte == 0x00 {
                    return MouseOutcome::Reset;
                }
                // Not a reset after all: 0xAA is a legal head byte (Y overflow
                // set, so its motion is dropped and its right button stands)
                // and this byte is its first body byte.
                self.state = State::Body { head: 0xAA, count: 1, byte1: byte };
                MouseOutcome::Pending
            }
            State::Body { head, count, byte1 } => {
                if count == 0 {
                    self.state = State::Body { head, count: 1, byte1: byte };
                    return MouseOutcome::Pending;
                }
                self.state = State::Head;
                MouseOutcome::Packet {
                    buttons: head & 0x07,
                    dx: delta(head, X_SIGN, byte1),
                    dy: -delta(head, Y_SIGN, byte),
                }
            }
        }
    }
}

/// 9-bit sign extension, and zero when the field overflowed.
///
/// The overflow bit reports **9**-bit overflow, so a genuine +200 count has
/// no overflow set and reinterpreting the byte as `i8` yields −56: a fast
/// flick would reverse direction.
fn delta(head: u8, sign_bit: u8, value: u8) -> i32 {
    if head & OVERFLOW != 0 {
        return 0;
    }
    value as i32 - if head & sign_bit != 0 { 256 } else { 0 }
}

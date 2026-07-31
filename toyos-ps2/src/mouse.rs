//! The standard 3-byte PS/2 mouse packet.
//!
//! ```text
//! byte 0: [yovf][xovf][ysign][xsign][ 1 ][mid][right][left]
//! byte 1: dx, low 8 bits — the sign is in byte 0
//! byte 2: dy, low 8 bits — the sign is in byte 0, and POSITIVE MEANS UP
//! ```
//!
//! No IntelliMouse extension: the TrackPoint has no wheel, and a fixed frame
//! is what makes resync trivially self-healing. Two things here are one line
//! away from a bug that no "the cursor moved" test would catch — the deltas
//! are **9-bit**, so `byte as i8` reverses direction on a fast flick, and dy
//! points the opposite way from the screen.

/// Motion is reported only when neither overflow bit is set; the field is
/// meaningless when they are, but the buttons in the same byte are not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseOutcome {
    /// Mid-packet, or a byte discarded to resync.
    None,
    /// Buttons in HID boot-mouse order (bit 0 left, 1 right, 2 middle),
    /// which is the PS/2 order unchanged. `dy` is already screen-oriented.
    Packet { buttons: u8, dx: i32, dy: i32 },
    /// `0xAA 0x00`: the device reset itself. Data reporting is off again,
    /// so the driver has to re-enable it or the pointer goes silent.
    Reset,
}

/// A partial packet older than this is abandoned. Catches the case a lost
/// byte leaves behind: parked mid-packet with no further traffic to resync
/// against, so the next real packet would be framed one byte off forever.
pub const STALE_PARTIAL_NS: u64 = 50_000_000;

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
    started_ns: u64,
}

impl Default for MouseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl MouseDecoder {
    pub const fn new() -> Self {
        Self { state: State::Head, started_ns: 0 }
    }

    /// Abandon any partial packet. The response to a hole in the byte stream.
    pub fn reset(&mut self) {
        self.state = State::Head;
    }

    pub fn feed(&mut self, byte: u8, now_ns: u64) -> MouseOutcome {
        if !matches!(self.state, State::Head)
            && now_ns.saturating_sub(self.started_ns) > STALE_PARTIAL_NS
        {
            self.state = State::Head;
        }

        match self.state {
            State::Head => {
                if byte == 0xAA {
                    self.state = State::MaybeReset;
                    self.started_ns = now_ns;
                    return MouseOutcome::None;
                }
                // Bit 3 reads 1 in every legal head byte. A byte without it
                // cannot be one, and discarding it *without* advancing is
                // what bounds resync at two packets from any offset.
                if byte & ALWAYS_ONE == 0 {
                    return MouseOutcome::None;
                }
                self.state = State::Body { head: byte, count: 0, byte1: 0 };
                self.started_ns = now_ns;
                MouseOutcome::None
            }
            State::MaybeReset => {
                self.state = State::Head;
                if byte == 0x00 {
                    return MouseOutcome::Reset;
                }
                // Not a reset after all: 0xAA was a legal head byte (both
                // overflow bits set, so its motion is dropped anyway) and
                // this byte is its first body byte.
                self.state = State::Body { head: 0xAA, count: 1, byte1: byte };
                MouseOutcome::None
            }
            State::Body { head, count, byte1 } => {
                if count == 0 {
                    self.state = State::Body { head, count: 1, byte1: byte };
                    return MouseOutcome::None;
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

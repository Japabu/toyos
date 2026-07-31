//! Scancode set 1 → HID usage.
//!
//! Set 1 is what arrives when the keyboard is in set 2 and the controller's
//! translation bit is on — Linux's default configuration, and therefore the
//! best-trodden path on an EC we cannot debug. The translation table lives in
//! the controller, so it costs this crate nothing; what is left is three
//! states (base, `0xE0`, `0xE1`) and two 128-byte tables.

/// Unmapped. Never emitted.
const X: u8 = 0x00;

/// `scancode & 0x7F` → HID usage. High bit of the scancode means release.
#[rustfmt::skip]
pub const SET1: [u8; 128] = [
//  0x00  0x01  0x02  0x03  0x04  0x05  0x06  0x07
    X,    0x29, 0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23,
//  0x08  0x09  0x0A  0x0B  0x0C  0x0D  0x0E  0x0F
    0x24, 0x25, 0x26, 0x27, 0x2D, 0x2E, 0x2A, 0x2B,
//  Q     W     E     R     T     Y     U     I
    0x14, 0x1A, 0x08, 0x15, 0x17, 0x1C, 0x18, 0x0C,
//  O     P     [     ]     Enter LCtrl A     S
    0x12, 0x13, 0x2F, 0x30, 0x28, 0xE0, 0x04, 0x16,
//  D     F     G     H     J     K     L     ;
    0x07, 0x09, 0x0A, 0x0B, 0x0D, 0x0E, 0x0F, 0x33,
//  '     `     LShft \     Z     X     C     V
    0x34, 0x35, 0xE1, 0x31, 0x1D, 0x1B, 0x06, 0x19,
//  B     N     M     ,     .     /     RShft KP*
    0x05, 0x11, 0x10, 0x36, 0x37, 0x38, 0xE5, 0x55,
//  LAlt  Space Caps  F1    F2    F3    F4    F5
    0xE2, 0x2C, 0x39, 0x3A, 0x3B, 0x3C, 0x3D, 0x3E,
//  F6    F7    F8    F9    F10   NumLk ScrLk KP7
    0x3F, 0x40, 0x41, 0x42, 0x43, 0x53, 0x47, 0x5F,
//  KP8   KP9   KP-   KP4   KP5   KP6   KP+   KP1
    0x60, 0x61, 0x56, 0x5C, 0x5D, 0x5E, 0x57, 0x59,
//  KP2   KP3   KP0   KP.   0x54  0x55  ISO   F11
    0x5A, 0x5B, 0x62, 0x63, X,    X,    0x64, 0x44,
//  F12   0x59..0x5F
    0x45, X,    X,    X,    X,    X,    X,    X,
    X,    X,    X,    X,    X,    X,    X,    X,
    X,    X,    X,    X,    X,    X,    X,    X,
    X,    X,    X,    X,    X,    X,    X,    X,
    X,    X,    X,    X,    X,    X,    X,    X,
];

/// `scancode & 0x7F` → HID usage, for the byte after an `0xE0` prefix.
///
/// `0x2A` and `0x36` are deliberately unmapped: under translation PrtScn is
/// `E0 2A E0 37` make / `E0 B7 E0 AA` break, and mapping them would emit a
/// phantom Shift press and release around every PrtScn.
#[rustfmt::skip]
pub const SET1_E0: [u8; 128] = [
    X,    X,    X,    X,    X,    X,    X,    X,
    X,    X,    X,    X,    X,    X,    X,    X,
    X,    X,    X,    X,    X,    X,    X,    X,
//  0x18  0x19  0x1A  0x1B  KPEnt RCtrl 0x1E  0x1F
    X,    X,    X,    X,    0x58, 0xE4, X,    X,
    X,    X,    X,    X,    X,    X,    X,    X,
//  0x28  0x29  fake  0x2B  0x2C  0x2D  0x2E  0x2F
    X,    X,    X,    X,    X,    X,    X,    X,
//  0x30  0x31  0x32  0x33  0x34  KP/   fake  PrtSc
    X,    X,    X,    X,    X,    0x54, X,    0x46,
//  RAlt  0x39  0x3A  0x3B  0x3C  0x3D  0x3E  0x3F
    0xE6, X,    X,    X,    X,    X,    X,    X,
//  0x40  0x41  0x42  0x43  0x44  0x45  0x46  Home
    X,    X,    X,    X,    X,    X,    X,    0x4A,
//  Up    PgUp  0x4A  Left  0x4C  Right KP-?  End
    0x52, 0x4B, X,    0x50, X,    0x4F, X,    0x4D,
//  Down  PgDn  Ins   Del   0x54  0x55  0x56  0x57
    0x51, 0x4E, 0x49, 0x4C, X,    X,    X,    X,
//  0x58  0x59  0x5A  LGUI  RGUI  Menu  0x5E  0x5F
    X,    X,    X,    0xE3, 0xE7, 0x65, X,    X,
    X,    X,    X,    X,    X,    X,    X,    X,
    X,    X,    X,    X,    X,    X,    X,    X,
    X,    X,    X,    X,    X,    X,    X,    X,
    X,    X,    X,    X,    X,    X,    X,    X,
];

/// What one scancode byte produced.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyOutcome {
    /// A prefix, a swallowed sequence byte, or a code this keyboard has and
    /// ToyOS does not.
    None,
    Key { usage: u8, pressed: bool },
    /// The keyboard lost bytes (`0x00`/`0xFF` are the set-1 overrun and
    /// detection-error codes). Everything held must be released, because the
    /// break codes for it may be among what was lost.
    Lost,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Base,
    /// After `0xE0`.
    Extended,
    /// After `0xE1` (Pause): the fixed count of bytes still to swallow.
    Pause(u8),
}

/// Set-1 scancodes in, key transitions out.
///
/// Deliberately holds no record of what is down. The kernel's `keyboard`
/// module keeps one held-set across every keyboard in the machine and drops
/// a transition to a state a usage is already in, so a typematic repeat
/// costs nothing there. A second held-set here would be derived state that
/// can disagree with it — and would wedge a key in the one case where they
/// do: usage released on the *other* keyboard while this one still holds it.
pub struct KeyDecoder {
    state: State,
}

impl Default for KeyDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyDecoder {
    pub const fn new() -> Self {
        Self { state: State::Base }
    }

    /// Drop any half-decoded sequence. The response to a hole in the byte
    /// stream: a prefix whose second byte was lost would otherwise mis-decode
    /// the next unrelated byte.
    pub fn reset(&mut self) {
        self.state = State::Base;
    }

    pub fn feed(&mut self, byte: u8) -> KeyOutcome {
        match self.state {
            // Pause is `E1 1D 45 E1 9D C5` and means nothing in ToyOS.
            // Swallowing a fixed count is what stops it desyncing the stream.
            State::Pause(remaining) => {
                self.state = if remaining <= 1 { State::Base } else { State::Pause(remaining - 1) };
                KeyOutcome::None
            }
            State::Extended => {
                self.state = State::Base;
                emit(SET1_E0[(byte & 0x7F) as usize], byte & 0x80 == 0)
            }
            State::Base => match byte {
                0xE0 => {
                    self.state = State::Extended;
                    KeyOutcome::None
                }
                0xE1 => {
                    self.state = State::Pause(5);
                    KeyOutcome::None
                }
                // Neither is a reachable set-1 code: scancode 0 is unused, so
                // neither its make (0x00) nor 0x7F's break (0xFF) can be a
                // key. `0xAA` is NOT in this list — under translation it is
                // left Shift's break code, and the two are indistinguishable.
                0x00 | 0xFF => KeyOutcome::Lost,
                _ => emit(SET1[(byte & 0x7F) as usize], byte & 0x80 == 0),
            },
        }
    }
}

fn emit(usage: u8, pressed: bool) -> KeyOutcome {
    if usage == 0 {
        KeyOutcome::None
    } else {
        KeyOutcome::Key { usage, pressed }
    }
}

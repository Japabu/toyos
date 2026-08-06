//! A root-hub port that behaves like the register, so the machine is tested
//! against hardware's rules rather than against the test author's memory of
//! them.
//!
//! Everything here is xHCI 1.2 §5.4.8's own semantics: the change flags are
//! write-1-to-clear, PR is write-1-to-set and the *controller* clears it, PED
//! is set by a reset that finds a device and cleared by a write of '1'. The
//! last of those is what QEMU does not implement and what disabled every port
//! on the T14.

use toyos_xhci::port::Nanos;
use toyos_xhci::Portsc;

const CCS: u32 = 1 << 0;
const PED: u32 = 1 << 1;
const PR: u32 = 1 << 4;
const PP: u32 = 1 << 9;
const SPEED_SHIFT: u32 = 10;
const CSC: u32 = 1 << 17;
const PRC: u32 = 1 << 21;
const CHANGES: u32 = 0x7F << 17;
const READ_ONLY: u32 = CCS | (1 << 3) | (0xF << SPEED_SHIFT) | (1 << 30);
const READ_WRITE_SAME: u32 = (0xF << 5) | PP | (0x3 << 14) | (0x7 << 25);

const PLS_SHIFT: u32 = 5;
const PLS_U0: u32 = 0;
const PLS_RX_DETECT: u32 = 5;
const PLS_INACTIVE: u32 = 6;
const WRC: u32 = 1 << 19;
const WPR: u32 = 1 << 31;

/// What a port does when it is asked to reset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResetBehaviour {
    /// Completes after this long, sets PRC and enables the port.
    Completes { after: Nanos },
    /// Never finishes. A marginal cable, or a controller that refuses.
    Never,
    /// **A SuperSpeed port given a hot reset it cannot take.** PR is accepted
    /// and the link falls over: no PRC ever comes, and the link state goes
    /// Inactive, which §4.19.1.2.4 says only a warm reset leaves. This is what
    /// the T14's USB-A ports do and the state QEMU has no way to produce.
    HotResetKillsTheLink { warm_works: bool },
}

pub struct FakePort {
    raw: u32,
    speed: u8,
    behaviour: ResetBehaviour,
    resetting_since: Option<Nanos>,
    /// Whether the reset in flight is a warm one.
    warm: bool,
    /// A SuperSpeed port: it trains its own link and reads Enabled when it is
    /// up, with no reset from the driver at all.
    superspeed: bool,
    /// Every write the driver made, for the assertions that are about what it
    /// did rather than about where it ended up.
    pub writes: Vec<u32>,
}

impl FakePort {
    pub fn empty(behaviour: ResetBehaviour) -> Self {
        Self {
            raw: PP | (PLS_RX_DETECT << PLS_SHIFT),
            speed: 3,
            behaviour,
            resetting_since: None,
            warm: false,
            superspeed: false,
            writes: Vec::new(),
        }
    }

    /// A port with a device already in it and its connect flag already raised,
    /// which is every populated port on a machine that has just powered its
    /// root hub.
    pub fn occupied(behaviour: ResetBehaviour) -> Self {
        let mut port = Self::empty(behaviour);
        port.attach();
        port
    }

    pub fn read(&self) -> Portsc {
        Portsc::from_raw(self.raw)
    }

    pub fn raw(&self) -> u32 {
        self.raw
    }

    /// A SuperSpeed port. Its link trains itself: a device appearing brings the
    /// port to Enabled with the link at U0 and no reset from anybody, which is
    /// §4.19.1.2's own sequence and the thing a USB2-shaped driver resets away.
    pub fn superspeed(behaviour: ResetBehaviour) -> Self {
        Self { superspeed: true, ..Self::empty(behaviour) }
    }

    pub fn attach(&mut self) {
        if self.raw & CCS == 0 {
            self.raw |= CCS | CSC;
            if self.superspeed {
                self.raw |= PED | ((self.speed as u32) << SPEED_SHIFT);
                self.set_link(PLS_U0);
            }
        }
    }

    pub fn detach(&mut self) {
        if self.raw & CCS != 0 {
            self.raw = (self.raw & !(CCS | PED | PR)) | CSC;
            self.resetting_since = None;
        }
    }

    /// A device pulled and pushed back between two of the driver's looks. The
    /// level ends where it started and only the edge records that anything
    /// happened, which is the whole point.
    pub fn replug(&mut self) {
        self.detach();
        self.attach();
    }

    pub fn write(&mut self, value: u32, now: Nanos) {
        self.writes.push(value);
        // Read-only bits ignore the write; read-write-same bits take it.
        let mut next = (self.raw & READ_ONLY) | (value & READ_WRITE_SAME);
        // The change flags are RW1C, so a '0' leaves one alone.
        next |= self.raw & CHANGES & !(value & CHANGES);
        // PED is RW1CS: a written '1' disables the port, a '0' leaves it.
        if self.raw & PED != 0 && value & PED == 0 {
            next |= PED;
        }
        // PR and WPR are both RW1S and the controller clears them, never the
        // driver. A warm reset drives PR too, so what tells them apart is which
        // bit the driver wrote.
        let hot = value & PR != 0 && self.raw & CCS != 0;
        let warm = value & WPR != 0 && self.raw & CCS != 0;
        if hot || warm {
            next = (next | PR) & !PED;
        } else {
            next |= self.raw & PR;
        }
        let started = next & PR != 0 && self.raw & PR == 0;
        self.raw = next;
        if started {
            self.warm = warm;
            self.resetting_since = Some(now);
        }
    }

    /// Let time pass. A reset in flight completes here, because a reset is the
    /// one thing a port does on its own clock.
    pub fn tick(&mut self, now: Nanos) {
        let Some(since) = self.resetting_since else { return };
        let after = match self.behaviour {
            ResetBehaviour::Completes { after } => after,
            ResetBehaviour::Never => return,
            ResetBehaviour::HotResetKillsTheLink { warm_works } => {
                if !self.warm {
                    // The link went down when the hot reset hit it, and it is
                    // not coming back on its own.
                    self.set_link(PLS_INACTIVE);
                    self.raw &= !(PR | PED);
                    self.resetting_since = None;
                    return;
                }
                if !warm_works {
                    return;
                }
                1_000_000
            }
        };
        if now.saturating_sub(since) < after {
            return;
        }
        self.resetting_since = None;
        let warm = self.warm;
        self.raw &= !(PR | WPR);
        self.raw |= PRC;
        if warm {
            self.raw |= WRC;
        }
        if self.raw & CCS != 0 {
            self.raw |= PED | ((self.speed as u32) << SPEED_SHIFT);
            self.set_link(PLS_U0);
        }
    }

    fn set_link(&mut self, pls: u32) {
        self.raw = (self.raw & !(0xF << PLS_SHIFT)) | (pls << PLS_SHIFT);
    }
}

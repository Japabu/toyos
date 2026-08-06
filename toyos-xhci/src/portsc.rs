//! One root-hub port register, read and written.
//!
//! xHCI 1.2 §5.4.8 Table 5-27 for the bits, Table 5-28 for the link state.

/// A PORTSC word the driver has read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Portsc(u32);

/// What a port's link is doing (§5.4.8 Table 5-28).
///
/// Only the states this driver decides on are named. The rest are carried
/// verbatim, because a controller reporting one is a machine to describe rather
/// than to guess about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkState {
    /// U0 — the link is up and the port is usable.
    Active,
    /// The link failed and **only a warm reset gets it back** (§4.19.1.2.4). A
    /// USB3 port answering a hot reset it could not take lands here, which is
    /// where a driver with no warm reset stops having a port.
    Inactive,
    /// RxDetect: nothing is there yet, or the far end has not answered.
    RxDetect,
    /// Polling: the link is training. Neither up nor failed, so neither reset
    /// applies — waiting is what this state asks for.
    Polling,
    Other(u8),
}

impl LinkState {
    const fn decode(raw: u8) -> Self {
        match raw {
            0 => Self::Active,
            5 => Self::RxDetect,
            6 => Self::Inactive,
            7 => Self::Polling,
            other => Self::Other(other),
        }
    }
}

/// CCS — a device is attached. Read-only.
const CCS: u32 = 1 << 0;
/// PED — the port is enabled. RW1CS, which is why nothing here can set it.
const PED: u32 = 1 << 1;
/// OCA — over-current. Read-only.
const OCA: u32 = 1 << 3;
/// PR — port reset. RW1S.
const PR: u32 = 1 << 4;
/// PLS — the link state. RWS, written only through LWS, which this never sets.
const PLS: u32 = 0xF << 5;
/// WRC — a warm reset finished. RW1CS, and USB3 ports only.
const WRC: u32 = 1 << 19;
/// WPR — warm port reset. RW1S, USB3 ports only, and the only way out of
/// Inactive (§4.19.1.2.4).
const WPR: u32 = 1 << 31;
/// PP — port power. RWS.
const PP: u32 = 1 << 9;
/// The speed the port trained at. Read-only, and §4.19.5 says it is not valid
/// until PR has gone from '1' to '0'.
const SPEED: u32 = 0xF << 10;
/// PIC — port indicator control. RWS.
const PIC: u32 = 0x3 << 14;
/// CSC — the connect state changed.
const CSC: u32 = 1 << 17;
/// PRC — the reset finished.
const PRC: u32 = 1 << 21;
/// The wake enables. RWS.
const WAKE: u32 = 0x7 << 25;
/// DR — the device is removable. Read-only.
const DR: u32 = 1 << 30;

/// Every change flag: CSC, PEC, WRC, OCC, PRC, PLC and CEC, bits 17 to 23.
///
/// They are the reason a port event exists at all — §4.19.2 raises a Port
/// Status Change Event when one of these goes '0' to '1' and **only then**, so
/// a flag left set is a change the controller has already reported and will not
/// report again.
const CHANGES: u32 = 0x7F << 17;

/// The bits a write cannot change.
const READ_ONLY: u32 = CCS | OCA | SPEED | DR;
/// The bits where writing back what was read reproduces the port's state.
const READ_WRITE_SAME: u32 = PLS | PP | PIC | WAKE;

impl Portsc {
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    pub const fn connected(self) -> bool {
        self.0 & CCS != 0
    }

    pub const fn enabled(self) -> bool {
        self.0 & PED != 0
    }

    /// Whether the connect state has changed since this flag was last cleared.
    ///
    /// **CCS is a level and this is the edge, and only the edge can report a
    /// gap.** §5.4.8 sets CSC on a '0'→'1' *or* a '1'→'0' transition, so a port
    /// that reads connected with this set was disconnected and reconnected in
    /// between — which is what a person replugging a mouse does.
    pub const fn connect_changed(self) -> bool {
        self.0 & CSC != 0
    }

    /// Whether a reset of either kind has finished. A warm reset sets WRC and a
    /// conformant controller sets PRC with it; one flag is enough to act on and
    /// both are cleared together.
    pub const fn reset_changed(self) -> bool {
        self.0 & (PRC | WRC) != 0
    }

    /// What the link is doing, which on a USB3 port is what decides whether a
    /// reset is needed at all and which kind.
    pub const fn link_state(self) -> LinkState {
        LinkState::decode(((self.0 & PLS) >> 5) as u8)
    }

    pub const fn any_change(self) -> bool {
        self.0 & CHANGES != 0
    }

    /// What the port trained at, valid only after a reset has completed.
    pub const fn speed(self) -> u8 {
        ((self.0 & SPEED) >> 10) as u8
    }

    /// The value to build a write on top of, so that setting one bit sets
    /// exactly that bit.
    ///
    /// Everything outside [`READ_ONLY`] and [`READ_WRITE_SAME`] *acts* on a
    /// written '1': PR and WPR are RW1S, and PED and the seven change flags are
    /// RW1C. Writing back what was read therefore does something, and for PED
    /// what it does is take the port from Enabled to Disabled (§4.19.1.1.6).
    pub const fn neutral(self) -> Write {
        Write(self.0 & (READ_ONLY | READ_WRITE_SAME))
    }
}

impl core::fmt::Debug for Portsc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PORTSC({:#010x}", self.0)?;
        for (bit, name) in [
            (CCS, "CCS"),
            (PED, "PED"),
            (PR, "PR"),
            (CSC, "CSC"),
            (PRC, "PRC"),
        ] {
            if self.0 & bit != 0 {
                write!(f, " {name}")?;
            }
        }
        write!(f, " speed={})", self.speed())
    }
}

/// A PORTSC write under construction.
///
/// It can only be built from a [`Portsc::neutral`], and nothing here sets PED,
/// so the two ways to disable a port a driver was trying to enable are both
/// unreachable rather than guarded: writing back a read word, and setting PED
/// and PR together, which §5.4.8 note 82 calls undefined behaviour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Write(u32);

impl Write {
    /// Clear the change flags that were set in the word this was read from.
    ///
    /// Only those, so a flag raised between the read and the write survives to
    /// the next pass — which is what RW1C is for.
    pub const fn acknowledging(self, seen: Portsc) -> Self {
        Self(self.0 | (seen.0 & CHANGES))
    }

    /// Clear the reset-finished flag and **only** that one.
    ///
    /// What the enumeration acknowledges, because it is the flag it consumed. A
    /// connect change set while the reset ran means the device was replugged
    /// during it, and that has to survive to be acted on — so an enumeration
    /// that acknowledged everything would swallow the evidence that what it
    /// just brought up is already gone.
    pub const fn acknowledging_reset(self) -> Self {
        Self(self.0 | PRC | WRC)
    }

    /// Reset the port, which for a USB2 port is how a device is enabled at all.
    /// On a USB3 port this is a *hot* reset of a link that is already trained.
    pub const fn resetting(self) -> Self {
        Self(self.0 | PR)
    }

    /// Warm-reset the port: a full re-training of the link, and the only way
    /// out of [`LinkState::Inactive`]. USB3 ports only — a USB2 port has no
    /// such bit and writing it does nothing.
    pub const fn warm_resetting(self) -> Self {
        Self(self.0 | WPR)
    }

    /// Power the port. HCRST returns every root-hub port to the state it has
    /// with nothing attached, and on a controller with Port Power Control that
    /// state is unpowered — a port with no power reports no device, for the
    /// life of the boot.
    pub const fn powered(self) -> Self {
        Self(self.0 | PP)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// The write a driver makes when it hands the register back what it read.
    /// Compiled only for the negative gate that requires this to be wrong.
    #[cfg(feature = "flaws")]
    pub const fn whole_word(read: Portsc) -> Self {
        Self(read.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bit positions, against the table. A decoder nothing checks is a
    /// decoder nobody knows is wrong, and every higher test in this crate reads
    /// the world through this one.
    #[test]
    fn bits_are_where_the_table_puts_them() {
        assert!(Portsc::from_raw(1 << 0).connected());
        assert!(Portsc::from_raw(1 << 1).enabled());
        assert!(Portsc::from_raw(1 << 17).connect_changed());
        assert!(Portsc::from_raw(1 << 21).reset_changed());
        assert_eq!(Portsc::from_raw(4 << 10).speed(), 4);
        assert!(!Portsc::from_raw(0).any_change());
        for bit in 17..=23 {
            assert!(Portsc::from_raw(1 << bit).any_change(), "bit {bit}");
        }
        assert!(!Portsc::from_raw(1 << 24).any_change());
        assert!(!Portsc::from_raw(1 << 16).any_change());
    }

    /// The finding `xhci-portsc-rw1c` exists for, as a host test: a write built
    /// from a read word must not carry PED back.
    #[test]
    fn a_write_never_carries_ped() {
        let enabled = Portsc::from_raw(CCS | PED | PRC | PP | (4 << 10));
        let write = enabled.neutral().acknowledging(enabled);
        assert_eq!(write.raw() & PED, 0, "{write:?} would disable the port");
        assert_eq!(write.raw() & PR, 0);
        assert_ne!(write.raw() & PRC, 0, "the flag that was set must be cleared");
        assert_ne!(write.raw() & PP, 0, "power is read-write-same and must survive");
    }

    /// And it never sets PED and PR together, which the specification calls
    /// undefined behaviour.
    #[test]
    fn a_write_never_sets_ped_and_pr_together() {
        let word = Portsc::from_raw(u32::MAX);
        let write = word.neutral().acknowledging(word).resetting();
        assert_eq!(write.raw() & PED, 0, "{write:?}");
        assert_ne!(write.raw() & PR, 0);
    }

    /// A change flag raised between the read and the write is not cleared by
    /// it, so the next pass still sees it.
    #[test]
    fn an_unseen_change_survives_the_acknowledge() {
        let seen = Portsc::from_raw(CCS | PRC);
        // CSC went up after the read.
        let write = seen.neutral().acknowledging(seen);
        assert_eq!(write.raw() & CSC, 0, "a flag nobody looked at must not be cleared");
    }

    /// A neutral write reproduces the read-only and read-write-same halves and
    /// nothing else, for every word.
    #[test]
    fn neutral_keeps_exactly_the_bits_that_are_safe_to_write_back() {
        for raw in [0, u32::MAX, 0x5555_5555, 0xAAAA_AAAA, CCS | PED | PR | PRC] {
            let write = Portsc::from_raw(raw).neutral();
            assert_eq!(write.raw(), raw & (READ_ONLY | READ_WRITE_SAME), "{raw:#010x}");
            assert_eq!(write.raw() & CHANGES, 0, "{raw:#010x}");
            assert_eq!(write.raw() & (PED | PR), 0, "{raw:#010x}");
        }
    }
}

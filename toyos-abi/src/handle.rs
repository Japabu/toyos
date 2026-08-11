//! What a process names a kernel object with, and what it is allowed to do to
//! it.
//!
//! A handle is a *slot in one process's own table*. It designates nothing
//! outside that table: a number lifted out of another process's log, or counted
//! up from zero, resolves to that process's own slot or to nothing at all.
//! That is the whole difference between this and the ids it replaces — a
//! `PipeId`, a `SharedToken` and a service name were all machine-wide, so
//! holding one *was* the authority.

/// One entry in a process's handle table.
///
/// Twelve bits of slot and twenty of generation, in one `u32` so it costs a
/// register at the syscall boundary. **A slot at generation 0 encodes as the
/// bare index**, which is what keeps stdio literally `0`, `1`, `2` and the std
/// fork's stdio plumbing untouched.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RawHandle(pub u32);

impl RawHandle {
    const SLOT_BITS: u32 = 12;
    const SLOT_MASK: u32 = (1 << Self::SLOT_BITS) - 1;

    /// Slots one process may hold. Bounds the table; `MAX_HANDLES` is the name
    /// the kernel refuses by.
    pub const MAX_SLOTS: usize = 1 << Self::SLOT_BITS;

    /// A slot is retired rather than reissued at this generation, so
    /// [`HANDLE_INVALID`] — which encodes slot 4095 at this generation — is
    /// never a handle anything holds.
    pub const MAX_GENERATION: u32 = (1 << (32 - Self::SLOT_BITS)) - 1;

    pub const fn new(slot: u16, generation: u32) -> Self {
        Self((generation << Self::SLOT_BITS) | (slot as u32 & Self::SLOT_MASK))
    }

    pub const fn slot(self) -> u16 {
        (self.0 & Self::SLOT_MASK) as u16
    }

    pub const fn generation(self) -> u32 {
        self.0 >> Self::SLOT_BITS
    }
}

/// No handle. What an absent optional argument is spelled as, and never a slot
/// any table issues.
pub const HANDLE_INVALID: RawHandle = RawHandle(u32::MAX);

/// What the holder of a handle may do with the object behind it.
///
/// Rights only ever shrink: a duplicate may ask for a subset and nothing
/// widens. There is no default — every syscall states what it needs, because a
/// right left unstated is a right each call site invents.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rights(u32);

impl Rights {
    pub const NONE: Rights = Rights(0);
    /// May be duplicated. A [device claim] is created without it, which is what
    /// makes at most one handle to a claim expressible.
    ///
    /// [device claim]: crate::syscall::SYS_DEVICE_CLAIM
    pub const DUP: Rights = Rights(1 << 0);
    /// May be endowed at spawn or sent over a connection.
    pub const TRANSFER: Rights = Rights(1 << 1);
    pub const READ: Rights = Rights(1 << 2);
    pub const WRITE: Rights = Rights(1 << 3);
    /// Shared memory, a pipe's ring page, an io_uring ring.
    pub const MAP: Rights = Rights(1 << 4);
    /// Block on it, or name it in an io_uring `POLL_ADD`.
    pub const WAIT: Rights = Rights(1 << 5);
    /// Kill a process; on a `SysCap`, open one by pid.
    pub const MANAGE: Rights = Rights(1 << 6);
    /// On a `SysCap`: enter the RT band.
    pub const RT: Rights = Rights(1 << 7);
    /// On a `SysCap`: mint a device claim.
    pub const DEVICE: Rights = Rights(1 << 8);
    /// On a `SysCap`: read the whole machine's kernel log.
    ///
    /// [`SYS_LOG_READ`] answers every record every CPU wrote, which is every
    /// process's business and no process's right by default. `/bin/logd` holds
    /// it because writing `/log` is its job, `/bin/console` because it paints
    /// the panel, and `test-runner` because a gate reads what the kernel said.
    ///
    /// [`SYS_LOG_READ`]: crate::syscall::SYS_LOG_READ
    pub const LOG: Rights = Rights(1 << 9);

    /// Every bit that has a caller. A wider set than this is a bug in whoever
    /// composed it, not a right nobody uses.
    pub const ALL: Rights = Rights(0x3ff);

    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !Self::ALL.0 == 0 { Some(Rights(bits)) } else { None }
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }

    pub const fn without(self, other: Rights) -> Rights {
        Rights(self.0 & !other.0)
    }

    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn subset_of(self, of: Rights) -> bool {
        self.0 & !of.0 == 0
    }
}

/// Named bits rather than a number, because the one place this is read is a
/// refusal saying which right was missing.
impl core::fmt::Debug for Rights {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const NAMES: [(Rights, &str); 10] = [
            (Rights::DUP, "DUP"),
            (Rights::TRANSFER, "TRANSFER"),
            (Rights::READ, "READ"),
            (Rights::WRITE, "WRITE"),
            (Rights::MAP, "MAP"),
            (Rights::WAIT, "WAIT"),
            (Rights::MANAGE, "MANAGE"),
            (Rights::RT, "RT"),
            (Rights::DEVICE, "DEVICE"),
            (Rights::LOG, "LOG"),
        ];
        if self.0 == 0 {
            return f.write_str("NONE");
        }
        let mut first = true;
        for (right, name) in NAMES {
            if self.contains(right) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_zero_is_the_bare_index() {
        for slot in [0u16, 1, 2, 4095] {
            assert_eq!(RawHandle::new(slot, 0).0, u32::from(slot));
        }
    }

    #[test]
    fn a_handle_answers_the_slot_and_generation_it_was_made_from() {
        let h = RawHandle::new(4095, RawHandle::MAX_GENERATION - 1);
        assert_eq!(h.slot(), 4095);
        assert_eq!(h.generation(), RawHandle::MAX_GENERATION - 1);
        assert_ne!(h, HANDLE_INVALID);
    }

    /// The one encoding a table must never issue, and the reason a slot is
    /// retired at `MAX_GENERATION` rather than wrapped.
    #[test]
    fn nothing_below_the_retirement_generation_can_be_the_invalid_handle() {
        assert_eq!(
            HANDLE_INVALID,
            RawHandle::new((RawHandle::MAX_SLOTS - 1) as u16, RawHandle::MAX_GENERATION)
        );
    }

    #[test]
    fn rights_only_shrink() {
        let full = Rights::READ.union(Rights::WRITE).union(Rights::DUP);
        assert!(Rights::READ.subset_of(full));
        assert!(full.subset_of(full));
        assert!(!Rights::MAP.subset_of(full));
        assert!(full.contains(Rights::READ.union(Rights::WRITE)));
        assert_eq!(full.without(Rights::DUP), Rights::READ.union(Rights::WRITE));
    }

    #[test]
    fn a_bit_no_right_uses_is_not_a_rights_set() {
        assert!(Rights::from_bits(Rights::ALL.bits()).is_some());
        assert!(Rights::from_bits(Rights::ALL.bits() | (1 << 31)).is_none());
    }

    /// **`ALL` and the `Debug` table are two spellings of one set**, and a
    /// right added to one and not the other is invisible in both directions: a
    /// missing `ALL` bit makes `from_bits` refuse a legitimate set, and a
    /// missing name makes the one place a right is ever printed — a refusal
    /// saying which one was absent — omit it without saying so.
    ///
    /// The second assertion is the one that has teeth against a *future* right:
    /// an unnamed non-zero bit renders as the empty string rather than as
    /// anything a reader would question.
    #[test]
    fn every_right_in_all_has_a_name_and_every_name_is_in_all() {
        let named = |r: Rights| std::format!("{r:?}").split('|').count();
        assert_eq!(
            named(Rights::ALL),
            Rights::ALL.bits().count_ones() as usize,
            "a bit in ALL has no name in Debug: {:?}",
            Rights::ALL
        );
        assert_eq!(
            std::format!("{:?}", Rights(u32::MAX)),
            std::format!("{:?}", Rights::ALL),
            "a named right is outside ALL, so from_bits refuses a set that has it"
        );
        assert_eq!(std::format!("{:?}", Rights::LOG), "LOG");
    }
}

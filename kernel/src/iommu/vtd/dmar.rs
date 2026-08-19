//! The `DMAR` table: what firmware says about this machine's remapping units.
//!
//! Firmware bytes are untrusted input, and `drivers::acpi`'s module doc
//! already states what follows: nothing on this path panics for any input, and
//! a table that cannot be believed is a refusal naming what it was refused on.
//! Every read here goes through [`Table::field`], whose bound is the length
//! `find_table` validated and checksummed once.
//!
//! The walk is `parse_madt`'s, deliberately and not by coincidence: an entry
//! declaring zero length, or a length running past the list that holds it,
//! ends it. There is no way to resynchronise a self-describing list that lied
//! about an element's size, and a walk that tries is a walk over whatever
//! follows. The same rule applies one level down, where a device scope is
//! bounded by its own structure rather than by the table.
//!
//! Nothing here allocates and nothing here is stored. A [`Dmar`] is a copy of
//! a validated `(base, len)` pair and every structure in it is an offset into
//! that; the table stays in the direct map where firmware left it. That keeps
//! this stage honest — I1 has no consumer for an inventory, and a `Vec` built
//! for a reader that does not exist yet is state nobody can check.

use core::mem::size_of;

use crate::drivers::acpi::{find_table, Table, TableError};
use crate::iommu::StreamId;

/// Header (36) + Host Address Width (1) + Flags (1) + 10 reserved. The first
/// remapping structure starts here, and it is what a `DMAR` must be long
/// enough to declare before any of it is read.
const REMAPPING_STRUCTURES: usize = 48;

const HOST_ADDRESS_WIDTH: usize = 36;
const FLAGS: usize = 37;

/// Flags bit 0. Its absence is the platform declaring it cannot remap
/// interrupts — one of the conditions that will one day refuse the machine at
/// boot, because an interrupt is a memory write that DMA remapping does not
/// see.
pub const FLAG_INTR_REMAP: u8 = 1 << 0;
/// Flags bit 1: firmware asks the OS not to enable x2APIC mode.
pub const FLAG_X2APIC_OPT_OUT: u8 = 1 << 1;
/// Flags bit 2: the platform asks that DMA be blocked until the OS has
/// programmed the unit. Honoured for free by this kernel's ordering — nothing
/// is told to DMA before translation is on — so it is logged and costs nothing.
pub const FLAG_DMA_CTRL_OPT_IN: u8 = 1 << 2;

/// A list element that declared a size the list cannot hold: where it starts,
/// and what it claimed. The last thing a walk yields.
pub struct Malformed {
    pub at: usize,
    pub declared: usize,
}

/// A validated `DMAR`, and the two fields of its own header.
///
/// Those two are read in [`Dmar::open`] rather than behind accessors because
/// what makes them readable is the table's declared length, which is a fact
/// about the whole table and not about each read.
#[derive(Clone, Copy)]
pub struct Dmar {
    table: Table,
    /// The widest physical address the units on this machine can produce.
    /// Firmware reports it one less than it is; this is the width.
    pub host_address_width: u16,
    pub flags: u8,
}

impl Dmar {
    pub fn open(rsdp_addr: u64) -> Result<Self, TableError> {
        let table = find_table(rsdp_addr, b"DMAR", REMAPPING_STRUCTURES)?;
        let length = || TableError::Length {
            declared: table.len() as u32,
            needed: REMAPPING_STRUCTURES,
        };
        let haw: u8 = table.field(HOST_ADDRESS_WIDTH).ok_or_else(length)?;
        let flags: u8 = table.field(FLAGS).ok_or_else(length)?;
        Ok(Self { table, host_address_width: haw as u16 + 1, flags })
    }

    pub fn structures(&self) -> Structures {
        Structures { table: self.table, offset: REMAPPING_STRUCTURES }
    }
}

/// One remapping structure.
pub enum Structure {
    Drhd(Drhd),
    Rmrr(Rmrr),
    /// A type this kernel walks past. Carried rather than dropped so the skip
    /// can be logged with its type: a machine carrying an `ATSR` — which §10.1
    /// rejects along with device-TLB itself — is a machine somebody will want
    /// to know is under-configured, rather than silently served.
    Skipped { kind: u16, at: usize, len: usize },
}

pub struct Structures {
    table: Table,
    offset: usize,
}

/// Every type this kernel knows the name of. The rest are still skipped by
/// length and still logged — by number, which is the honest thing to print for
/// a structure a later revision of the specification added.
pub fn structure_name(kind: u16) -> &'static str {
    match kind {
        0 => "DRHD",
        1 => "RMRR",
        2 => "ATSR",
        3 => "RHSA",
        4 => "ANDD",
        5 => "SATC",
        6 => "SIDP",
        _ => "unknown",
    }
}

impl Iterator for Structures {
    type Item = Result<Structure, Malformed>;

    fn next(&mut self) -> Option<Self::Item> {
        let at = self.offset;
        let kind: u16 = self.table.field(at)?;
        let declared: u16 = self.table.field(at + 2)?;
        let len = declared as usize;
        if len < 4 || at.checked_add(len)? > self.table.len() {
            // Past the end of the table, so the next call reads nothing and
            // the walk stops here whatever the caller does with this item.
            self.offset = self.table.len();
            return Some(Err(Malformed { at, declared: len }));
        }
        self.offset = at + len;
        Some(Ok(match kind {
            0 => Structure::Drhd(Drhd { table: self.table, at, len }),
            1 => Structure::Rmrr(Rmrr { table: self.table, at, len }),
            kind => Structure::Skipped { kind, at, len },
        }))
    }
}

/// A field of a variable-length structure, bounded by the length that
/// structure declared and not by the table's.
///
/// The difference is the whole point: a DRHD declaring sixteen bytes has no
/// register base, and reading one out of it would read the *next* structure's
/// bytes and call them a physical address. Zero outside the declared length,
/// which is a value the caller's own bound refuses by name.
fn bounded<T: Copy + Default>(table: Table, at: usize, len: usize, offset: usize) -> T {
    match offset.checked_add(size_of::<T>()) {
        Some(end) if end <= len => table.field(at + offset).unwrap_or_default(),
        _ => T::default(),
    }
}

/// Type 0: one hardware unit's register window and the devices in its scope.
#[derive(Clone, Copy)]
pub struct Drhd {
    table: Table,
    at: usize,
    len: usize,
}

/// DRHD flags bit 0: this unit is the catch-all for everything on its segment
/// that no other unit's scope names. The specification requires it last.
const DRHD_INCLUDE_PCI_ALL: u8 = 1 << 0;

const DRHD_FLAGS: usize = 4;
const DRHD_SEGMENT: usize = 6;
const DRHD_REGISTER_BASE: usize = 8;
const DRHD_SCOPES: usize = 16;

impl Drhd {
    pub fn include_pci_all(&self) -> bool {
        self.field::<u8>(DRHD_FLAGS) & DRHD_INCLUDE_PCI_ALL != 0
    }

    pub fn segment(&self) -> u16 {
        self.field(DRHD_SEGMENT)
    }

    /// Where the unit's 4 KiB register window is. Firmware's number, so the
    /// caller checks it before mapping anything at it.
    pub fn register_base(&self) -> u64 {
        self.field(DRHD_REGISTER_BASE)
    }

    pub fn scopes(&self) -> Scopes {
        Scopes { table: self.table, offset: self.at + DRHD_SCOPES, end: self.at + self.len }
    }

    fn field<T: Copy + Default>(&self, offset: usize) -> T {
        bounded(self.table, self.at, self.len, offset)
    }
}

/// Type 1: a physical range firmware requires stay identity-mapped for the
/// devices in its scope.
#[derive(Clone, Copy)]
pub struct Rmrr {
    table: Table,
    at: usize,
    len: usize,
}

const RMRR_SEGMENT: usize = 6;
const RMRR_BASE: usize = 8;
const RMRR_LIMIT: usize = 16;
const RMRR_SCOPES: usize = 24;

impl Rmrr {
    pub fn segment(&self) -> u16 {
        self.field(RMRR_SEGMENT)
    }

    pub fn base(&self) -> u64 {
        self.field(RMRR_BASE)
    }

    /// Inclusive, as firmware states it.
    pub fn limit(&self) -> u64 {
        self.field(RMRR_LIMIT)
    }

    pub fn scopes(&self) -> Scopes {
        Scopes { table: self.table, offset: self.at + RMRR_SCOPES, end: self.at + self.len }
    }

    fn field<T: Copy + Default>(&self, offset: usize) -> T {
        bounded(self.table, self.at, self.len, offset)
    }
}

/// A device scope list, bounded by its parent structure's declared length.
pub struct Scopes {
    table: Table,
    offset: usize,
    end: usize,
}

pub struct Scope {
    table: Table,
    at: usize,
    len: usize,
}

const SCOPE_ENUMERATION_ID: usize = 4;
const SCOPE_START_BUS: usize = 5;
/// Type (1) + Length (1) + 2 reserved + Enumeration ID (1) + Start Bus (1).
/// The `(device, function)` path follows, and it is what makes a scope
/// variable-length.
const SCOPE_PATH: usize = 6;

impl Iterator for Scopes {
    type Item = Result<Scope, Malformed>;

    fn next(&mut self) -> Option<Self::Item> {
        let at = self.offset;
        if at + SCOPE_PATH > self.end {
            return None;
        }
        let len = self.table.field::<u8>(at + 1)? as usize;
        if len < SCOPE_PATH || at + len > self.end {
            self.offset = self.end;
            return Some(Err(Malformed { at, declared: len }));
        }
        self.offset = at + len;
        Some(Ok(Scope { table: self.table, at, len }))
    }
}

impl Scope {
    /// 1 PCI endpoint, 2 PCI sub-hierarchy, 3 I/O APIC, 4 MSI-capable HPET,
    /// 5 ACPI namespace device.
    pub fn kind(&self) -> u8 {
        self.field(0)
    }

    pub fn kind_name(&self) -> &'static str {
        match self.kind() {
            1 => "pci-endpoint",
            2 => "pci-bridge",
            3 => "ioapic",
            4 => "hpet",
            5 => "acpi-device",
            _ => "unknown",
        }
    }

    /// The I/O APIC id or HPET number this scope names, for the two scope
    /// types that carry one. §6.3 needs the first: every redirection entry the
    /// kernel has already programmed has to be reprogrammed into remappable
    /// form before `IRE` is set, and this is which unit each belongs to.
    pub fn enumeration_id(&self) -> u8 {
        self.field(SCOPE_ENUMERATION_ID)
    }

    pub fn start_bus(&self) -> u8 {
        self.field(SCOPE_START_BUS)
    }

    /// `(device, function)` from the start bus down to the named device.
    pub fn path(&self) -> impl Iterator<Item = (u8, u8)> + '_ {
        let entries = (self.len - SCOPE_PATH) / 2;
        (0..entries).map(move |i| {
            let at = SCOPE_PATH + i * 2;
            (self.field(at), self.field(at + 1))
        })
    }

    /// The requester id of the device this scope names, when the scope names
    /// it directly.
    ///
    /// `None` for a path through one or more bridges. The requester id there
    /// is not in the table: it needs each bridge's secondary bus number read
    /// out of that bridge's own config space, and a function that guessed
    /// instead would hand back a number naming a different device. Every scope
    /// QEMU publishes and every one the T14 is expected to publish has a
    /// single-element path; the bridge walk belongs to stage I2, where there
    /// is an ECAM window to walk it with.
    pub fn stream_id(&self) -> Option<StreamId> {
        let mut path = self.path();
        let (device, function) = path.next()?;
        path.next().is_none().then(|| StreamId::pci(self.start_bus(), device, function))
    }

    fn field<T: Copy + Default>(&self, offset: usize) -> T {
        bounded(self.table, self.at, self.len, offset)
    }
}

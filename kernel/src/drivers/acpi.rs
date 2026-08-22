//! ACPI table parsing.
//!
//! Every byte here is **firmware-supplied, i.e. untrusted input**, and the
//! distinction CLAUDE.md draws applies in full: fail-fast is for kernel bugs,
//! not for a length a machine we have never booted put in a header. So this
//! module has no panic on any input path. A table that cannot be believed is a
//! [`TableError`] with a reason, and the caller decides — because the callers
//! want opposite things from the same refusal. The i8042 driver must tell
//! "firmware says there is no 8042" from "firmware's answer is not readable",
//! because it prints them differently and they are different facts; it probes
//! on both, since its own handshake is better evidence than either.
//!
//! Nothing reads a table except through [`Table::open`], which is what makes
//! the bounds hold by construction rather than by review. The two subtractions
//! that replaced — `length - size_of::<SdtHeader>()` in the XSDT walk and
//! `length - size_of::<Madt>()` in the MADT walk — were both "the header
//! declared itself shorter than its own header", and with overflow checks off
//! the first one produces an entry count near 2^61 and walks arbitrary
//! physical memory as if it were a table pointer array.

use alloc::vec::Vec;
use core::mem::{offset_of, size_of};
use core::ptr::{read_unaligned, read_volatile};
use core::sync::atomic::{AtomicU16, Ordering};
use crate::log;
use crate::DirectMap;

pub struct MadtInfo {
    pub apic_ids: Vec<u32>,
    pub io_apics: Vec<IoApicEntry>,
    pub source_overrides: Vec<SourceOverride>,
}

/// MADT type 1: one I/O APIC's register window and the GSI its first
/// redirection entry carries.
pub struct IoApicEntry {
    pub id: u8,
    pub address: u32,
    pub gsi_base: u32,
}

/// MADT type 2: an ISA IRQ that does not land on the identity GSI, and/or
/// does not use the ISA default of edge-triggered active-high. `flags` is the
/// raw MPS INTI word — bits 0-1 polarity, 2-3 trigger mode.
pub struct SourceOverride {
    pub bus: u8,
    pub source_irq: u8,
    pub gsi: u32,
    pub flags: u16,
}

/// Why a firmware table cannot be used.
///
/// Every variant is a distinct instruction to the caller, which is the whole
/// reason this is not an `Option`. `Absent` is firmware answering the
/// question; everything else is firmware failing to answer it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TableError {
    /// The RSDP UEFI handed us has the wrong signature or does not checksum.
    BadRsdp,
    /// An ACPI 1.0 root pointer, or a null XSDT address. There is no RSDT
    /// fallback and there will not be one: this kernel is UEFI-only and every
    /// machine it targets publishes an XSDT.
    NoXsdt,
    /// No table with that signature in the XSDT.
    Absent,
    /// The declared length cannot hold the fields the caller reads, or is
    /// outside anything a real table has.
    Length { declared: u32, needed: usize },
    /// The declared bytes do not sum to zero. Which table is not carried
    /// here: every call site names the table in its own log line, and a
    /// `[u8; 4]` renders as four decimal numbers nobody reads as "FACP".
    Checksum,
}

/// The largest length a table may declare. A DSDT is the biggest table anyone
/// ships and those run to tens of KiB, so this is three orders of magnitude of
/// headroom — and it is what bounds the checksum walk and every derived entry
/// count to something the direct map certainly covers. Policy, not physics.
const MAX_TABLE_LEN: usize = 1024 * 1024;

/// x86-64's architectural physical-address ceiling is 52 bits — CPUID's
/// MAXPHYADDR may be smaller, never larger — so a table pointer at or above
/// this is not an address at all. It is also what stops `DirectMap::as_ptr`'s
/// unchecked `+ PHYS_OFFSET` from wrapping a firmware-supplied `u64` round
/// into the user half, where the read would be a fault at best.
const MAX_PHYS: u64 = 1 << 52;

/// A physical address that could name a table, or `None`.
///
/// The one constructor of [`Mapped`], which is why it returns one: an address
/// firmware supplied is not dereferenceable until it has been through here, and
/// making that the *type* is what stops the next reader from taking a `u64` and
/// a `DirectMap` and going straight to a pointer.
fn table_at(phys: u64) -> Option<Mapped> {
    (phys != 0 && phys < MAX_PHYS).then(|| Mapped(DirectMap::from_phys(phys)))
}

/// A firmware-supplied physical address that [`table_at`] has bounded, and the
/// only thing in this module that can be read through.
///
/// **This is where nine `unsafe` blocks went.** Every field read here used to
/// be its own `read_unaligned(base.as_ptr::<u8>().add(offset).cast())` at the
/// call site — nine of them, each restating the same argument — and the ones
/// in [`xsdt`] restated it about an address that had never been through
/// `table_at` at all. Concentrating them in the two accessors below makes the
/// bound a property of the type instead of a habit at nine call sites, and the
/// accessors are safe because `Mapped` cannot be built out of an address the
/// bound does not hold for.
///
/// What the bound is: [`MAX_PHYS`] is x86-64's architectural 52-bit physical
/// ceiling, and `mm` direct-maps all of physical memory at
/// [`crate::mm::PHYS_OFFSET`], so `phys + PHYS_OFFSET` is a mapped kernel
/// address that cannot wrap into the user half. What the bound is *not* is a
/// claim that a table lives there — that is [`Table::open`]'s job, and it is
/// why reading past a table's declared length still goes through [`Table`].
#[derive(Clone, Copy)]
pub struct Mapped(DirectMap);

impl Mapped {
    /// A copy of the `T` at `offset`. Unaligned by construction: ACPI tables
    /// are byte-packed and the direct map does not align them.
    fn field<T: Copy>(self, offset: usize) -> T {
        // SAFETY: irreducible — this is the module's one dereference of a
        // firmware-supplied address, so there is no safe primitive left to
        // express it in terms of. Sound because `Mapped` is only ever built by
        // `table_at`, which refused 0 and anything at or above `MAX_PHYS`, and
        // the direct map covers every physical address below that; `offset` is
        // bounded by the caller (`Table::field` against the declared length,
        // `Table::open`/`xsdt` against `size_of::<SdtHeader>()`/`size_of::<Rsdp>()`,
        // both of which are smaller than the `MAX_TABLE_LEN`/`RSDP_MAX_LEN` the
        // checksum already walked). `read_unaligned` because the tables are
        // `#[repr(C, packed)]` and nothing aligns them.
        unsafe { read_unaligned(self.0.as_ptr::<u8>().add(offset).cast::<T>()) }
    }

    /// One byte, volatile — what the checksum walk reads, kept apart from
    /// [`Mapped::field`] so the integrity check keeps reading exactly the bytes
    /// it did before, once each and in order.
    fn byte(self, offset: usize) -> u8 {
        // SAFETY: irreducible for the same reason as `field`, and sound on the
        // same bound; `offset` is `0..len` where the caller bounded `len` by
        // `MAX_TABLE_LEN` or `RSDP_MAX_LEN` before calling.
        unsafe { read_volatile(self.0.as_ptr::<u8>().add(offset)) }
    }
}

impl core::fmt::Display for Mapped {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

// ACPI table structures. All packed: tables are not guaranteed aligned, and
// these exist to derive offsets with `offset_of!` rather than to be
// dereferenced — a reference to one would assert that every byte to the end of
// the struct is readable, which is exactly what the firmware has not proved.

#[repr(C, packed)]
struct SdtHeader {
    signature: [u8; 4],
    length: u32,
    revision: u8,
    checksum: u8,
    oem_id: [u8; 6],
    oem_table_id: [u8; 8],
    oem_revision: u32,
    creator_id: u32,
    creator_revision: u32,
}

#[repr(C, packed)]
struct Rsdp {
    signature: [u8; 8],
    checksum: u8,
    oem_id: [u8; 6],
    revision: u8,
    rsdt_address: u32,
    // ACPI 2.0+ fields:
    length: u32,
    xsdt_address: u64,
    extended_checksum: u8,
    _reserved: [u8; 3],
}

#[repr(C, packed)]
struct Fadt {
    header: SdtHeader,
    firmware_ctrl: u32,
    dsdt: u32,
    _reserved0: u8,
    preferred_pm_profile: u8,
    sci_interrupt: u16,
    smi_command_port: u32,
    acpi_enable: u8,
    acpi_disable: u8,
    s4bios_req: u8,
    pstate_control: u8,
    pm1a_event_block: u32,
    pm1b_event_block: u32,
    pm1a_control_block: u32,
    pm1b_control_block: u32,
    pm2_control_block: u32,
    pm_timer_block: u32,
    gpe0_block: u32,
    gpe1_block: u32,
    pm1_event_length: u8,
    pm1_control_length: u8,
    pm2_control_length: u8,
    pm_timer_length: u8,
    gpe0_block_length: u8,
    gpe1_block_length: u8,
    gpe1_base: u8,
    c_state_control: u8,
    worst_c2_latency: u16,
    worst_c3_latency: u16,
    flush_size: u16,
    flush_stride: u16,
    duty_offset: u8,
    duty_width: u8,
    day_alarm: u8,
    month_alarm: u8,
    century: u8,
    iapc_boot_arch: u16,
    _reserved1: u8,
    flags: u32,
    reset_reg: [u8; 12], // Generic Address Structure
    reset_value: u8,
    arm_boot_arch: u16,
    fadt_minor_version: u8,
    x_firmware_ctrl: u64,
    x_dsdt: u64,
}

#[repr(C, packed)]
struct Madt {
    header: SdtHeader,
    local_apic_address: u32,
    flags: u32,
    // variable-length entries follow
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct MadtEntryHeader {
    entry_type: u8,
    length: u8,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct MadtLocalApic {
    header: MadtEntryHeader,
    processor_id: u8,
    apic_id: u8,
    flags: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct MadtIoApic {
    header: MadtEntryHeader,
    io_apic_id: u8,
    _reserved: u8,
    address: u32,
    gsi_base: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct MadtSourceOverride {
    header: MadtEntryHeader,
    bus: u8,
    source_irq: u8,
    gsi: u32,
    flags: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct MadtLocalX2Apic {
    header: MadtEntryHeader,
    _reserved: u16,
    x2apic_id: u32,
    flags: u32,
    _processor_uid: u32,
}

#[repr(C, packed)]
struct McfgEntry {
    base_address: u64,
    segment_group: u16,
    start_bus: u8,
    end_bus: u8,
    _reserved: u32,
}

#[repr(C, packed)]
struct HpetTable {
    header: SdtHeader,
    event_timer_block_id: u32,
    base_address: [u8; 4], // Generic Address Structure prefix (address_space, bit_width, bit_offset, access_size)
    base_address_value: u64,
}

/// The MCFG's first allocation structure sits one 8-byte reserved field past
/// the header.
const MCFG_FIRST_ENTRY: usize = size_of::<SdtHeader>() + 8;

const SLP_EN: u16 = 1 << 13;

static PM1A_CNT_PORT: AtomicU16 = AtomicU16::new(0);
static SLP_TYPA: AtomicU16 = AtomicU16::new(0);

/// A firmware table whose declared length has been checked to cover the bytes
/// the caller is about to read, and whose declared bytes sum to zero.
///
/// Constructing one is the only way to read a table in this module, and
/// [`Table::field`] is the only way to read out of one. Together they are why
/// no arithmetic here can underflow and no read can leave the table: the
/// length is validated once, at the only place it enters the module.
/// Public because the DMAR walk lives in `crate::iommu::vtd`, one table format
/// per module, and it has to read its bytes the same way everything here does.
/// What stays private is [`Table::open`] — so a table can still only be
/// obtained from [`find_table`], with its length and checksum already
/// validated, which is the invariant the module doc rests on.
#[derive(Clone, Copy)]
pub struct Table {
    base: Mapped,
    len: usize,
}

impl Table {
    /// Validate the table at `base`: signature, a length that is plausible and
    /// long enough for `needed` bytes, then the checksum over exactly the
    /// declared length.
    ///
    /// Order matters. The checksum walks `length` bytes, so it must not run
    /// until `length` has been bounded — otherwise a header declaring 4 GiB
    /// turns the integrity check into the out-of-bounds read it exists to
    /// prevent.
    fn open(base: Mapped, signature: &[u8; 4], needed: usize) -> Result<Table, TableError> {
        let found: [u8; 4] = base.field(offset_of!(SdtHeader, signature));
        if &found != signature {
            return Err(TableError::Absent);
        }
        let declared: u32 = base.field(offset_of!(SdtHeader, length));
        let len = declared as usize;
        let floor = needed.max(size_of::<SdtHeader>());
        if len < floor || len > MAX_TABLE_LEN {
            return Err(TableError::Length { declared, needed: floor });
        }
        if !sums_to_zero(base, len) {
            return Err(TableError::Checksum);
        }
        Ok(Table { base, len })
    }

    /// The declared length, already bounded by [`Table::open`]. What a walk
    /// over variable-length entries measures its own progress against.
    pub fn len(&self) -> usize {
        self.len
    }

    /// A copy of the `T` at `offset`, or `None` when the table is not that
    /// long. Unaligned by construction — ACPI tables are byte-packed and the
    /// direct map does not align them.
    ///
    /// The read is a copy rather than a reference on purpose: a `&T` into a
    /// table asserts that all of `T` is valid memory, which for a struct whose
    /// tail runs past the declared length is exactly the claim being checked.
    pub fn field<T: Copy>(&self, offset: usize) -> Option<T> {
        let end = offset.checked_add(size_of::<T>())?;
        if end > self.len {
            return None;
        }
        Some(self.base.field(offset))
    }

    /// Like [`Table::field`], but for a caller that has already proved the
    /// length covers `offset`. Used only inside the MADT walk, whose bound is
    /// re-established per entry.
    fn field_unchecked<T: Copy>(&self, offset: usize) -> T {
        self.base.field(offset)
    }
}

/// The ACPI integrity check: a table is intact when its declared bytes sum to
/// zero in 8 bits. `len` is bounded by the caller before this runs.
fn sums_to_zero(base: Mapped, len: usize) -> bool {
    let mut sum: u8 = 0;
    for i in 0..len {
        sum = sum.wrapping_add(base.byte(i));
    }
    sum == 0
}

/// The XSDT, validated, from the RSDP UEFI handed the bootloader.
///
/// The RSDP is the one structure with no self-describing length: the ACPI 1.0
/// part is 20 bytes by spec and the 2.0 part declares its own size. So the
/// first checksum covers the fixed 20 and the second covers what the RSDP
/// claims, and neither is read before the one before it passed.
fn xsdt(rsdp_addr: u64) -> Result<Table, TableError> {
    const RSDP_V1_LEN: usize = 20;
    /// An RSDP is 36 bytes today and has never been anything else; the bound
    /// exists so the extended checksum cannot be pointed at the whole map.
    const RSDP_MAX_LEN: usize = 64;

    // Through `table_at` like every other firmware address, which it was not
    // before: the RSDP's own pointer came from UEFI straight to
    // `DirectMap::from_phys`, so a zero or a value at or above `MAX_PHYS` —
    // the two cases `MAX_PHYS`'s own doc says it exists to stop — reached
    // `as_ptr` and wrapped. Refused by name now, on the path that was the only
    // one skipping the check.
    let base = table_at(rsdp_addr).ok_or(TableError::BadRsdp)?;
    let signature: [u8; 8] = base.field(0);
    if &signature != b"RSD PTR " {
        return Err(TableError::BadRsdp);
    }
    if !sums_to_zero(base, RSDP_V1_LEN) {
        return Err(TableError::BadRsdp);
    }

    let revision: u8 = base.field(offset_of!(Rsdp, revision));
    if revision < 2 {
        return Err(TableError::NoXsdt);
    }

    let declared: u32 = base.field(offset_of!(Rsdp, length));
    let len = declared as usize;
    if len < size_of::<Rsdp>() || len > RSDP_MAX_LEN {
        return Err(TableError::Length { declared, needed: size_of::<Rsdp>() });
    }
    if !sums_to_zero(base, len) {
        return Err(TableError::BadRsdp);
    }

    let address: u64 = base.field(offset_of!(Rsdp, xsdt_address));
    let root = table_at(address).ok_or(TableError::NoXsdt)?;
    Table::open(root, b"XSDT", size_of::<SdtHeader>())
}

/// The first table in the XSDT with this signature, validated far enough to
/// read `needed` bytes out of it.
///
/// The *first* signature match is the answer even when it fails to validate.
/// A second table with the same signature is not a fallback — reporting the
/// defect in the one firmware pointed at first is the whole point, and a
/// silent second choice is how a machine ends up running on a table nobody
/// looked at.
pub fn find_table(rsdp_addr: u64, signature: &[u8; 4], needed: usize) -> Result<Table, TableError> {
    let xsdt = xsdt(rsdp_addr)?;
    // `Table::open` guarantees `len >= size_of::<SdtHeader>()`, which is what
    // makes this subtraction total. It is the underflow that was here.
    let entry_count = (xsdt.len - size_of::<SdtHeader>()) / size_of::<u64>();

    for i in 0..entry_count {
        let offset = size_of::<SdtHeader>() + i * size_of::<u64>();
        let Some(phys) = xsdt.field::<u64>(offset) else { break };
        let Some(base) = table_at(phys) else { continue };
        match Table::open(base, signature, needed) {
            Err(TableError::Absent) => continue,
            other => return other,
        }
    }
    Err(TableError::Absent)
}

/// Log a refusal with the reason firmware earned, and hand the caller a
/// `None`. Every one of these lines is a line the owner of a machine that
/// will not boot needs to see; none of them may be a panic.
fn refuse<T>(what: &str, error: TableError) -> Option<T> {
    log!("ACPI: {what} unusable: {error:?}");
    None
}

/// Given the RSDP address from UEFI, parse XSDT -> MCFG -> return ECAM base address.
pub fn find_ecam_base(rsdp_addr: u64) -> Option<u64> {
    log!("ACPI: RSDP at {rsdp_addr:#x}");
    let needed = MCFG_FIRST_ENTRY + size_of::<McfgEntry>();
    let mcfg = match find_table(rsdp_addr, b"MCFG", needed) {
        Ok(table) => table,
        Err(e) => return refuse("MCFG", e),
    };
    log!("ACPI: MCFG found at {}", mcfg.base);

    let ecam_base = mcfg.field::<u64>(MCFG_FIRST_ENTRY + offset_of!(McfgEntry, base_address))?;
    log!("ACPI: ECAM base address: {ecam_base:#x}");
    Some(ecam_base)
}

/// Parse FADT and DSDT to prepare for ACPI shutdown.
///
/// A machine whose tables do not support this keeps booting without soft-off:
/// [`shutdown`] already halts when no PM1a port was ever published, so the
/// degradation is defined and the boot says which table failed. Refusing to
/// boot because firmware cannot spell its own power block would be a
/// firmware-triggered kernel panic wearing fail-fast's clothes.
pub fn init_power(rsdp_addr: u64) {
    const FADT_FOR_POWER: usize =
        offset_of!(Fadt, pm1a_control_block) + size_of::<u32>();
    const FADT_FOR_X_DSDT: usize = offset_of!(Fadt, x_dsdt) + size_of::<u64>();

    let fadt = match find_table(rsdp_addr, b"FACP", FADT_FOR_POWER) {
        Ok(table) => table,
        Err(e) => {
            log!("ACPI: FADT unusable: {e:?} — no soft-off, shutdown will halt instead");
            return;
        }
    };

    let Some(pm1a) = fadt.field::<u32>(offset_of!(Fadt, pm1a_control_block)) else {
        log!("ACPI: FADT has no PM1a control block — no soft-off");
        return;
    };
    let pm1a = pm1a as u16;

    // Prefer X_DSDT (64-bit, ACPI 2.0+) over DSDT (32-bit). A revision that
    // claims 2.0 does not prove the table is long enough to hold the field,
    // which is why the length is asked and not the revision alone.
    let revision = fadt.field::<u8>(offset_of!(Fadt, header) + offset_of!(SdtHeader, revision));
    let x_dsdt = match revision {
        Some(r) if r >= 2 => fadt.field::<u64>(offset_of!(Fadt, x_dsdt)).filter(|a| *a != 0),
        _ => None,
    };
    let dsdt_addr = match x_dsdt {
        Some(addr) => addr,
        None => fadt.field::<u32>(offset_of!(Fadt, dsdt)).unwrap_or(0) as u64,
    };
    if dsdt_addr == 0 {
        log!(
            "ACPI: FADT names no DSDT (rev {:?}, needs {FADT_FOR_X_DSDT} bytes for X_DSDT) — no soft-off",
            revision
        );
        return;
    }

    let Some(dsdt_base) = table_at(dsdt_addr) else {
        log!("ACPI: FADT points the DSDT at {dsdt_addr:#x}, which is not an address — no soft-off");
        return;
    };
    let dsdt = match Table::open(dsdt_base, b"DSDT", size_of::<SdtHeader>()) {
        Ok(table) => table,
        Err(e) => {
            log!("ACPI: DSDT at {dsdt_addr:#x} unusable: {e:?} — no soft-off");
            return;
        }
    };

    let Some(slp_typ) = find_s5_slp_typ(&dsdt) else {
        log!("ACPI: no \\_S5_ package in the DSDT — no soft-off");
        return;
    };

    PM1A_CNT_PORT.store(pm1a, Ordering::Relaxed);
    SLP_TYPA.store(slp_typ, Ordering::Relaxed);
    log!("ACPI: PM1a={pm1a:#x} SLP_TYPa={slp_typ}");
}

/// FADT revision and the IA-PC boot architecture flags.
///
/// Bit 1 of the flags is "the motherboard has a port 60/64 keyboard
/// controller", defined from revision 3. It is a vendor's summary and this
/// kernel treats it as one: the i8042 driver logs it and probes regardless,
/// because its own handshake observes the hardware directly and the T14 clears
/// the bit on a machine whose keyboard is PS/2. The revision comes back with
/// the flags so the line can say how much the claim is worth.
///
/// The error is not an absence and must not be treated as one. A caller that
/// collapses `Err` into "the 8042 is absent" turns every firmware quirk into a
/// dead keyboard with a log line blaming the vendor.
pub fn iapc_boot_arch(rsdp_addr: u64) -> Result<(u8, u16), TableError> {
    const NEEDED: usize = offset_of!(Fadt, iapc_boot_arch) + size_of::<u16>();
    let fadt = find_table(rsdp_addr, b"FACP", NEEDED)?;
    let revision = fadt
        .field::<u8>(offset_of!(Fadt, header) + offset_of!(SdtHeader, revision))
        .ok_or(TableError::Length { declared: fadt.len as u32, needed: NEEDED })?;
    let flags = fadt
        .field::<u16>(offset_of!(Fadt, iapc_boot_arch))
        .ok_or(TableError::Length { declared: fadt.len as u32, needed: NEEDED })?;
    Ok((revision, flags))
}

/// Which CMOS register holds the RTC's century, as the FADT names it.
///
/// `Ok(None)` is firmware saying this machine has no century register, which
/// the field expresses as zero. It is a real answer and the RTC decoder acts on
/// it — a two-digit year and nothing to widen it with — rather than reading
/// some register anyway. Reading 0x32 regardless is what this replaces, on the
/// strength of a comment saying most hardware puts it there.
///
/// The bound is the CMOS index space and not a policy: port 0x70's bit 7 masks
/// NMI, so an index at or above 0x80 is a register selection *and* a change to
/// how the machine handles hardware failures. Below 0x0E is the RTC's own clock
/// and status registers, none of which is a century. Firmware naming either is
/// refused by name and treated as no century register, because an index that
/// cannot be a century register is not evidence that there is one.
///
/// The error is not an absence, and a caller must not report it as one. Both
/// end in the same two-digit year, so the log line is the only thing that tells
/// the owner whether firmware said "no century register" or whether the table
/// carrying that answer could not be read at all.
pub fn rtc_century_register(rsdp_addr: u64) -> Result<Option<u8>, TableError> {
    /// The battery-backed CMOS RAM, which is where a century register can be.
    const CMOS_RAM: core::ops::RangeInclusive<u8> = 0x0E..=0x7F;

    const NEEDED: usize = offset_of!(Fadt, century) + size_of::<u8>();
    let fadt = find_table(rsdp_addr, b"FACP", NEEDED)?;
    let declared = fadt
        .field::<u8>(offset_of!(Fadt, century))
        .ok_or(TableError::Length { declared: fadt.len as u32, needed: NEEDED })?;
    // Firmware that leaves the field zero, which is how ACPI spells "this
    // machine has no century register". The FADT a guest reads is generated by
    // QEMU, so what it says is not something the host can vary.
    let index = if crate::actuator::rtc_no_century() { 0 } else { declared };

    if index == 0 {
        log!("ACPI: the FADT names no RTC century register");
        return Ok(None);
    }
    if !CMOS_RAM.contains(&index) {
        log!(
            "ACPI: the FADT puts the RTC century register at CMOS {index:#04x}, outside {:#04x}..={:#04x} — ignoring it",
            CMOS_RAM.start(),
            CMOS_RAM.end()
        );
        return Ok(None);
    }
    log!("ACPI: the FADT puts the RTC century register at CMOS {index:#04x}");
    Ok(Some(index))
}

/// Trigger ACPI S5 (soft-off) shutdown.
pub fn shutdown() -> ! {
    // Last chance: nothing drains the log ring after this point.
    crate::drivers::serial::flush_final();

    let pm1a = PM1A_CNT_PORT.load(Ordering::Relaxed);
    let slp_typ = SLP_TYPA.load(Ordering::Relaxed);

    if pm1a != 0 {
        let val = (slp_typ << 10) | SLP_EN;
        // SAFETY: `outw` asks its caller to own the port and the word, and this
        // is the one site in the kernel where the *port* is not a constant: it
        // is `PM1a_CNT_BLK` out of the FADT, so which port this is comes from
        // firmware and not from this file. That is what makes it the right one —
        // ACPI defines the S5 transition as this word written to exactly the
        // port the FADT names, and any other address would be a guess about a
        // machine whose own description is right here. `PM1A_CNT_PORT` is
        // written only by the FADT parse, the zero check above is what says the
        // parse happened, and the word is `SLP_TYPa` in its own field with
        // `SLP_EN` beside it — the PM1 control register's layout and nothing
        // this file invents. The machine is expected not to execute another
        // instruction.
        unsafe { crate::arch::cpu::outw(pm1a, val) };
    }

    crate::arch::cpu::halt();
}

/// Given the RSDP address, parse XSDT -> HPET table -> return HPET MMIO base address.
pub fn find_hpet_base(rsdp_addr: u64) -> Option<u64> {
    let needed = offset_of!(HpetTable, base_address_value) + size_of::<u64>();
    let hpet = match find_table(rsdp_addr, b"HPET", needed) {
        Ok(table) => table,
        Err(e) => return refuse("HPET", e),
    };
    let base = hpet.field::<u64>(offset_of!(HpetTable, base_address_value))?;
    log!("ACPI: HPET at {base:#x}");
    Some(base)
}

/// Parse MADT (signature "APIC") to discover per-CPU APIC IDs.
pub fn parse_madt(rsdp_addr: u64) -> Option<MadtInfo> {
    let madt = match find_table(rsdp_addr, b"APIC", size_of::<Madt>()) {
        Ok(table) => table,
        Err(e) => return refuse("MADT", e),
    };

    let mut apic_ids = Vec::new();
    let mut io_apics = Vec::new();
    let mut source_overrides = Vec::new();

    // Total because `Table::open` was given `size_of::<Madt>()` as the floor.
    // This is the second underflow that was here.
    let entries_len = madt.len - size_of::<Madt>();
    let mut offset = 0usize;

    while offset + size_of::<MadtEntryHeader>() <= entries_len {
        let base = size_of::<Madt>() + offset;
        let entry: MadtEntryHeader = madt.field_unchecked(base);
        let entry_len = entry.length as usize;
        // A zero length would not advance, and one that runs past the table is
        // firmware describing an entry it did not supply. Both end the walk:
        // there is no way to resynchronise a self-describing list that lied
        // about an element's size.
        if entry_len < size_of::<MadtEntryHeader>() || offset + entry_len > entries_len {
            log!(
                "ACPI: MADT entry at +{offset} declares {entry_len} bytes of a {entries_len}-byte list — stopping"
            );
            break;
        }

        match entry.entry_type {
            // Type 0 = Processor Local APIC
            0 if entry_len >= size_of::<MadtLocalApic>() => {
                let lapic: MadtLocalApic = madt.field_unchecked(base);
                if lapic.flags & 1 != 0 {
                    apic_ids.push(lapic.apic_id as u32);
                }
            }
            // Type 1 = I/O APIC
            1 if entry_len >= size_of::<MadtIoApic>() => {
                let io: MadtIoApic = madt.field_unchecked(base);
                io_apics.push(IoApicEntry {
                    id: io.io_apic_id,
                    address: io.address,
                    gsi_base: io.gsi_base,
                });
            }
            // Type 2 = Interrupt Source Override
            2 if entry_len >= size_of::<MadtSourceOverride>() => {
                let iso: MadtSourceOverride = madt.field_unchecked(base);
                source_overrides.push(SourceOverride {
                    bus: iso.bus,
                    source_irq: iso.source_irq,
                    gsi: iso.gsi,
                    flags: iso.flags,
                });
            }
            // Type 9 = Processor Local x2APIC (32-bit APIC IDs)
            9 if entry_len >= size_of::<MadtLocalX2Apic>() => {
                let x2: MadtLocalX2Apic = madt.field_unchecked(base);
                if x2.flags & 1 != 0 {
                    apic_ids.push(x2.x2apic_id);
                }
            }
            _ => {}
        }

        offset += entry_len;
    }

    log!("ACPI: MADT cpus={:?}", apic_ids);
    Some(MadtInfo { apic_ids, io_apics, source_overrides })
}

/// Scan DSDT AML bytecode for the \_S5_ package and extract SLP_TYPa.
///
/// The scan reads through [`Table::field`] rather than off a raw pointer, so
/// the declared length is what bounds it — the previous version took that
/// length from an unvalidated header and walked it.
fn find_s5_slp_typ(dsdt: &Table) -> Option<u16> {
    let s5 = b"_S5_";
    let len = dsdt.len;

    for i in size_of::<SdtHeader>()..len.saturating_sub(7) {
        if (0..4).any(|j| dsdt.field::<u8>(i + j) != Some(s5[j])) {
            continue;
        }
        // Expect PackageOp (0x12) after the name
        if dsdt.field::<u8>(i + 4) != Some(0x12) {
            continue;
        }

        // Parse PkgLength to skip past it
        let pkg_lead = dsdt.field::<u8>(i + 5)?;
        let pkg_len_bytes = match (pkg_lead >> 6) & 0x03 {
            0 => 1usize,
            n => (n + 1) as usize,
        };

        // Skip: "_S5_"(4) + PackageOp(1) + PkgLength + NumElements(1)
        let val_off = i + 4 + 1 + pkg_len_bytes + 1;
        let byte = dsdt.field::<u8>(val_off)?;
        return Some(if byte == 0x0A {
            // BytePrefix -- next byte is the value
            dsdt.field::<u8>(val_off + 1)? as u16
        } else {
            // ZeroOp (0x00), OneOp (0x01), or raw value
            byte as u16
        });
    }

    None
}

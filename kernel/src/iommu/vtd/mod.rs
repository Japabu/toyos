//! Intel VT-d. Every register layout and every Intel name in this subsystem is
//! at or below this module (`specs/iommu-spec.md` §3).
//!
//! Stage I1: the units are found, their register windows mapped, their
//! capabilities decoded, and each described on one line. Nothing is written
//! and nothing is refused. Every observation `specs/iommu-spec.md` §2.2 makes
//! a refusal of at I5 appears here as a line naming the register value it
//! decided on, because a refusal that says only "unsupported" is a refusal
//! nobody can act on, and this is the line that will be read off a laptop
//! panel with no serial port.
//!
//! Register offsets, and the field positions inside `CAP` and `ECAP`, come
//! from the VT-d architecture specification's register chapter. What makes
//! them *checked* rather than cited is the harness: it stages units that
//! differ in `CAP.SAGAW` and in `ECAP.IR`, and asserts the guest reports the
//! difference. A decode reading the wrong bits cannot track a register it is
//! not looking at.

pub mod dmar;

use crate::drivers::acpi::TableError;
use crate::iommu::AddressWidth;
use crate::mm::Mmio;

use dmar::{Dmar, Malformed, Scope, Scopes, Structure};

/// A unit's register window. The architecture defines 4 KiB; everything read
/// here is in the first 32 bytes of it.
const REGISTER_WINDOW: u64 = 4096;

const VER_REG: u64 = 0x00;
const CAP_REG: u64 = 0x08;
const ECAP_REG: u64 = 0x10;

/// x86-64's architectural physical-address ceiling is 52 bits, so a register
/// base at or above this is not an address at all. It is also what stops
/// `DirectMap::as_ptr`'s unchecked `+ PHYS_OFFSET` wrapping a firmware-supplied
/// `u64` round into the user half — the same bound `drivers::acpi` puts on a
/// table pointer, for the same reason.
const MAX_PHYS: u64 = 1 << 52;

/// How many units this kernel will inventory.
///
/// Policy, not physics: a walk over a list firmware wrote needs a ceiling that
/// is not the list's own. This is far above anything a chipset publishes, and
/// what a machine past it loses is the description of the units past it —
/// which it is told, rather than left to infer from a short log.
const MAX_UNITS: usize = 16;

pub fn init(rsdp_addr: u64) {
    let dmar = match Dmar::open(rsdp_addr) {
        Ok(dmar) => dmar,
        // Firmware omits the table both when the platform has no VT-d silicon
        // and when VT-d is switched off in firmware setup, and ACPI cannot
        // separate the two. §2.2: probing a hardcoded MCHBAR-relative window
        // to tell them apart is exactly the model-table guessing this project
        // bans, so the line names both and names the setting.
        Err(TableError::Absent) => {
            log!(
                "iommu: no DMAR table — this platform has no IOMMU, or VT-d is disabled in \
                 firmware setup (look for \"VT-d\" / \"Intel Virtualization Technology for \
                 Directed I/O\")"
            );
            return;
        }
        Err(e) => {
            log!("iommu: DMAR unusable: {e:?} — this machine has no IOMMU the kernel can use");
            return;
        }
    };

    log!(
        "iommu: DMAR haw={} flags={:#04x} intr_remap={} x2apic_opt_out={} dma_ctrl_opt_in={}",
        dmar.host_address_width,
        dmar.flags,
        yn(dmar.flags & dmar::FLAG_INTR_REMAP != 0),
        yn(dmar.flags & dmar::FLAG_X2APIC_OPT_OUT != 0),
        yn(dmar.flags & dmar::FLAG_DMA_CTRL_OPT_IN != 0),
    );

    let mut units = 0usize;
    let mut regions = 0usize;
    for structure in dmar.structures() {
        match structure {
            Ok(Structure::Drhd(drhd)) => {
                if units == MAX_UNITS {
                    log!("iommu: more than {MAX_UNITS} units described; the rest are not inventoried");
                    return;
                }
                describe_unit(units, &drhd);
                units += 1;
            }
            // §7.4: a kernel-owned device is in the passthrough domain and its
            // reserved regions are satisfied for free, and a device carrying
            // one is refused for userspace handoff. Both are I4's decisions;
            // here the region is reported so that a machine which has one says
            // so on its first boot. QEMU publishes none, so this arm is
            // untestable in the harness and the T14 is its first exercise.
            Ok(Structure::Rmrr(rmrr)) => {
                log!(
                    "iommu: rmrr{regions} seg={} {:#018x}..{:#018x}",
                    rmrr.segment(),
                    rmrr.base(),
                    rmrr.limit()
                );
                describe_scopes("rmrr", regions, rmrr.scopes());
                regions += 1;
            }
            Ok(Structure::Skipped { kind, at, len }) => {
                log!(
                    "iommu: DMAR structure type {kind} ({}) at +{at}, {len} bytes — not used by \
                     this kernel",
                    dmar::structure_name(kind)
                );
            }
            Err(Malformed { at, declared }) => {
                log!(
                    "iommu: DMAR structure at +{at} declares {declared} bytes the table cannot \
                     hold — stopping"
                );
            }
        }
    }

    if units == 0 {
        log!("iommu: DMAR describes no remapping unit");
    }
}

fn describe_unit(index: usize, drhd: &dmar::Drhd) {
    let base = drhd.register_base();
    // Firmware's number, and the one address in this table the kernel is about
    // to dereference. A window that is not 4 KiB-aligned, or that is outside
    // the architectural physical range, is refused by name — never clamped to
    // fit. Not checked here, and the reason it matters at I2 rather than now:
    // a base pointing into usable RAM would read plausible garbage as a
    // capability register, which costs a wrong log line today and a register
    // write into somebody's heap at the stage that programs the unit.
    if base == 0 || base % REGISTER_WINDOW != 0 || base >= MAX_PHYS {
        log!(
            "iommu: unit{index} register base {base:#x} is not a 4 KiB-aligned physical address \
             — not mapped"
        );
        return;
    }

    let regs: Mmio = crate::mm::paging::kernel()
        .lock()
        .as_mut()
        .unwrap()
        .map_mmio(base, REGISTER_WINDOW);

    let version = regs.read_u32(VER_REG);
    // §2.2 row 2, and the one case here that is distinguishable from "no unit
    // at all": firmware described a unit whose window does not decode, either
    // because it is a firmware bug or because the unit was left powered down.
    if version == u32::MAX || (version >> 4) & 0xF == 0 {
        log!(
            "iommu: unit{index} @{base:#x}: register window reads ver={version:#010x}, the unit \
             is described but not present"
        );
        return;
    }

    let caps = Capabilities { cap: regs.read_u64(CAP_REG), ecap: regs.read_u64(ECAP_REG) };
    log!(
        "iommu: unit{index} @{base:#x} seg={} pci_all={} ver={}.{} cap={:#018x} ecap={:#018x} \
         aw={} sagaw={:#04x} mgaw={} nd={} sps2m={} cm={} psi={} nfr={} fro={:#x} qi={} ir={} \
         eim={} pt={} coherent={}",
        drhd.segment(),
        yn(drhd.include_pci_all()),
        (version >> 4) & 0xF,
        version & 0xF,
        caps.cap,
        caps.ecap,
        // The one decision on this line rather than a register field: the
        // widest of 48 and 39 the unit advertises (§5.3). A unit offering
        // neither is §2.2's last row, and at I5 it is a refusal.
        match caps.address_width() {
            Some(aw) => aw.bits(),
            None => 0,
        },
        caps.sagaw(),
        caps.mgaw(),
        caps.domains(),
        yn(caps.superpage_2m()),
        yn(caps.caching_mode()),
        yn(caps.page_selective_invalidation()),
        caps.fault_recording_registers(),
        caps.fault_recording_offset(),
        yn(caps.queued_invalidation()),
        yn(caps.interrupt_remapping()),
        yn(caps.extended_interrupt_mode()),
        yn(caps.passthrough()),
        yn(caps.coherent()),
    );

    describe_scopes("unit", index, drhd.scopes());
}

fn describe_scopes(owner: &str, index: usize, scopes: Scopes) {
    for scope in scopes {
        match scope {
            Ok(scope) => log_scope(owner, index, &scope),
            Err(Malformed { at, declared }) => log!(
                "iommu: {owner}{index} device scope at +{at} declares {declared} bytes the \
                 structure cannot hold — stopping"
            ),
        }
    }
}

fn log_scope(owner: &str, index: usize, scope: &Scope) {
    match scope.stream_id() {
        Some(stream) => log!(
            "iommu: {owner}{index} scope {} {stream} id={}",
            scope.kind_name(),
            scope.enumeration_id()
        ),
        // The requester id is not in the table for a device behind a bridge —
        // see `Scope::stream_id`. What is printed is what firmware wrote, so
        // the line is still a name a bus walk can be matched against.
        None => log!(
            "iommu: {owner}{index} scope {} bus={:#04x} path={} id={} — requester id needs a \
             bridge walk",
            scope.kind_name(),
            scope.start_bus(),
            scope.path().count(),
            scope.enumeration_id()
        ),
    }
}

/// `CAP` and `ECAP`, read once and decoded on demand.
///
/// §4.2: read once at init, logged once, and then never re-read. Holding the
/// two raw values and deriving from them is what makes that true — a decode
/// that went back to the register per field would be re-reading it a dozen
/// times per boot.
struct Capabilities {
    cap: u64,
    ecap: u64,
}

impl Capabilities {
    /// `CAP.ND`: concurrent domains, encoded as the exponent.
    fn domains(&self) -> u32 {
        1u32 << (4 + 2 * (self.cap & 0x7))
    }

    /// `CAP.CM`. Read for the log line only: §5.5 invalidates after every
    /// table modification in both directions and refuses to branch on this,
    /// because the arm a machine in reach does not execute is the arm that is
    /// wrong when somebody finally runs it.
    fn caching_mode(&self) -> bool {
        self.cap & (1 << 7) != 0
    }

    /// `CAP.SAGAW`, raw. Bit *n* of this field is a page-table depth covering
    /// `30 + 9n` address bits, so bit 1 is 39-bit and bit 2 is 48-bit.
    fn sagaw(&self) -> u8 {
        ((self.cap >> 8) & 0x1F) as u8
    }

    /// The widest depth this kernel implements that the unit advertises.
    ///
    /// `None` is a unit offering neither, which §2.2 refuses at I5. 57-bit is
    /// not considered even when advertised: §11.5, a fifth level of page
    /// tables for an address space nothing here needs, and an unused level is
    /// an untested one.
    fn address_width(&self) -> Option<AddressWidth> {
        let sagaw = self.sagaw();
        if sagaw & (1 << 2) != 0 {
            Some(AddressWidth::Bits48)
        } else if sagaw & (1 << 1) != 0 {
            Some(AddressWidth::Bits39)
        } else {
            None
        }
    }

    /// `CAP.MGAW`: the widest address the unit will accept, encoded one less
    /// than it is. It bounds every IOVA, so §5.3's base is only usable if it
    /// fits under this.
    fn mgaw(&self) -> u8 {
        (((self.cap >> 16) & 0x3F) + 1) as u8
    }

    /// `CAP.SPS` bit 0: 2 MiB leaf entries. §5.4 requires it, because the
    /// kernel is 2 MiB-page-only and a 4 KiB-leaf path would be 512× the
    /// page-table memory for the same mapping and dead code on every machine
    /// in reach.
    fn superpage_2m(&self) -> bool {
        self.cap & (1 << 34) != 0
    }

    /// `CAP.PSI`. Without it every invalidation is domain-wide, which is
    /// correct and coarser — never a refusal.
    fn page_selective_invalidation(&self) -> bool {
        self.cap & (1 << 39) != 0
    }

    /// `CAP.NFR`, encoded one less than it is.
    fn fault_recording_registers(&self) -> u32 {
        (((self.cap >> 40) & 0xFF) + 1) as u32
    }

    /// `CAP.FRO`, in 16-byte units, from the start of the register window.
    fn fault_recording_offset(&self) -> u64 {
        ((self.cap >> 24) & 0x3FF) * 16
    }

    /// `ECAP.C`: page-table walks snoop the CPU cache. §5.2 reads it for this
    /// log line and for nothing else — the flush is unconditional, because the
    /// `C=0` arm is one no machine anybody here can boot would execute.
    fn coherent(&self) -> bool {
        self.ecap & (1 << 0) != 0
    }

    /// `ECAP.QI`. Absent means invalidation goes through `CCMD_REG`/
    /// `IOTLB_REG`, which is correct and slower.
    fn queued_invalidation(&self) -> bool {
        self.ecap & (1 << 1) != 0
    }

    /// `ECAP.IR`: this unit can remap interrupts. §6.1 is why its absence is a
    /// refusal rather than a reduced mode — without remapping, a driver
    /// process with a mapped BAR can inject an arbitrary vector.
    fn interrupt_remapping(&self) -> bool {
        self.ecap & (1 << 3) != 0
    }

    /// `ECAP.EIM`: 32-bit APIC ids in interrupt remap table entries.
    fn extended_interrupt_mode(&self) -> bool {
        self.ecap & (1 << 4) != 0
    }

    /// `ECAP.PT`: a context entry may name passthrough translation, which is
    /// what every kernel-owned device gets (§5.7). Absent means those devices
    /// get identity-mapped translated domains instead: same protection, more
    /// page tables.
    fn passthrough(&self) -> bool {
        self.ecap & (1 << 6) != 0
    }
}

/// One character per boolean, so a line carrying twelve of them still fits a
/// laptop panel's row. `n` is printed rather than omitted: a field whose
/// absence is its value is a field a reader cannot tell from a field the
/// kernel forgot.
fn yn(v: bool) -> char {
    if v {
        'y'
    } else {
        'n'
    }
}

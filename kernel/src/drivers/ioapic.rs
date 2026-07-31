//! I/O APIC — the only path a pin interrupt has into this kernel.
//!
//! Every device here is MSI-X and the 8259 is masked first thing in
//! `idt::init`, so before this module an ISA line had nowhere to land. Two
//! properties follow, and both are the point of the module rather than
//! incidental to it:
//!
//! - Nothing this module reads back is trusted to be a chip. An MMIO window
//!   the machine does not decode accepts every write and answers 0xFFFFFFFF,
//!   which passes a mask read-back, claims 256 entries, and makes `route`
//!   succeed into nothing — the failure mode is a green log and a dead device.
//!   The version register is the gate: a unit whose answer is not plausibly a
//!   redirection table is dropped, and `route` reads its entry back.
//! - `init` masks every redirection entry on every unit it finds. Firmware
//!   hands the OS whatever state it left the chip in, and an unmasked entry
//!   pointing at a vector we have no IDT gate for — the ACPI SCI is the
//!   obvious candidate — turns the first assertion of that pin into #GP and
//!   panics the boot. That is why `init` runs between `lidt` and the first
//!   `sti`: exception handlers are live throughout, and the window in which a
//!   stray entry could fire never opens. Accepted consequence: no ACPI SCI,
//!   so no power-button or lid events. They were a panic before.
//! - A register access is an index write followed by a data access, so it is
//!   never atomic. `TOPOLOGY` serializes it and is taken from thread context
//!   only. No ISR touches this module.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::log;
use crate::mm::Mmio;
use crate::sync::Lock;
use super::acpi::MadtInfo;

const IOREGSEL: u64 = 0x00;
const IOWIN: u64 = 0x10;

const REG_VER: u32 = 0x01;
const REG_REDTBL: u32 = 0x10;

const RTE_DELIVERY_STATUS: u32 = 1 << 12;
const RTE_POLARITY_LOW: u32 = 1 << 13;
const RTE_REMOTE_IRR: u32 = 1 << 14;
const RTE_TRIGGER_LEVEL: u32 = 1 << 15;
const RTE_MASKED: u32 = 1 << 16;

/// The redirection table is at most 240 entries (8 bits of "max redirection
/// entry", and no shipped part is near that), and a version of 0x00 or 0xFF is
/// what an undecoded MMIO window returns rather than what a chip reports. Both
/// halves come from the one register, so one implausible read condemns it.
const MAX_PLAUSIBLE_ENTRIES: u32 = 240;

/// Global System Interrupt: the flat interrupt-input space the MADT numbers
/// I/O APIC pins in. An ISA IRQ maps into it identically unless a type-2
/// override says otherwise.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Gsi(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trigger {
    Edge,
    Level,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Polarity {
    High,
    Low,
}

/// An ISA line resolved against the override table: where it actually lands
/// and how it is actually driven. Returned as one value so a caller cannot
/// take the GSI and forget the electrical properties that came with it.
#[derive(Clone, Copy, Debug)]
pub struct IsaLine {
    pub gsi: Gsi,
    pub trigger: Trigger,
    pub polarity: Polarity,
}

pub enum RouteError {
    /// No discovered unit covers this GSI. A driver that gets this must
    /// refuse to enable its device rather than assume the pin works.
    NoUnit(Gsi),
    /// Physical destination is 8 bits wide without interrupt remapping, and
    /// 0xFF in those 8 bits is the broadcast encoding — every LAPIC accepts
    /// the message, which is the same undiagnosable mis-delivery as an id that
    /// does not fit at all. Not routing it is one log line.
    DestTooWide(u32),
    /// The entry did not take. An MMIO window that decodes to nothing accepts
    /// every write and returns 0xFFFFFFFF, so a `route` that only wrote would
    /// report success on a machine where no pin is connected to anything.
    Readback { wrote: u32, read: u32 },
}

/// Hand-written because every field here is a register value, and the derive
/// prints them in decimal — on a machine with no UART these lines are read off
/// a screen repaint once.
impl core::fmt::Debug for RouteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoUnit(gsi) => write!(f, "no I/O APIC covers GSI {}", gsi.0),
            Self::DestTooWide(id) => write!(f, "apic id {id:#x} does not fit an 8-bit destination"),
            Self::Readback { wrote, read } => {
                write!(f, "wrote {wrote:#010x}, read back {read:#010x}")
            }
        }
    }
}

struct Unit {
    mmio: Mmio,
    gsi_base: u32,
    entries: u32,
}

impl Unit {
    fn read(&self, index: u32) -> u32 {
        self.mmio.write_u32(IOREGSEL, index);
        self.mmio.read_u32(IOWIN)
    }

    fn write(&self, index: u32, value: u32) {
        self.mmio.write_u32(IOREGSEL, index);
        self.mmio.write_u32(IOWIN, value);
    }
}

struct Override {
    source_irq: u8,
    gsi: u32,
    trigger: Trigger,
    polarity: Polarity,
}

struct Topology {
    units: Vec<Unit>,
    overrides: Vec<Override>,
}

static TOPOLOGY: Lock<Topology> = Lock::new(Topology {
    units: Vec::new(),
    overrides: Vec::new(),
});

pub fn init(madt: &MadtInfo) {
    let mut topology = TOPOLOGY.lock();

    for entry in &madt.io_apics {
        // 0x20 covers IOREGSEL and IOWIN; every redirection entry is reached
        // through those two, so the window never needs to be larger.
        let mmio = crate::mm::paging::kernel()
            .lock()
            .as_mut()
            .unwrap()
            .map_mmio(entry.address as u64, 0x20);
        let mut unit = Unit { mmio, gsi_base: entry.gsi_base, entries: 0 };
        let ver = unit.read(REG_VER);
        let version = ver & 0xFF;
        unit.entries = ((ver >> 16) & 0xFF) + 1;
        if version == 0x00
            || version == 0xFF
            || unit.entries > MAX_PLAUSIBLE_ENTRIES
        {
            // Believing this window would claim to cover every GSI a driver
            // ever asks for, mask 256 entries that are not there, and route
            // into the void — all of it logging green.
            log!(
                "ioapic: id={} at {:#x} IGNORED — version register {:#010x} is not a redirection table",
                entry.id,
                entry.address,
                ver
            );
            continue;
        }
        let mut masked = 0;
        for n in 0..unit.entries {
            unit.write(REG_REDTBL + 2 * n, RTE_MASKED);
            // Read back rather than trust the write: an entry this loop
            // failed to mask is the exact hazard the loop exists for, and it
            // would otherwise be discovered by the boot dying.
            if unit.read(REG_REDTBL + 2 * n) & RTE_MASKED != 0 {
                masked += 1;
            }
        }
        log!(
            "ioapic: id={} at {:#x} ver={:#04x} gsi {}..{} masked {}/{}",
            entry.id,
            entry.address,
            version,
            unit.gsi_base,
            unit.gsi_base + unit.entries - 1,
            masked,
            unit.entries
        );
        topology.units.push(unit);
    }

    // One line for the whole table, not one per entry: on a machine with no
    // UART these are read off the next boot checkpoint's repaint of the log
    // tail, which holds a fixed number of rows.
    let mut table = String::new();
    for iso in &madt.source_overrides {
        // MPS INTI flags: 00 in either field means "conforms to the bus", and
        // the ISA bus default is edge-triggered active-high.
        let polarity = match iso.flags & 0x3 {
            3 => Polarity::Low,
            _ => Polarity::High,
        };
        let trigger = match (iso.flags >> 2) & 0x3 {
            3 => Trigger::Level,
            _ => Trigger::Edge,
        };
        let _ = write!(
            table,
            "{}{}:{}->{} {}",
            if table.is_empty() { "" } else { ", " },
            iso.bus,
            iso.source_irq,
            iso.gsi,
            describe(trigger, polarity)
        );
        topology.overrides.push(Override {
            source_irq: iso.source_irq,
            gsi: iso.gsi,
            trigger,
            polarity,
        });
    }
    log!("ioapic: iso bus:irq->gsi [{}]", table);

    if topology.units.is_empty() {
        log!("ioapic: none in MADT — no pin interrupts on this machine");
    }
}

fn describe(trigger: Trigger, polarity: Polarity) -> &'static str {
    match (trigger, polarity) {
        (Trigger::Edge, Polarity::High) => "edge/high",
        (Trigger::Edge, Polarity::Low) => "edge/low",
        (Trigger::Level, Polarity::High) => "level/high",
        (Trigger::Level, Polarity::Low) => "level/low",
    }
}

/// Where ISA `irq` lands and how it is driven, or `None` when no I/O APIC
/// exists at all.
pub fn gsi_for_isa_irq(irq: u8) -> Option<IsaLine> {
    let topology = TOPOLOGY.lock();
    if topology.units.is_empty() {
        return None;
    }
    Some(
        topology
            .overrides
            .iter()
            .find(|o| o.source_irq == irq)
            .map_or(
                IsaLine {
                    gsi: Gsi(irq as u32),
                    trigger: Trigger::Edge,
                    polarity: Polarity::High,
                },
                |o| IsaLine { gsi: Gsi(o.gsi), trigger: o.trigger, polarity: o.polarity },
            ),
    )
}

fn locate(topology: &Topology, gsi: Gsi) -> Result<(&Unit, u32), RouteError> {
    topology
        .units
        .iter()
        .find(|u| gsi.0 >= u.gsi_base && gsi.0 < u.gsi_base + u.entries)
        .map(|u| (u, gsi.0 - u.gsi_base))
        .ok_or(RouteError::NoUnit(gsi))
}

/// Point `gsi` at `vector` on one CPU, fixed delivery, physical destination.
/// The entry is left masked: `set_masked(gsi, false)` is a separate step so a
/// driver can finish taking the device out of whatever state firmware left it
/// in before the first edge can arrive.
pub fn route(
    gsi: Gsi,
    vector: u8,
    dest_apic_id: u32,
    trigger: Trigger,
    polarity: Polarity,
) -> Result<(), RouteError> {
    if dest_apic_id >= 0xFF {
        return Err(RouteError::DestTooWide(dest_apic_id));
    }
    let topology = TOPOLOGY.lock();
    let (unit, n) = locate(&topology, gsi)?;
    let low = vector as u32
        | RTE_MASKED
        | if polarity == Polarity::Low { RTE_POLARITY_LOW } else { 0 }
        | if trigger == Trigger::Level { RTE_TRIGGER_LEVEL } else { 0 };
    // Destination first: the entry is masked either way, and writing the low
    // word last means it is never briefly armed at the previous destination.
    unit.write(REG_REDTBL + 2 * n + 1, dest_apic_id << 24);
    unit.write(REG_REDTBL + 2 * n, low);
    // Delivery status (12) and remote IRR (14) are the chip's, not ours.
    let read_low = unit.read(REG_REDTBL + 2 * n) & !(RTE_DELIVERY_STATUS | RTE_REMOTE_IRR);
    let read_dest = (unit.read(REG_REDTBL + 2 * n + 1) >> 24) & 0xFF;
    if read_low != low || read_dest != dest_apic_id {
        return Err(RouteError::Readback { wrote: low, read: read_low });
    }
    Ok(())
}

pub fn set_masked(gsi: Gsi, masked: bool) -> Result<(), RouteError> {
    let topology = TOPOLOGY.lock();
    let (unit, n) = locate(&topology, gsi)?;
    let index = REG_REDTBL + 2 * n;
    let low = unit.read(index);
    unit.write(index, if masked { low | RTE_MASKED } else { low & !RTE_MASKED });
    Ok(())
}

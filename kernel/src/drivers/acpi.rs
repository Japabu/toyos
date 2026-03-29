use alloc::vec::Vec;
use core::mem::size_of;
use core::ptr::{read_unaligned, read_volatile};
use core::sync::atomic::{AtomicU16, Ordering};
use crate::log;
use crate::DirectMap;

pub struct MadtInfo {
    pub apic_ids: Vec<u32>,
}

// ACPI table structures (all packed — tables are not guaranteed aligned)

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
struct MadtEntryHeader {
    entry_type: u8,
    length: u8,
}

#[repr(C, packed)]
struct MadtLocalApic {
    header: MadtEntryHeader,
    processor_id: u8,
    apic_id: u8,
    flags: u32,
}

#[repr(C, packed)]
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

const SLP_EN: u16 = 1 << 13;

static PM1A_CNT_PORT: AtomicU16 = AtomicU16::new(0);
static SLP_TYPA: AtomicU16 = AtomicU16::new(0);

/// Given the RSDP address from UEFI, parse XSDT -> MCFG -> return ECAM base address.
pub fn find_ecam_base(rsdp_addr: u64) -> Option<u64> {
    let rsdp = DirectMap::from_phys(rsdp_addr);
    log!("ACPI: RSDP at {:#x}", rsdp_addr);

    let xsdt = get_xsdt(rsdp);
    log!("ACPI: XSDT at {}", xsdt);

    let mcfg = find_table(xsdt, b"MCFG")?;
    log!("ACPI: MCFG found at {}", mcfg);

    let entry = unsafe { &*mcfg.as_ptr::<u8>().add(size_of::<SdtHeader>() + 8).cast::<McfgEntry>() };
    let ecam_base = entry.base_address;
    log!("ACPI: ECAM base address: {:#x}", ecam_base);

    Some(ecam_base)
}

/// Parse FADT and DSDT to prepare for ACPI shutdown.
pub fn init_power(rsdp_addr: u64) {
    let xsdt = get_xsdt(DirectMap::from_phys(rsdp_addr));

    let fadt_phys = find_table(xsdt, b"FACP").expect("ACPI: FADT not found");
    let fadt = unsafe { &*fadt_phys.as_ptr::<Fadt>() };

    let pm1a = fadt.pm1a_control_block as u16;
    PM1A_CNT_PORT.store(pm1a, Ordering::Relaxed);

    // Prefer X_DSDT (64-bit, ACPI 2.0+) over DSDT (32-bit)
    let dsdt_addr = if fadt.header.revision >= 2 && fadt.x_dsdt != 0 {
        fadt.x_dsdt
    } else {
        fadt.dsdt as u64
    };
    assert!(dsdt_addr != 0, "ACPI: DSDT not found");
    let dsdt_phys = DirectMap::from_phys(dsdt_addr);

    let dsdt = dsdt_phys.as_ptr::<SdtHeader>();
    let dsdt_len = unsafe { read_unaligned(&raw const (*dsdt).length) } as usize;

    let slp_typ = find_s5_slp_typ(dsdt_phys.as_ptr(), dsdt_len).expect("ACPI: \\_S5_ not found in DSDT");
    SLP_TYPA.store(slp_typ, Ordering::Relaxed);
    log!("ACPI: PM1a={:#x} SLP_TYPa={}", pm1a, slp_typ);
}

/// Trigger ACPI S5 (soft-off) shutdown.
pub fn shutdown() -> ! {
    let pm1a = PM1A_CNT_PORT.load(Ordering::Relaxed);
    let slp_typ = SLP_TYPA.load(Ordering::Relaxed);

    if pm1a != 0 {
        let val = (slp_typ << 10) | SLP_EN;
        crate::arch::cpu::outw(pm1a, val);
    }

    crate::arch::cpu::halt();
}

/// Read XSDT physical address from RSDP.
fn get_xsdt(rsdp_addr: DirectMap) -> DirectMap {
    let rsdp = unsafe { &*rsdp_addr.as_ptr::<Rsdp>() };
    DirectMap::from_phys(rsdp.xsdt_address)
}

/// Iterate XSDT entries looking for a table with the given 4-byte signature.
/// Returns the physical address of the matching table.
fn find_table(xsdt: DirectMap, signature: &[u8; 4]) -> Option<DirectMap> {
    let header = unsafe { &*xsdt.as_ptr::<SdtHeader>() };
    let length = header.length as usize;
    let entry_count = (length - size_of::<SdtHeader>()) / 8;

    let entries_base = unsafe { xsdt.as_ptr::<u8>().add(size_of::<SdtHeader>()) } as *const u64;

    for i in 0..entry_count {
        let table_phys = DirectMap::from_phys(unsafe { read_unaligned(entries_base.add(i)) });
        let table_header = unsafe { &*table_phys.as_ptr::<SdtHeader>() };

        if &table_header.signature == signature {
            return Some(table_phys);
        }
    }

    None
}

/// Given the RSDP address, parse XSDT -> HPET table -> return HPET MMIO base address.
pub fn find_hpet_base(rsdp_addr: u64) -> Option<u64> {
    let xsdt = get_xsdt(DirectMap::from_phys(rsdp_addr));
    let hpet_phys = find_table(xsdt, b"HPET")?;
    let hpet = unsafe { &*hpet_phys.as_ptr::<HpetTable>() };
    let base = hpet.base_address_value;
    log!("ACPI: HPET at {:#x}", base);
    Some(base)
}

/// Parse MADT (signature "APIC") to discover per-CPU APIC IDs.
pub fn parse_madt(rsdp_addr: u64) -> Option<MadtInfo> {
    let xsdt = get_xsdt(DirectMap::from_phys(rsdp_addr));
    let madt_phys = find_table(xsdt, b"APIC")?;
    let madt = unsafe { &*madt_phys.as_ptr::<Madt>() };

    let length = madt.header.length as usize;

    let mut apic_ids = Vec::new();
    let entries_base: *const u8 = unsafe { madt_phys.as_ptr::<u8>().add(size_of::<Madt>()) };
    let mut offset = 0usize;
    let entries_len = length - size_of::<Madt>();

    while offset + size_of::<MadtEntryHeader>() <= entries_len {
        let entry = unsafe { &*(entries_base.add(offset) as *const MadtEntryHeader) };
        let entry_len = entry.length as usize;
        if entry_len == 0 { break; }

        match entry.entry_type {
            // Type 0 = Processor Local APIC
            0 if entry_len >= size_of::<MadtLocalApic>() => {
                let lapic = unsafe { &*(entries_base.add(offset) as *const MadtLocalApic) };
                if lapic.flags & 1 != 0 {
                    apic_ids.push(lapic.apic_id as u32);
                }
            }
            // Type 9 = Processor Local x2APIC (32-bit APIC IDs)
            9 if entry_len >= size_of::<MadtLocalX2Apic>() => {
                let x2 = unsafe { &*(entries_base.add(offset) as *const MadtLocalX2Apic) };
                if x2.flags & 1 != 0 {
                    apic_ids.push(x2.x2apic_id);
                }
            }
            _ => {}
        }

        offset += entry_len;
    }

    log!("ACPI: MADT cpus={:?}", apic_ids);
    Some(MadtInfo { apic_ids })
}

/// Scan DSDT AML bytecode for the \_S5_ package and extract SLP_TYPa.
fn find_s5_slp_typ(dsdt: *const u8, len: usize) -> Option<u16> {
    let s5 = b"_S5_";

    for i in 0..len.saturating_sub(7) {
        // Match "_S5_"
        let mut found = true;
        for j in 0..4 {
            if unsafe { read_volatile(dsdt.add(i + j)) } != s5[j] {
                found = false;
                break;
            }
        }
        if !found { continue; }

        // Expect PackageOp (0x12) after the name
        if unsafe { read_volatile(dsdt.add(i + 4)) } != 0x12 {
            continue;
        }

        // Parse PkgLength to skip past it
        let pkg_lead = unsafe { read_volatile(dsdt.add(i + 5)) };
        let pkg_len_bytes = match (pkg_lead >> 6) & 0x03 {
            0 => 1usize,
            n => (n + 1) as usize,
        };

        // Skip: "_S5_"(4) + PackageOp(1) + PkgLength + NumElements(1)
        let val_off = i + 4 + 1 + pkg_len_bytes + 1;
        if val_off >= len { return None; }

        let byte = unsafe { read_volatile(dsdt.add(val_off)) };
        let slp_typ = if byte == 0x0A {
            // BytePrefix -- next byte is the value
            if val_off + 1 >= len { return None; }
            (unsafe { read_volatile(dsdt.add(val_off + 1)) }) as u16
        } else {
            // ZeroOp (0x00), OneOp (0x01), or raw value
            byte as u16
        };

        return Some(slp_typ);
    }

    None
}

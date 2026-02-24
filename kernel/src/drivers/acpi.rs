use alloc::vec::Vec;
use core::ptr::{read_unaligned, read_volatile};
use core::sync::atomic::{AtomicU16, Ordering};
use crate::log;

pub struct MadtInfo {
    pub local_apic_addr: u32,
    pub apic_ids: Vec<u8>,
}

// ACPI table structure offsets
const SDT_LENGTH: usize = 4;         // u32 table length at offset 4
const SDT_HEADER_SIZE: usize = 36;   // XSDT entries start after 36-byte header

// RSDP offsets
const RSDP_XSDT_ADDR: usize = 24;   // u64 XSDT physical address

// FADT (FACP) offsets
const FADT_REVISION: usize = 8;      // u8 revision
const FADT_DSDT: usize = 40;         // u32 DSDT physical address (ACPI 1.0)
const FADT_PM1A_CNT_BLK: usize = 64; // u32 PM1a control block port
const FADT_X_DSDT: usize = 140;      // u64 DSDT physical address (ACPI 2.0+)

// MCFG/HPET first entry base address (after 36B header + 8B reserved)
const MCFG_FIRST_ENTRY_BASE: usize = 44;
const HPET_BASE_ADDR: usize = 44;    // 36B header + 4B event timer block ID + 4B GAS header

const SLP_EN: u16 = 1 << 13;

static PM1A_CNT_PORT: AtomicU16 = AtomicU16::new(0);
static SLP_TYPA: AtomicU16 = AtomicU16::new(0);

/// Given the RSDP address from UEFI, parse XSDT -> MCFG -> return ECAM base address.
pub fn find_ecam_base(rsdp_addr: u64) -> Option<u64> {
    let rsdp = rsdp_addr as *const u8;
    log!("ACPI: RSDP at {:#x}", rsdp_addr);

    let xsdt = get_xsdt_addr(rsdp);
    log!("ACPI: XSDT at {:#x}", xsdt as u64);

    let mcfg = find_table(xsdt, b"MCFG")?;
    log!("ACPI: MCFG found at {:#x}", mcfg as u64);

    let ecam_base = unsafe { read_unaligned(mcfg.add(MCFG_FIRST_ENTRY_BASE) as *const u64) };
    log!("ACPI: ECAM base address: {:#x}", ecam_base);

    Some(ecam_base)
}

/// Parse FADT and DSDT to prepare for ACPI shutdown.
pub fn init_power(rsdp_addr: u64) {
    let rsdp = rsdp_addr as *const u8;
    let xsdt = get_xsdt_addr(rsdp);

    let fadt = find_table(xsdt, b"FACP").expect("ACPI: FADT not found");

    let pm1a = unsafe { read_unaligned(fadt.add(FADT_PM1A_CNT_BLK) as *const u32) } as u16;
    PM1A_CNT_PORT.store(pm1a, Ordering::Relaxed);

    // Prefer X_DSDT (64-bit, ACPI 2.0+) over DSDT (32-bit)
    let revision = unsafe { read_volatile(fadt.add(FADT_REVISION)) };
    let dsdt_addr = if revision >= 2 {
        let x_dsdt = unsafe { read_unaligned(fadt.add(FADT_X_DSDT) as *const u64) };
        if x_dsdt != 0 {
            x_dsdt
        } else {
            (unsafe { read_unaligned(fadt.add(FADT_DSDT) as *const u32) }) as u64
        }
    } else {
        (unsafe { read_unaligned(fadt.add(FADT_DSDT) as *const u32) }) as u64
    };

    assert!(dsdt_addr != 0, "ACPI: DSDT not found");

    let dsdt = dsdt_addr as *const u8;
    let dsdt_len = unsafe { read_unaligned(dsdt.add(SDT_LENGTH) as *const u32) } as usize;

    let slp_typ = find_s5_slp_typ(dsdt, dsdt_len).expect("ACPI: \\_S5_ not found in DSDT");
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

/// Read XSDT address from RSDP.
fn get_xsdt_addr(rsdp: *const u8) -> *const u8 {
    unsafe { read_unaligned(rsdp.add(RSDP_XSDT_ADDR) as *const u64) as *const u8 }
}

/// Iterate XSDT entries looking for a table with the given 4-byte signature.
fn find_table(xsdt: *const u8, signature: &[u8; 4]) -> Option<*const u8> {
    let length = unsafe { read_unaligned(xsdt.add(SDT_LENGTH) as *const u32) } as usize;
    let entry_count = (length - SDT_HEADER_SIZE) / 8;

    let entries_base = unsafe { xsdt.add(SDT_HEADER_SIZE) };

    for i in 0..entry_count {
        let table_addr =
            unsafe { read_unaligned(entries_base.add(i * 8) as *const u64) } as *const u8;

        let mut sig = [0u8; 4];
        for j in 0..4 {
            sig[j] = unsafe { read_volatile(table_addr.add(j)) };
        }

        if &sig == signature {
            return Some(table_addr);
        }
    }

    None
}

/// Given the RSDP address, parse XSDT -> HPET table -> return HPET MMIO base address.
pub fn find_hpet_base(rsdp_addr: u64) -> Option<u64> {
    let xsdt = get_xsdt_addr(rsdp_addr as *const u8);
    let hpet = find_table(xsdt, b"HPET")?;
    let base = unsafe { read_unaligned(hpet.add(HPET_BASE_ADDR) as *const u64) };
    log!("ACPI: HPET at {:#x}", base);
    Some(base)
}

/// Parse MADT (signature "APIC") to discover Local APIC address and per-CPU APIC IDs.
pub fn parse_madt(rsdp_addr: u64) -> Option<MadtInfo> {
    let xsdt = get_xsdt_addr(rsdp_addr as *const u8);
    let madt = find_table(xsdt, b"APIC")?;

    let length = unsafe { read_unaligned(madt.add(SDT_LENGTH) as *const u32) } as usize;
    let local_apic_addr = unsafe { read_unaligned(madt.add(36) as *const u32) };

    let mut apic_ids = Vec::new();
    let mut offset = 44; // variable-length entries start after 36B header + 4B LAPIC addr + 4B flags

    while offset + 1 < length {
        let entry_type = unsafe { read_volatile(madt.add(offset)) };
        let entry_len = unsafe { read_volatile(madt.add(offset + 1)) } as usize;
        if entry_len == 0 { break; }

        // Type 0 = Processor Local APIC (8 bytes: type, len, proc_id, apic_id, flags)
        if entry_type == 0 && entry_len >= 8 {
            let apic_id = unsafe { read_volatile(madt.add(offset + 3)) };
            let flags = unsafe { read_unaligned(madt.add(offset + 4) as *const u32) };
            // Bit 0: Processor Enabled, Bit 1: Online Capable
            if flags & 1 != 0 {
                apic_ids.push(apic_id);
            }
        }

        offset += entry_len;
    }

    log!("ACPI: MADT local_apic={:#x} cpus={:?}", local_apic_addr, apic_ids);
    Some(MadtInfo { local_apic_addr, apic_ids })
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

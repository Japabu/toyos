use core::ptr::{read_unaligned, read_volatile};
use core::sync::atomic::{AtomicU16, Ordering};
use crate::log;

static PM1A_CNT_PORT: AtomicU16 = AtomicU16::new(0);
static SLP_TYPA: AtomicU16 = AtomicU16::new(0);

/// Given the RSDP address from UEFI, parse XSDT → MCFG → return ECAM base address.
pub fn find_ecam_base(rsdp_addr: u64) -> Option<u64> {
    let rsdp = rsdp_addr as *const u8;
    log!("ACPI: RSDP at {:#x}", rsdp_addr);

    let xsdt = get_xsdt_addr(rsdp);
    log!("ACPI: XSDT at {:#x}", xsdt as u64);

    let mcfg = find_table(xsdt, b"MCFG")?;
    log!("ACPI: MCFG found at {:#x}", mcfg as u64);

    let ecam_base = read_ecam_base(mcfg);
    log!("ACPI: ECAM base address: {:#x}", ecam_base);

    Some(ecam_base)
}

/// Parse FADT and DSDT to prepare for ACPI shutdown.
pub fn init_power(rsdp_addr: u64) {
    let rsdp = rsdp_addr as *const u8;
    let xsdt = get_xsdt_addr(rsdp);

    let fadt = find_table(xsdt, b"FACP").expect("ACPI: FADT not found");

    // PM1a_CNT_BLK: FADT offset 64, 4 bytes
    let pm1a = unsafe { read_unaligned(fadt.add(64) as *const u32) } as u16;
    PM1A_CNT_PORT.store(pm1a, Ordering::Relaxed);

    // Get DSDT address — prefer X_DSDT (offset 140) over DSDT (offset 40)
    let revision = unsafe { read_volatile(fadt.add(8)) };
    let dsdt_addr = if revision >= 2 {
        let x_dsdt = unsafe { read_unaligned(fadt.add(140) as *const u64) };
        if x_dsdt != 0 {
            x_dsdt
        } else {
            (unsafe { read_unaligned(fadt.add(40) as *const u32) }) as u64
        }
    } else {
        (unsafe { read_unaligned(fadt.add(40) as *const u32) }) as u64
    };

    assert!(dsdt_addr != 0, "ACPI: DSDT not found");

    let dsdt = dsdt_addr as *const u8;
    let dsdt_len = unsafe { read_unaligned(dsdt.add(4) as *const u32) } as usize;

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

const SLP_EN: u16 = 1 << 13;

/// Read XSDT address from RSDP (offset 24, 8 bytes).
fn get_xsdt_addr(rsdp: *const u8) -> *const u8 {
    unsafe { read_unaligned(rsdp.add(24) as *const u64) as *const u8 }
}

/// Iterate XSDT entries looking for a table with the given 4-byte signature.
fn find_table(xsdt: *const u8, signature: &[u8; 4]) -> Option<*const u8> {
    let length = unsafe { read_unaligned(xsdt.add(4) as *const u32) } as usize;
    let header_size = 36;
    let entry_count = (length - header_size) / 8;

    let entries_base = unsafe { xsdt.add(header_size) };

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

/// Given the RSDP address, parse XSDT → HPET table → return HPET MMIO base address.
pub fn find_hpet_base(rsdp_addr: u64) -> Option<u64> {
    let xsdt = get_xsdt_addr(rsdp_addr as *const u8);
    let hpet = find_table(xsdt, b"HPET")?;
    // HPET table: 36B SDT header + 4B Event Timer Block ID + 4B GAS header + 8B address
    let base = unsafe { read_unaligned(hpet.add(44) as *const u64) };
    log!("ACPI: HPET at {:#x}", base);
    Some(base)
}

/// Read ECAM base address from the first MCFG configuration entry.
/// MCFG layout: 36-byte SDT header + 8 bytes reserved + 16-byte entries.
/// First entry base address is at offset 44.
fn read_ecam_base(mcfg: *const u8) -> u64 {
    unsafe { read_unaligned(mcfg.add(44) as *const u64) }
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
            // BytePrefix — next byte is the value
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

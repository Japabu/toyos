use core::ptr::{read_unaligned, read_volatile};
use crate::log;
use alloc::format;

const MCFG_SIGNATURE: &[u8; 4] = b"MCFG";

/// Given the RSDP address from UEFI, parse XSDT → MCFG → return ECAM base address.
pub fn find_ecam_base(rsdp_addr: u64) -> Option<u64> {
    let rsdp = rsdp_addr as *const u8;
    log::println(&format!("ACPI: RSDP at {:#x}", rsdp_addr));

    let xsdt = get_xsdt_addr(rsdp);
    log::println(&format!("ACPI: XSDT at {:#x}", xsdt as u64));

    let mcfg = find_mcfg(xsdt)?;
    log::println(&format!("ACPI: MCFG found at {:#x}", mcfg as u64));

    let ecam_base = read_ecam_base(mcfg);
    log::println(&format!("ACPI: ECAM base address: {:#x}", ecam_base));

    Some(ecam_base)
}

/// Read XSDT address from RSDP (offset 24, 8 bytes).
fn get_xsdt_addr(rsdp: *const u8) -> *const u8 {
    unsafe { read_unaligned(rsdp.add(24) as *const u64) as *const u8 }
}

/// Iterate XSDT entries looking for MCFG table signature.
fn find_mcfg(xsdt: *const u8) -> Option<*const u8> {
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

        if &sig == MCFG_SIGNATURE {
            return Some(table_addr);
        }
    }

    None
}

/// Read ECAM base address from the first MCFG configuration entry.
/// MCFG layout: 36-byte SDT header + 8 bytes reserved + 16-byte entries.
/// First entry base address is at offset 44.
fn read_ecam_base(mcfg: *const u8) -> u64 {
    unsafe { read_unaligned(mcfg.add(44) as *const u64) }
}

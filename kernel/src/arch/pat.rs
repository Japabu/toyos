//! The Page Attribute Table, and the one entry this kernel programs into it.
//!
//! The reset `IA32_PAT` offers WB, WT, UC- and UC and **no write-combining
//! entry at all**, so a kernel that never writes this MSR cannot ask for WC
//! however it sets a page's PAT, PCD and PWT bits. That is the whole reason
//! this file exists: the GOP scanout wants WC, and under the reset table the
//! request is unspeakable.
//!
//! It is also the only mechanism needed. SDM Vol. 3A Table 11-7 gives WC as
//! the effective type for a WC PAT entry under *every* MTRR type, UC included
//! — see [`mtrr::effective_under_wc`](crate::arch::mtrr::effective_under_wc) —
//! so no range register is programmed here and none needs to be.

use crate::arch::cpu;

const IA32_PAT: u32 = 0x277;

/// Architectural memory-type encodings, as a PAT entry holds them.
const UC: u8 = 0x00;
const WC: u8 = 0x01;
const WT: u8 = 0x04;
const WB: u8 = 0x06;
const UC_MINUS: u8 = 0x07;

/// The entry this kernel programs to WC, and the only one it changes.
///
/// Four rather than one of 0..=3, for two reasons that agree. Entries 0..=3
/// are the four types the architecture puts there at reset and 4..=7 repeat
/// them, so 4 is the first slot whose value nothing can already be selecting —
/// every page the kernel has mapped before this runs selects entry 0. And an
/// entry in 4..=7 is chosen by the PAT bit alone, with PCD and PWT clear, so
/// asking for WC costs a 2 MiB PDE exactly one bit and leaves the two bits
/// that mean "uncacheable" to older software untouched.
pub const WC_ENTRY: usize = 4;

/// `IA32_PAT` as this kernel programs it, one byte per entry.
const ENTRIES: [u8; 8] = [WB, WT, UC_MINUS, UC, WC, WT, UC_MINUS, UC];

const fn packed(entries: [u8; 8]) -> u64 {
    let mut value = 0u64;
    let mut i = 0;
    while i < 8 {
        value |= (entries[i] as u64) << (i * 8);
        i += 1;
    }
    value
}

const _: () = assert!(ENTRIES[WC_ENTRY] == WC);

/// Put [`ENTRIES`] in this CPU's `IA32_PAT`. Every CPU must run it, because
/// the table is per-logical-processor state and the page tables are not.
///
/// The sequence is SDM Vol. 3A §11.12.4, which sends a PAT change through
/// §11.11.8's MTRR-change procedure minus the two steps that clear and restore
/// `MTRRdefType.E`: no-fill cache mode, flush the caches, flush the TLBs,
/// write the MSR, flush both again, and only then let the caches fill.
///
/// **There is no cross-CPU rendezvous, and the reason is the entry chosen.**
/// §11.11.8 wants every processor inside the window at once because a stale
/// entry means a page whose memory type differs between two CPUs. Entry 4 is
/// unselected by every mapping in existence until the framebuffer is mapped,
/// which happens on the BSP after this has run on it, and on an AP before it
/// executes anything else — so the entry no CPU agrees about is one no page
/// names.
pub fn init() {
    let flags: u64;
    let cr0: u64;
    let cr4: u64;
    unsafe {
        core::arch::asm!("pushfq", "pop {}", "cli", out(reg) flags);
        core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        // CD set with NW clear is no-fill mode: hits still write through, and
        // nothing new enters the caches while the table is inconsistent.
        core::arch::asm!("mov cr0, {}", in(reg) (cr0 | CR0_CD) & !CR0_NW, options(nostack));
        core::arch::asm!("wbinvd", options(nostack, preserves_flags));
        flush_tlb(cr4);

        cpu::wrmsr(IA32_PAT, packed(ENTRIES));

        flush_tlb(cr4);
        core::arch::asm!("wbinvd", options(nostack, preserves_flags));
        core::arch::asm!("mov cr0, {}", in(reg) cr0, options(nostack));
    }

    if flags & RFLAGS_IF != 0 {
        cpu::enable_interrupts();
    }

    // A CPU that took the write but reports something else has broken the one
    // claim every WC mapping in the machine rests on, and it is cheaper to say
    // so here than to have a panel explain it later.
    let read_back = cpu::rdmsr(IA32_PAT);
    assert!(
        read_back == packed(ENTRIES),
        "PAT: wrote {:#018x}, IA32_PAT reads {read_back:#018x}",
        packed(ENTRIES)
    );
}

const CR0_NW: u64 = 1 << 29;
const CR0_CD: u64 = 1 << 30;
const CR4_PGE: u64 = 1 << 7;
const RFLAGS_IF: u64 = 1 << 9;

/// Flush every TLB entry including the global ones, which a plain CR3 reload
/// leaves alone (SDM Vol. 3A §4.10.4.1).
unsafe fn flush_tlb(cr4: u64) {
    if cr4 & CR4_PGE != 0 {
        core::arch::asm!("mov cr4, {}", in(reg) cr4 & !CR4_PGE, options(nostack));
        core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nostack));
    } else {
        let cr3: u64;
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
        core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack));
    }
}

/// The live `IA32_PAT`, for the one line that reports what this CPU has.
pub fn msr() -> u64 {
    cpu::rdmsr(IA32_PAT)
}

/// The type in PAT entry `index` on this CPU.
///
/// Its own decode rather than [`MemoryType`](crate::arch::mtrr::MemoryType)'s:
/// a PAT entry may hold UC-, which no MTRR can, and admitting that encoding to
/// the MTRR's type would mean ruling it out again in `range_type`.
pub fn entry_name(index: usize) -> &'static str {
    match (msr() >> (index * 8)) as u8 {
        UC => "UC",
        WC => "WC",
        WT => "WT",
        WB => "WB",
        UC_MINUS => "UC-",
        _ => "reserved",
    }
}

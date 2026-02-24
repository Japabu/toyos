use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use alloc::alloc::{alloc_zeroed, Layout};

use crate::arch::{apic, percpu, syscall};
use crate::clock;
use crate::drivers::acpi::MadtInfo;
use crate::log;

const TRAMPOLINE_PAGE: u64 = 0x8000;
const TRAMPOLINE_VECTOR: u8 = 0x08;
const AP_STACK_SIZE: usize = 64 * 1024;

// Data block at TRAMPOLINE_PAGE + 0xF00 = 0x8F00.
// Layout (offsets from 0x8F00):
//   +0x00: u64  CR3 (PML4 physical address)
//   +0x08: u64  stack top
//   +0x10: u64  Rust entry function pointer
//   +0x18: 10B  kernel GDT descriptor (u16 limit + u64 base)
//   +0x28: 10B  kernel IDT descriptor (u16 limit + u64 base)
//   +0x38: 6B   temp GDT descriptor (u16 limit + u32 base) for 16-bit lgdt
//   +0x40: 32B  temp GDT entries (null, code32, data, code64)
//   +0x60: 6B   PM32 far-jump target {u32 offset, u16 selector}
//   +0x68: 6B   LM64 far-jump target {u32 offset, u16 selector}
//   +0x70: u64  64-bit CS reload address (for retfq)
const DATA_OFFSET: usize = 0xF00;

static AP_STARTED: AtomicBool = AtomicBool::new(false);
static NEXT_CPU_ID: AtomicU32 = AtomicU32::new(1); // BSP is 0

extern "C" {
    static _trampoline_start: u8;
    static _trampoline_end: u8;
    static _ap_pm32: u8;
    static _ap_lm64: u8;
    static _ap_cs_reload: u8;
}

/// Boot all Application Processors found in the MADT.
pub fn boot_aps(madt: &MadtInfo) {
    let bsp_id = apic::id();

    // Copy trampoline blob to physical 0x8000
    let tramp_start = unsafe { &_trampoline_start as *const u8 };
    let tramp_end = unsafe { &_trampoline_end as *const u8 };
    let size = tramp_end as usize - tramp_start as usize;
    assert!(size <= DATA_OFFSET, "trampoline code exceeds data block offset");
    unsafe {
        core::ptr::copy_nonoverlapping(tramp_start, TRAMPOLINE_PAGE as *mut u8, size);
    }

    // Compute runtime addresses for trampoline labels
    let base = tramp_start as usize;
    let pm32_addr = 0x8000u32 + (unsafe { &_ap_pm32 as *const u8 } as usize - base) as u32;
    let lm64_addr = 0x8000u32 + (unsafe { &_ap_lm64 as *const u8 } as usize - base) as u32;
    let cs_reload_addr = 0x8000u64 + (unsafe { &_ap_cs_reload as *const u8 } as usize - base) as u64;

    // Fill data block
    let data = (TRAMPOLINE_PAGE + DATA_OFFSET as u64) as *mut u8;
    unsafe {
        // CR3
        let cr3: u64;
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
        core::ptr::write_unaligned(data as *mut u64, cr3);

        // Kernel GDT pointer (10 bytes)
        core::arch::asm!("sgdt [{}]", in(reg) data.add(0x18), options(nostack));
        // Kernel IDT pointer (10 bytes)
        core::arch::asm!("sidt [{}]", in(reg) data.add(0x28), options(nostack));

        // Temp GDT entries at +0x40
        let gdt = data.add(0x40) as *mut u64;
        gdt.add(0).write(0);                      // null
        gdt.add(1).write(0x00CF_9A00_0000_FFFF);  // code32
        gdt.add(2).write(0x00CF_9200_0000_FFFF);  // data
        gdt.add(3).write(0x00AF_9A00_0000_FFFF);  // code64

        // Temp GDT descriptor at +0x38 (u16 limit + u32 base)
        core::ptr::write_unaligned(data.add(0x38) as *mut u16, 4 * 8 - 1); // limit
        core::ptr::write_unaligned(data.add(0x3A) as *mut u32, 0x8F40);    // base

        // PM32 far-jump target at +0x60 {u32 offset, u16 selector}
        core::ptr::write_unaligned(data.add(0x60) as *mut u32, pm32_addr);
        core::ptr::write_unaligned(data.add(0x64) as *mut u16, 0x08);

        // LM64 far-jump target at +0x68 {u32 offset, u16 selector}
        core::ptr::write_unaligned(data.add(0x68) as *mut u32, lm64_addr);
        core::ptr::write_unaligned(data.add(0x6C) as *mut u16, 0x18);

        // CS reload address at +0x70
        core::ptr::write_unaligned(data.add(0x70) as *mut u64, cs_reload_addr);
    }

    for &ap_id in &madt.apic_ids {
        if ap_id == bsp_id {
            continue;
        }

        // Allocate per-AP stack
        let stack_layout = Layout::from_size_align(AP_STACK_SIZE, 4096).unwrap();
        let stack_base = unsafe { alloc_zeroed(stack_layout) };
        assert!(!stack_base.is_null(), "SMP: failed to allocate AP stack");
        let stack_top = stack_base as u64 + AP_STACK_SIZE as u64;

        // Write per-AP data
        unsafe {
            core::ptr::write_unaligned(data.add(0x08) as *mut u64, stack_top);
            core::ptr::write_unaligned(data.add(0x10) as *mut u64, ap_entry as u64);
        }

        AP_STARTED.store(false, Ordering::Release);
        log!("SMP: starting AP (LAPIC ID {})", ap_id);

        // INIT-SIPI-SIPI sequence
        apic::send_init(ap_id);
        delay_ms(10);

        apic::send_sipi(ap_id, TRAMPOLINE_VECTOR);
        delay_ms(1);

        if !AP_STARTED.load(Ordering::Acquire) {
            apic::send_sipi(ap_id, TRAMPOLINE_VECTOR);
            delay_ms(1);
        }

        if AP_STARTED.load(Ordering::Acquire) {
            log!("SMP: AP {} online", ap_id);
        } else {
            log!("SMP: AP {} failed to start!", ap_id);
        }
    }
}

extern "C" fn ap_entry() -> ! {
    let lapic_id = apic::id();
    let cpu_id = NEXT_CPU_ID.fetch_add(1, Ordering::Relaxed);

    // Set up per-CPU data, GDT, and GS base
    percpu::init_ap(cpu_id, lapic_id as u32);

    // Enable syscall/sysret MSRs for this CPU
    syscall::init();

    // Enable this CPU's local APIC
    apic::init_ap();

    log!("Hello from CPU {} (LAPIC ID {})", cpu_id, lapic_id);
    AP_STARTED.store(true, Ordering::Release);
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)); }
    }
}

fn delay_ms(ms: u64) {
    let start = clock::nanos_since_boot();
    while clock::nanos_since_boot() - start < ms * 1_000_000 {}
}

// AP trampoline: real mode → protected mode → long mode → Rust entry.
// Assembled as a blob in .text, copied to 0x8000 at runtime.
// All memory addresses reference the data block at 0x8F00 (hardcoded constants).
// No label arithmetic — BSP fills far-jump targets at runtime.
global_asm!(
    ".global _trampoline_start",
    ".global _trampoline_end",
    ".global _ap_pm32",
    ".global _ap_lm64",
    ".global _ap_cs_reload",
    "_trampoline_start:",

    // ==================== 16-bit real mode ====================
    ".code16",
    "cli",
    "xor ax, ax",
    "mov ds, ax",
    "mov es, ax",
    "mov ss, ax",

    // Load temp GDT descriptor from data block at 0x8F38
    "lgdt [0x8F38]",

    // Enable Protected Mode (CR0.PE)
    "mov eax, cr0",
    "or al, 1",
    "mov cr0, eax",

    // Indirect far jump to PM32 via data block at 0x8F60 {u32 offset, u16 selector}
    ".byte 0x66, 0xFF, 0x2E",  // data32 jmp far [disp16]
    ".word 0x8F60",

    // ==================== 32-bit protected mode ====================
    ".code32",
    "_ap_pm32:",
    "mov ax, 0x10",
    "mov ds, ax",
    "mov es, ax",
    "mov ss, ax",

    // Enable PAE (CR4.PAE)
    "mov eax, cr4",
    "or eax, 0x20",
    "mov cr4, eax",

    // Load PML4 from data block
    "mov eax, [0x8F00]",
    "mov cr3, eax",

    // Enable Long Mode (IA32_EFER.LME)
    "mov ecx, 0xC0000080",
    "rdmsr",
    "or eax, 0x100",
    "wrmsr",

    // Enable Paging (CR0.PG)
    "mov eax, cr0",
    "or eax, 0x80000000",
    "mov cr0, eax",

    // Indirect far jump to LM64 via data block at 0x8F68 {u32 offset, u16 selector}
    ".byte 0xFF, 0x2D",  // jmp far [disp32]
    ".long 0x8F68",

    // ==================== 64-bit long mode ====================
    ".code64",
    "_ap_lm64:",

    // Set up stack and load data block base
    "mov edi, 0x8F00",
    "mov rsp, [rdi + 0x08]",

    // Load kernel GDT and reload CS via retfq
    "lgdt [rdi + 0x18]",
    "push 0x08",
    "push qword ptr [rdi + 0x70]",
    ".byte 0x48, 0xCB",  // REX.W RETF

    "_ap_cs_reload:",
    "mov ax, 0x10",
    "mov ds, ax",
    "mov es, ax",
    "mov fs, ax",
    "mov gs, ax",
    "mov ss, ax",

    // Load kernel IDT
    "mov edi, 0x8F00",
    "lidt [rdi + 0x28]",

    // Call Rust entry
    "call qword ptr [rdi + 0x10]",
    "2: hlt",
    "jmp 2b",

    "_trampoline_end:",
    ".code64",
);

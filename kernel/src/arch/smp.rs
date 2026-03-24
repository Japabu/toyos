use core::arch::global_asm;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use alloc::alloc::{alloc_zeroed, Layout};

use crate::arch::{apic, cpu, percpu, syscall};
use crate::clock;
use crate::drivers::acpi::MadtInfo;
use crate::{log, process};

const TRAMPOLINE_PAGE: u64 = 0x8000;
const TRAMPOLINE_VECTOR: u8 = 0x08;
const AP_STACK_SIZE: usize = 64 * 1024;
const DATA_OFFSET: usize = 0xF00;

static AP_STARTED: AtomicBool = AtomicBool::new(false);
static SMP_READY: AtomicBool = AtomicBool::new(false);
static NEXT_CPU_ID: AtomicU32 = AtomicU32::new(1); // BSP is 0
static CPU_COUNT: AtomicU32 = AtomicU32::new(1); // BSP counts as 1

pub fn cpu_count() -> u32 {
    CPU_COUNT.load(Ordering::Relaxed)
}

/// Signal APs that the kernel is fully initialized and they can join the scheduler.
pub fn set_ready() {
    SMP_READY.store(true, Ordering::Release);
}

// ---- Trampoline data block layout ----
//
// Shared between BSP (Rust) and AP (assembly trampoline at 0x8000).
// Field offsets are hardcoded in the global_asm! below — the static
// assertion at the bottom guarantees the struct matches.

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct DescriptorTablePointer32 {
    limit: u16,
    base: u32,
}

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct FarPointer {
    offset: u32,
    selector: u16,
}

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct TrampolineData {
    cr3: u64,                                // +0x00
    stack_top: u64,                          // +0x08
    entry: u64,                              // +0x10
    kernel_gdt: DescriptorTablePointer,      // +0x18 (10 bytes)
    _pad1: [u8; 6],                          // +0x22
    kernel_idt: DescriptorTablePointer,      // +0x28 (10 bytes)
    _pad2: [u8; 6],                          // +0x32
    temp_gdt_ptr: DescriptorTablePointer32,  // +0x38 (6 bytes)
    _pad3: [u8; 2],                          // +0x3E
    temp_gdt: [u64; 4],                      // +0x40 (32 bytes)
    pm32_far: FarPointer,                    // +0x60 (6 bytes)
    _pad4: [u8; 2],                          // +0x66
    lm64_far: FarPointer,                    // +0x68 (6 bytes)
    _pad5: [u8; 2],                          // +0x6E
    cs_reload_addr: u64,                     // +0x70
    percpu_ptr: u64,                         // +0x78
}

const _: () = assert!(size_of::<TrampolineData>() == 0x80);

// ---- Trampoline blob (linked into .text, copied to 0x8000 at runtime) ----

// These are assembly labels — we must use inline asm to get their addresses
// directly, bypassing GOT/PLT stubs that the PIE linker generates for
// `extern "C" { static }` references (which resolve to stubs in wrong order).

extern "C" {
    static _trampoline_start: u8;
    static _trampoline_end: u8;
    static _ap_pm32: u8;
    static _ap_lm64: u8;
    static _ap_cs_reload: u8;
}

/// Get the address of an assembly label directly via LEA, bypassing GOT stubs.
macro_rules! asm_label_addr {
    ($label:ident) => {{
        let addr: usize;
        core::arch::asm!(
            "lea {}, [rip + {}]",
            out(reg) addr,
            sym $label,
            options(nostack, nomem),
        );
        addr as *const u8
    }};
}

/// Copy the trampoline assembly blob to physical page 0x8000.
/// Accesses via the kernel direct map (PHYS_OFFSET) since there's no identity map.
fn copy_trampoline() {
    let start = unsafe { asm_label_addr!(_trampoline_start) };
    let end = unsafe { asm_label_addr!(_trampoline_end) };
    let size = end as usize - start as usize;
    assert!(size <= DATA_OFFSET, "trampoline code exceeds data block");
    let dest = crate::DirectMap::from_phys(TRAMPOLINE_PAGE).as_mut_ptr::<u8>();
    unsafe {
        core::ptr::copy_nonoverlapping(start, dest, size);
    }
}

/// Compute the runtime physical address of a trampoline label.
fn label_addr(label: *const u8) -> u32 {
    let base = unsafe { asm_label_addr!(_trampoline_start) } as usize;
    0x8000u32 + (label as usize - base) as u32
}

/// Build the TrampolineData struct with all global (non-per-AP) fields filled.
fn build_trampoline_data() -> TrampolineData {
    let pm32_addr = label_addr(unsafe { asm_label_addr!(_ap_pm32) });
    let lm64_addr = label_addr(unsafe { asm_label_addr!(_ap_lm64) });
    let cs_reload_addr = label_addr(unsafe { asm_label_addr!(_ap_cs_reload) }) as u64;

    // Read kernel's current GDT and IDT descriptors
    let mut kernel_gdt = DescriptorTablePointer { limit: 0, base: 0 };
    let mut kernel_idt = DescriptorTablePointer { limit: 0, base: 0 };
    unsafe {
        core::arch::asm!("sgdt [{}]", in(reg) &mut kernel_gdt, options(nostack));
        core::arch::asm!("sidt [{}]", in(reg) &mut kernel_idt, options(nostack));
    }

    let data_base = (TRAMPOLINE_PAGE + DATA_OFFSET as u64) as u32;

    TrampolineData {
        cr3: 0, // filled by boot_aps with the boot PML4 (has identity + high-half)
        stack_top: 0, // filled per-AP
        entry: 0,     // filled per-AP
        kernel_gdt,
        _pad1: [0; 6],
        kernel_idt,
        _pad2: [0; 6],
        temp_gdt_ptr: DescriptorTablePointer32 {
            limit: 4 * 8 - 1,
            base: data_base + 0x40, // points to temp_gdt field
        },
        _pad3: [0; 2],
        temp_gdt: [
            0x0000_0000_0000_0000, // null
            0x00CF_9A00_0000_FFFF, // code32
            0x00CF_9200_0000_FFFF, // data
            0x00AF_9A00_0000_FFFF, // code64
        ],
        pm32_far: FarPointer { offset: pm32_addr, selector: 0x08 },
        _pad4: [0; 2],
        lm64_far: FarPointer { offset: lm64_addr, selector: 0x18 },
        _pad5: [0; 2],
        cs_reload_addr,
        percpu_ptr: 0, // filled per-AP
    }
}

// ---- AP boot ----

/// Boot all Application Processors found in the MADT.
/// `boot_cr3` is the physical address of the bootloader's PML4 (has both
/// identity map and high-half). APs use this during their transition to
/// long mode, then switch to the kernel PML4 in `ap_entry`.
pub fn boot_aps(madt: &MadtInfo, boot_cr3: u64) {
    let bsp_id = apic::id();
    copy_trampoline();

    let mut data = build_trampoline_data();
    data.cr3 = boot_cr3;
    let target = crate::DirectMap::from_phys(TRAMPOLINE_PAGE + DATA_OFFSET as u64).as_mut_ptr::<TrampolineData>();

    let mut next_cpu_id = 1u32; // BSP is 0
    for &ap_id in &madt.apic_ids {
        if ap_id == bsp_id { continue; }

        let stack_layout = Layout::from_size_align(AP_STACK_SIZE, 4096).unwrap();
        let stack_base = unsafe { alloc_zeroed(stack_layout) };
        assert!(!stack_base.is_null(), "SMP: failed to allocate AP stack");

        let ap_cpu_id = next_cpu_id;
        next_cpu_id += 1;
        let ap_percpu = percpu::alloc_ap(ap_cpu_id, ap_id as u32);

        data.stack_top = stack_base as u64 + AP_STACK_SIZE as u64;
        data.entry = ap_entry as *const () as u64;
        data.percpu_ptr = ap_percpu as u64;
        unsafe { core::ptr::write_unaligned(target, data); }

        AP_STARTED.store(false, Ordering::Release);
        log!("SMP: starting AP (LAPIC ID {})", ap_id);

        // INIT-SIPI-SIPI sequence
        apic::send_init(ap_id);
        delay_ms(10);

        apic::send_sipi(ap_id, TRAMPOLINE_VECTOR);
        delay_ms(1);

        if !AP_STARTED.load(Ordering::Acquire) {
            apic::send_sipi(ap_id, TRAMPOLINE_VECTOR);
        }

        // Wait up to 100ms for AP to complete initialization
        let deadline = clock::nanos_since_boot() + 100_000_000;
        while !AP_STARTED.load(Ordering::Acquire) {
            if clock::nanos_since_boot() >= deadline { break; }
            core::hint::spin_loop();
        }

        if AP_STARTED.load(Ordering::Acquire) {
            CPU_COUNT.fetch_add(1, Ordering::Relaxed);
            log!("SMP: AP {} online", ap_id);
        } else {
            log!("SMP: AP {} failed to start!", ap_id);
        }
    }
}

extern "C" fn ap_entry() -> ! {
    // Switch from boot PML4 (identity + high-half) to kernel PML4 (high-half only).
    // We're already executing at a high-half address, so this is safe.
    unsafe { cpu::write_cr3(crate::mm::paging::kernel().lock().as_ref().unwrap().cr3()); }

    // GS base was set by the trampoline; finish percpu init (GDT, SSE, SMAP).
    percpu::init_ap(percpu::percpu_ptr());
    syscall::init();
    apic::init_ap();
    apic::init_timer_ap();

    log!("Hello from CPU {} (LAPIC ID {})", percpu::cpu_id(), apic::id());
    AP_STARTED.store(true, Ordering::Release);

    // Wait for BSP to finish kernel init
    while !SMP_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }

    log!("CPU {}: joining scheduler", percpu::cpu_id());
    process::ap_idle();
}

fn delay_ms(ms: u64) {
    let start = clock::nanos_since_boot();
    while clock::nanos_since_boot() - start < ms * 1_000_000 {}
}

// ---- AP trampoline assembly ----
//
// Real mode → protected mode → long mode → Rust entry.
// Assembled as a blob in .text, copied to 0x8000 at runtime.
// All memory addresses reference TrampolineData at 0x8F00.
// BSP fills far-jump targets and other fields at runtime.
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

    // Load temp GDT descriptor from TrampolineData.temp_gdt_ptr (+0x38)
    "lgdt [0x8F38]",

    // Enable Protected Mode (CR0.PE)
    "mov eax, cr0",
    "or al, 1",
    "mov cr0, eax",

    // Far jump to PM32 via TrampolineData.pm32_far (+0x60)
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

    // Load PML4 from TrampolineData.cr3 (+0x00)
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

    // Far jump to LM64 via TrampolineData.lm64_far (+0x68)
    ".byte 0xFF, 0x2D",  // jmp far [disp32]
    ".long 0x8F68",

    // ==================== 64-bit long mode ====================
    ".code64",
    "_ap_lm64:",

    // Load TrampolineData base and set up stack
    "mov edi, 0x8F00",
    "mov rsp, [rdi + 0x08]",   // TrampolineData.stack_top

    // Load kernel GDT and reload CS via retfq
    "lgdt [rdi + 0x18]",       // TrampolineData.kernel_gdt
    "push 0x08",
    "push qword ptr [rdi + 0x70]", // TrampolineData.cs_reload_addr
    ".byte 0x48, 0xCB",        // REX.W RETF

    "_ap_cs_reload:",
    "mov ax, 0x10",
    "mov ds, ax",
    "mov es, ax",
    "mov fs, ax",
    "mov gs, ax",
    "mov ss, ax",

    // Set GS base to percpu pointer (IA32_GS_BASE MSR 0xC0000101)
    // Must happen before IDT load so page fault handlers can access percpu.
    "mov edi, 0x8F00",
    "mov rax, [rdi + 0x78]",  // TrampolineData.percpu_ptr
    "mov rdx, rax",
    "shr rdx, 32",
    "mov ecx, 0xC0000101",    // IA32_GS_BASE
    "wrmsr",

    // Load kernel IDT (safe now — percpu/GS is set up)
    "mov edi, 0x8F00",
    "lidt [rdi + 0x28]",       // TrampolineData.kernel_idt

    // Call Rust entry
    "call qword ptr [rdi + 0x10]", // TrampolineData.entry
    "2: hlt",
    "jmp 2b",

    "_trampoline_end:",
    ".code64",
);

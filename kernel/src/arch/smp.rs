use core::arch::global_asm;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use alloc::alloc::{alloc_zeroed, Layout};

use crate::arch::{apic, percpu, syscall};
use crate::clock;
use crate::drivers::acpi::MadtInfo;
use crate::time::{Budget, Delay, Duration};
use crate::{log, process};

const TRAMPOLINE_PAGE: u64 = 0x8000;
const TRAMPOLINE_VECTOR: u8 = 0x08;
const AP_STACK_SIZE: usize = 64 * 1024;
const DATA_OFFSET: usize = 0xF00;

static AP_STARTED: AtomicBool = AtomicBool::new(false);
static SMP_READY: AtomicBool = AtomicBool::new(false);
static CPU_COUNT: AtomicU32 = AtomicU32::new(1); // BSP counts as 1

/// cpu_id → LAPIC id, filled during AP bring-up. Needed for targeted IPIs
/// (`apic::kick_cpu`) — cpu ids are kernel-assigned and dense, LAPIC ids
/// come from the MADT and need not be.
static CPU_APIC_IDS: [AtomicU32; crate::scheduler::MAX_CPUS] =
    [const { AtomicU32::new(0) }; crate::scheduler::MAX_CPUS];

pub fn cpu_count() -> u32 {
    CPU_COUNT.load(Ordering::Relaxed)
}

/// LAPIC id of `cpu_id`. Panics on out-of-range ids — callers must only
/// target CPUs that exist.
pub fn apic_id_for(cpu_id: u32) -> u32 {
    assert!(cpu_id < cpu_count(), "apic_id_for: cpu {cpu_id} not online");
    CPU_APIC_IDS[cpu_id as usize].load(Ordering::Relaxed)
}

/// Signal APs that the kernel is fully initialized and they can join the scheduler.
///
/// Also the point from which a TLB shootdown waits for acknowledgements. Until
/// here every AP that `CPU_COUNT` has counted is parked in the spin below with
/// `IF` clear and cannot take the IPI, so waiting for one would hang; the flush
/// each of them does on release is what settles the shootdowns issued in the
/// meantime (`arch::tlb::join`).
pub fn set_ready() {
    SMP_READY.store(true, Ordering::Release);
    crate::arch::tlb::siblings_answer();
}

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

// Trampoline blob (linked into .text, copied to 0x8000 at runtime)

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

/// The address of an assembly label, straight out of a `lea`.
///
/// **Safe, and the `unsafe` is inside**: a `lea` off `rip` computes an address
/// and reads nothing, so there is no caller obligation to state — the six sites
/// that spelled `unsafe { asm_label_addr!(…) }` were each restating the same
/// nothing. What the address then *means* is the caller's, and every caller here
/// treats it as an integer.
///
/// A macro rather than a function because `sym` needs the label as a token, and
/// inline asm rather than `&raw const $label` because the PIE linker resolves an
/// `extern "C" { static }` reference through a GOT stub, which for these labels
/// is filled in the wrong order.
macro_rules! asm_label_addr {
    ($label:ident) => {{
        let addr: usize;
        // SAFETY: `lea` with `nomem` reads and writes no memory; the operand is
        // an assembler symbol, so the only thing that can go wrong is the name
        // not existing, which is a link error rather than undefined behaviour.
        unsafe {
            core::arch::asm!(
                "lea {}, [rip + {}]",
                out(reg) addr,
                sym $label,
                options(nostack, nomem),
            );
        }
        addr as *const u8
    }};
}

/// Copy the trampoline assembly blob to physical page 0x8000.
/// Accesses via the kernel direct map (PHYS_OFFSET) since there's no identity map.
fn copy_trampoline() {
    let start = asm_label_addr!(_trampoline_start);
    let end = asm_label_addr!(_trampoline_end);
    let size = end as usize - start as usize;
    assert!(size <= DATA_OFFSET, "trampoline code exceeds data block");
    let dest = crate::DirectMap::from_phys(TRAMPOLINE_PAGE).as_mut_ptr::<u8>();
    // SAFETY: `start..end` is the `global_asm!` blob below, `'static` bytes of
    // this kernel's own `.text`. `dest` is the direct-map address of physical
    // page 0x8000, which `mm` reserves for exactly this and which no allocator
    // hands out; `size <= DATA_OFFSET` was just asserted, so the copy stays
    // clear of the `TrampolineData` block at 0x8F00 and inside the page. The two
    // ranges are kernel text and low physical memory, so they cannot overlap.
    unsafe {
        core::ptr::copy_nonoverlapping(start, dest, size);
    }
}

/// Compute the runtime physical address of a trampoline label.
fn label_addr(label: *const u8) -> u32 {
    let base = asm_label_addr!(_trampoline_start) as usize;
    0x8000u32 + (label as usize - base) as u32
}

/// Build the TrampolineData struct with all global (non-per-AP) fields filled.
fn build_trampoline_data() -> TrampolineData {
    let pm32_addr = label_addr(asm_label_addr!(_ap_pm32));
    let lm64_addr = label_addr(asm_label_addr!(_ap_lm64));
    let cs_reload_addr = label_addr(asm_label_addr!(_ap_cs_reload)) as u64;

    // Read kernel's current GDT and IDT descriptors
    let mut kernel_gdt = DescriptorTablePointer { limit: 0, base: 0 };
    let mut kernel_idt = DescriptorTablePointer { limit: 0, base: 0 };
    // SAFETY: both instructions write ten bytes to the address they are given,
    // and each is given a `&mut` to a live `DescriptorTablePointer` — `repr(C,
    // packed)` over a `u16` and a `u64`, which is that shape and that size.
    // Irreducible: `sgdt`/`sidt` are the only way to ask what this CPU loaded,
    // and they answer into memory rather than into a register.
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

/// Boot all Application Processors found in the MADT.
/// `boot_cr3` is the physical address of the bootloader's PML4 (has both
/// identity map and high-half). APs use this during their transition to
/// long mode, then switch to the kernel PML4 in `ap_entry`.
pub fn boot_aps(madt: &MadtInfo, boot_cr3: u64) {
    let bsp_id = apic::id();
    CPU_APIC_IDS[0].store(bsp_id, Ordering::Relaxed);
    copy_trampoline();

    let mut data = build_trampoline_data();
    data.cr3 = boot_cr3;
    let target = crate::DirectMap::from_phys(TRAMPOLINE_PAGE + DATA_OFFSET as u64).as_mut_ptr::<TrampolineData>();

    let mut next_cpu_id = 1u32; // BSP is 0
    for &ap_id in &madt.apic_ids {
        if ap_id == bsp_id { continue; }

        let stack_layout = Layout::from_size_align(AP_STACK_SIZE, 4096).unwrap();
        // SAFETY: `AP_STACK_SIZE` is non-zero and 4096 is a power of two, which
        // is `alloc_zeroed`'s whole contract. Irreducible for the reason
        // `percpu::alloc_percpu`'s is: the block is never freed and becomes an
        // AP's `rsp`, so no owning handle can hold it — a `Box` dropped here
        // would free the stack a CPU is running on, and no `Vec<u8>` expresses
        // the page alignment the trampoline's stack needs.
        let stack_base = unsafe { alloc_zeroed(stack_layout) };
        assert!(!stack_base.is_null(), "SMP: failed to allocate AP stack");

        let ap_cpu_id = next_cpu_id;
        next_cpu_id += 1;
        CPU_APIC_IDS[ap_cpu_id as usize].store(ap_id, Ordering::Relaxed);
        let ap_percpu = percpu::alloc_ap(ap_cpu_id, ap_id);

        data.stack_top = stack_base as u64 + AP_STACK_SIZE as u64;
        data.entry = ap_entry as *const () as u64;
        data.percpu_ptr = ap_percpu as u64;
        // SAFETY: `target` is the direct-map address of physical 0x8F00, the
        // 0x80-byte `TrampolineData` block `copy_trampoline`'s assertion keeps
        // clear of the blob — reserved low memory no allocator hands out, and
        // written only here. Unaligned because `TrampolineData` is `repr(C,
        // packed)`. The AP this describes has not been sent its INIT-SIPI yet,
        // and `boot_aps` waits for `AP_STARTED` before writing the block again,
        // so no CPU is reading it while this lands.
        unsafe { core::ptr::write_unaligned(target, data); }

        AP_STARTED.store(false, Ordering::Release);

        // INIT-SIPI-SIPI sequence. Both delays are spent rather than waited
        // on: nothing is polled across either and neither can fail.
        const AFTER_INIT: Delay =
            Delay::from_spec(Duration::from_millis(10), "SDM §8.4.4.1, after the INIT IPI");
        /// SDM §8.4.4.1 asks 200us here; a millisecond is the same shape with
        /// room, and it is paid once per AP at boot.
        const BETWEEN_SIPIS: Delay =
            Delay::from_spec(Duration::from_millis(1), "SDM §8.4.4.1, between the two SIPIs");
        apic::send_init(ap_id);
        delay(AFTER_INIT);

        apic::send_sipi(ap_id, TRAMPOLINE_VECTOR);
        delay(BETWEEN_SIPIS);

        if !AP_STARTED.load(Ordering::Acquire) {
            apic::send_sipi(ap_id, TRAMPOLINE_VECTOR);
        }

        // How long an AP gets to reach `ap_entry` before it is declared
        // absent by name and the machine boots without it.
        //
        // **A [`Budget`] and not the `Tripwire` offered as the alternative to
        // finding a source.** Its expiry is already a degraded
        // answer that says so — "failed to start!", one fewer CPU, a machine
        // that boots — and making it a panic would be a behaviour change,
        // which C1's own gate forbids. The number itself still has no source:
        // SDM §8.4.4.1's numbers are the two delays above, not this.
        const AP_START: Budget = Budget::of(
            Duration::from_millis(100),
            "the AP is named as failed to start and the machine boots one CPU short",
        );
        let deadline = clock::nanos_since_boot() + AP_START.nanos();
        while !AP_STARTED.load(Ordering::Acquire) {
            if clock::nanos_since_boot() >= deadline { break; }
            core::hint::spin_loop();
        }

        // One line per AP, carrying both halves of the identity: the cpu_id is
        // assigned here and appears in every later log prefix, the lapic_id is
        // what the INIT-SIPI-SIPI went to, and nothing else prints the pairing.
        // It is what lets the three lines this replaced go — `starting AP`,
        // `percpu: AP`, and the AP's own `Hello from CPU` each restated a
        // subset of it, four lines per core on a machine with eight.
        if AP_STARTED.load(Ordering::Acquire) {
            CPU_COUNT.fetch_add(1, Ordering::Relaxed);
            log!("SMP: AP cpu{} lapic={} online", ap_cpu_id, ap_id);
        } else {
            log!("SMP: AP cpu{} lapic={} failed to start!", ap_cpu_id, ap_id);
        }
    }
}

extern "C" fn ap_entry() -> ! {
    // The trampoline reaches long mode by OR-ing two bits into whatever INIT
    // left in CR0, so this is the first instruction that gives this CPU the
    // machine configuration the rest of the kernel is written against — and it
    // is before `pat::init`, which restores the CR0 it found.
    crate::arch::control_regs::init_cr0(percpu::cpu_id());

    // Before this CPU can reach a page that selects the entry it writes: the
    // framebuffer is already mapped write-combining by now.
    crate::arch::pat::init();

    // Switch from boot PML4 (identity + high-half) to kernel PML4 (high-half only).
    // We're already executing at a high-half address, so this is safe — which is
    // what `load_kernel_flush` says once instead of here.
    crate::mm::paging::load_kernel_flush();

    // GS base was set by the trampoline; finish percpu init (GDT, CR4).
    percpu::init_ap(percpu::percpu_ptr());
    syscall::init();
    // Calibration is one global measurement done on the BSP; `arm_one_shot`
    // programs divide and LVT per call, so there is nothing left for an AP to
    // do here.
    apic::init_ap();

    AP_STARTED.store(true, Ordering::Release);

    // Wait for BSP to finish kernel init
    while !SMP_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }

    // Before this CPU touches anything it did not map itself. The BSP mapped
    // every driver's registers and re-typed the framebuffer's leaf while this
    // one was parked above with `IF` clear, so no shootdown could reach it; the
    // acquire on `SMP_READY` makes those writes visible and this flush is what
    // discards whatever the spin left cached over them.
    crate::arch::tlb::join();

    log!("CPU {}: joining scheduler", percpu::cpu_id());
    process::ap_idle();
}

/// Spend a [`Delay`]. Boot only: there is no scheduler to give the CPU back
/// to, which is what the enclosing function's own presence in §4.4's
/// allow-list records.
fn delay(span: Delay) {
    let start = clock::nanos_since_boot();
    while clock::nanos_since_boot() - start < span.nanos() {}
}

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

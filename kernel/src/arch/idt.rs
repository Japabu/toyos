use core::arch::naked_asm;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::cpu;
use super::cpu::{outb, io_wait};
use crate::arch::{paging, syscall, percpu};
use crate::{process, log};

use crate::sync::Lock;

static CPU_BUSY_TICKS: AtomicU64 = AtomicU64::new(0);
static CPU_TOTAL_TICKS: AtomicU64 = AtomicU64::new(0);

pub fn cpu_ticks() -> (u64, u64) {
    (CPU_BUSY_TICKS.load(Ordering::Relaxed), CPU_TOTAL_TICKS.load(Ordering::Relaxed))
}

/// Atomically check and clear the xHCI interrupt pending flag.
pub fn xhci_irq_pending() -> bool {
    XHCI_IRQ_PENDING.swap(0, Ordering::Acquire) != 0
}

#[no_mangle]
static XHCI_IRQ_PENDING: AtomicU32 = AtomicU32::new(0);

// PIC ports
const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// IDT vector assignments — CPU exceptions and hardware interrupts.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Vector {
    InvalidOpcode = 0x06,
    DoubleFault = 0x08,
    GeneralProtection = 0x0D,
    PageFault = 0x0E,
    Timer = 0x20,
    Xhci = 0x21,
    TlbFlush = 0xFE,
}

impl Vector {
    fn from_raw(v: u64) -> Self {
        match v {
            0x06 => Self::InvalidOpcode,
            0x08 => Self::DoubleFault,
            0x0D => Self::GeneralProtection,
            0x0E => Self::PageFault,
            _ => panic!("unhandled exception vector {:#x}", v),
        }
    }
}

// Page fault error code bits
const PF_PRESENT: u64 = 1 << 0;
const PF_WRITE: u64 = 1 << 1;
const PF_INSTRUCTION_FETCH: u64 = 1 << 4;

// CS ring mask
const RPL_MASK: u64 = 3;

// IDT entry (16 bytes in 64-bit mode)
#[repr(C)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const EMPTY: Self = Self {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_high: 0,
        reserved: 0,
    };

    fn new(handler: u64) -> Self {
        Self {
            offset_low: handler as u16,
            selector: 0x08, // kernel CS
            ist: 0,
            type_attr: 0x8E, // interrupt gate, DPL=0, present
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }

    fn with_ist(mut self, ist_index: u8) -> Self {
        self.ist = ist_index;
        self
    }
}

#[repr(C, align(16))]
struct Idt {
    entries: [IdtEntry; 256],
}

static IDT: Lock<Idt> = Lock::new(Idt {
    entries: [IdtEntry::EMPTY; 256],
});

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

/// GPRs saved by exception/interrupt entry stubs.
/// Push order: rax, rbx, rcx, rdx, rsi, rdi, rbp, r8..r15 (lowest address first).
#[repr(C)]
pub struct SavedRegs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}


/// CPU-pushed interrupt/exception frame, sitting above the saved GPRs + error code.
#[repr(C)]
pub struct InterruptFrame {
    error_code: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

impl SavedRegs {
    fn interrupt_frame(&self) -> &InterruptFrame {
        unsafe { &*((self as *const SavedRegs).add(1) as *const InterruptFrame) }
    }
}

/// Disable the legacy 8259 PIC.
fn disable_pic() {
    outb(PIC1_CMD, 0x11);
    io_wait();
    outb(PIC2_CMD, 0x11);
    io_wait();

    outb(PIC1_DATA, 32);
    io_wait();
    outb(PIC2_DATA, 40);
    io_wait();

    outb(PIC1_DATA, 4);
    io_wait();
    outb(PIC2_DATA, 2);
    io_wait();

    outb(PIC1_DATA, 0x01);
    io_wait();
    outb(PIC2_DATA, 0x01);
    io_wait();

    outb(PIC1_DATA, 0xFF);
    outb(PIC2_DATA, 0xFF);
}

pub fn init() {
    disable_pic();

    {
        let mut idt = IDT.lock();
        idt.entries[Vector::InvalidOpcode as usize] = IdtEntry::new(ud_entry as *const () as u64);
        idt.entries[Vector::DoubleFault as usize] = IdtEntry::new(double_fault_entry as *const () as u64).with_ist(1);
        idt.entries[Vector::GeneralProtection as usize] = IdtEntry::new(gpf_entry as *const () as u64);
        idt.entries[Vector::PageFault as usize] = IdtEntry::new(page_fault_entry as *const () as u64);
        idt.entries[Vector::Timer as usize] = IdtEntry::new(timer_entry as *const () as u64);
        idt.entries[Vector::Xhci as usize] = IdtEntry::new(xhci_entry as *const () as u64);
        idt.entries[Vector::TlbFlush as usize] = IdtEntry::new(tlb_flush_entry as *const () as u64);
    }

    let ptr = IdtPointer {
        limit: (core::mem::size_of::<Idt>() - 1) as u16,
        base: IDT.data_ptr() as u64,
    };

    unsafe {
        cpu::lidt(&ptr as *const IdtPointer as *const u8);
        cpu::enable_interrupts();
    }
}

// --- Minimal asm stubs ---
//
// Each stub does only what hardware requires in asm:
//   1. Ring check + swapgs (user ↔ kernel GS base)
//   2. Save/restore GPRs
//   3. Call a single Rust handler with a pointer to the saved state
//   4. iretq
//
// All logic, argument reading, and branching lives in Rust.

// --- xHCI MSI-X handler (vector 0x21) ---
// Minimal: set atomic flag + EOI. No Rust call needed.
#[unsafe(naked)]
extern "sysv64" fn xhci_entry() {
    naked_asm!(
        "push rax",
        "push rdx",
        "lock bts dword ptr [rip + {flag}], 0",
        "mov rdx, [rip + LAPIC_BASE]",
        "mov dword ptr [rdx + 0xB0], 0",
        "pop rdx",
        "pop rax",
        "iretq",
        flag = sym XHCI_IRQ_PENDING,
    );
}

// --- TLB flush IPI handler (vector 0xFE) ---
#[unsafe(naked)]
extern "sysv64" fn tlb_flush_entry() {
    naked_asm!(
        "push rax",
        "push rdx",
        "mov rax, cr3",
        "mov cr3, rax",
        "mov rdx, [rip + LAPIC_BASE]",
        "mov dword ptr [rdx + 0xB0], 0",
        "pop rdx",
        "pop rax",
        "iretq",
    );
}

// --- Timer interrupt (vector 0x20) ---
// Ring 3: save all registers (GPR + SSE/XMM), call Rust handler (preempt), restore, iretq.
// Ring 0: just EOI and return (no preemption while kernel code runs).
#[unsafe(naked)]
extern "sysv64" fn timer_entry() {
    naked_asm!(
        // No error code for interrupts. CS is at [rsp + 8].
        "test dword ptr [rsp + 8], 3",
        "jz 2f",

        // Ring 3: preempt — save GPRs
        "swapgs",
        "push 0", // dummy error code for stack layout consistency
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",

        // Save SSE state (XMM0-XMM15 + MXCSR) — must happen before any Rust
        // code runs, since XMM registers are caller-saved in the System V ABI.
        "sub rsp, 8",           // MXCSR (4 bytes, padded to 8 for alignment)
        "stmxcsr [rsp]",
        "sub rsp, 256",         // 16 × 16 bytes for XMM0-XMM15
        "movdqu [rsp + 0*16], xmm0",
        "movdqu [rsp + 1*16], xmm1",
        "movdqu [rsp + 2*16], xmm2",
        "movdqu [rsp + 3*16], xmm3",
        "movdqu [rsp + 4*16], xmm4",
        "movdqu [rsp + 5*16], xmm5",
        "movdqu [rsp + 6*16], xmm6",
        "movdqu [rsp + 7*16], xmm7",
        "movdqu [rsp + 8*16], xmm8",
        "movdqu [rsp + 9*16], xmm9",
        "movdqu [rsp + 10*16], xmm10",
        "movdqu [rsp + 11*16], xmm11",
        "movdqu [rsp + 12*16], xmm12",
        "movdqu [rsp + 13*16], xmm13",
        "movdqu [rsp + 14*16], xmm14",
        "movdqu [rsp + 15*16], xmm15",

        "call {handler}",

        // Restore SSE state
        "movdqu xmm0,  [rsp + 0*16]",
        "movdqu xmm1,  [rsp + 1*16]",
        "movdqu xmm2,  [rsp + 2*16]",
        "movdqu xmm3,  [rsp + 3*16]",
        "movdqu xmm4,  [rsp + 4*16]",
        "movdqu xmm5,  [rsp + 5*16]",
        "movdqu xmm6,  [rsp + 6*16]",
        "movdqu xmm7,  [rsp + 7*16]",
        "movdqu xmm8,  [rsp + 8*16]",
        "movdqu xmm9,  [rsp + 9*16]",
        "movdqu xmm10, [rsp + 10*16]",
        "movdqu xmm11, [rsp + 11*16]",
        "movdqu xmm12, [rsp + 12*16]",
        "movdqu xmm13, [rsp + 13*16]",
        "movdqu xmm14, [rsp + 14*16]",
        "movdqu xmm15, [rsp + 15*16]",
        "add rsp, 256",
        "ldmxcsr [rsp]",
        "add rsp, 8",

        // Restore GPRs
        "pop rax",  "pop rbx",  "pop rcx",  "pop rdx",
        "pop rsi",  "pop rdi",  "pop rbp",
        "pop r8",   "pop r9",   "pop r10",  "pop r11",
        "pop r12",  "pop r13",  "pop r14",  "pop r15",
        "add rsp, 8", // pop dummy error code
        "swapgs",
        "iretq",

        // Ring 0: EOI + count idle tick
        "2:",
        "push rax",
        "push rdx",
        "mov rdx, [rip + LAPIC_BASE]",
        "mov dword ptr [rdx + 0xB0], 0",
        "lock inc qword ptr [rip + {total_ticks}]",
        "pop rdx",
        "pop rax",
        "iretq",
        handler = sym timer_handler,
        total_ticks = sym CPU_TOTAL_TICKS,
    );
}

extern "sysv64" fn timer_handler() {
    crate::arch::apic::eoi();
    CPU_BUSY_TICKS.fetch_add(1, Ordering::Relaxed);
    CPU_TOTAL_TICKS.fetch_add(1, Ordering::Relaxed);
    crate::scheduler::preempt();
}

// --- Exception entry stubs ---
//
// Asm: ring check + swapgs + save GPRs + call Rust handler(vector, regs_ptr).
// Rust handler never returns (kills process or halts kernel).

/// #UD — no error code pushed by CPU, so CS is at [rsp + 8].
#[unsafe(naked)]
extern "sysv64" fn ud_entry() {
    naked_asm!(
        "test dword ptr [rsp + 8], 3",
        "jz 1f",
        "swapgs",
        "1:",
        "push 0", // dummy error code for uniform stack layout
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",
        "mov rdi, 6",
        "mov rsi, rsp",
        "sub rsp, 8",
        "call {handler}",
        "cli", "hlt",
        handler = sym exception_handler,
    );
}

/// #GP — CPU pushes error code, so CS is at [rsp + 16].
#[unsafe(naked)]
extern "sysv64" fn gpf_entry() {
    naked_asm!(
        "test dword ptr [rsp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",
        "mov rdi, 13",
        "mov rsi, rsp",
        "sub rsp, 8",
        "call {handler}",
        "cli", "hlt",
        handler = sym exception_handler,
    );
}

/// Page fault entry — asm only does: ring check, swapgs, save GPRs, call Rust, restore, iretq.
/// All logic (demand paging vs fatal) lives in the Rust handler.
/// If the Rust handler returns, the fault was resolved. If fatal, it diverges.
#[unsafe(naked)]
extern "sysv64" fn page_fault_entry() {
    naked_asm!(
        // Error code on stack. CS is at [rsp + 16].
        "test dword ptr [rsp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",

        // One arg: pointer to saved regs
        "mov rdi, rsp",
        "sub rsp, 8", // 16-byte align (15 GPR pushes + error code = 16 pushes = aligned, but sub 8 for call)
        "call {handler}",
        "add rsp, 8",

        // Handler returned — fault was resolved. Restore and return.
        "pop rax",  "pop rbx",  "pop rcx",  "pop rdx",
        "pop rsi",  "pop rdi",  "pop rbp",
        "pop r8",   "pop r9",   "pop r10",  "pop r11",
        "pop r12",  "pop r13",  "pop r14",  "pop r15",
        "add rsp, 8", // skip error code
        "test dword ptr [rsp + 8], 3",
        "jz 3f",
        "swapgs",
        "3:",
        "iretq",
        handler = sym page_fault_handler,
    );
}

/// Double fault — runs on IST1 with a dedicated stack. Always from kernel (no swapgs).
#[unsafe(naked)]
extern "sysv64" fn double_fault_entry() {
    naked_asm!(
        // CPU pushes error code (always 0) for #DF.
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",
        "mov rdi, rsp",
        "sub rsp, 8", // 16-byte align
        "call {handler}",
        "cli", "hlt",
        handler = sym double_fault_handler,
    );
}

// ============================================================
// Rust handlers — all logic lives here, zero asm
// ============================================================

// --- Double fault ---
//
// The double fault handler runs on IST1 — a dedicated stack that is always valid.
// All memory reads go through safe_read_kernel() to prevent triple faults.
// After printing the kernel backtrace, it scans the original kernel stack for the
// interrupt frame that triggered the chain, recovering the user context if present.

/// Safe kernel memory read for the double fault handler.
/// Only reads identity-mapped kernel addresses. Returns None for anything suspect.
fn safe_read_kernel(addr: u64) -> Option<u64> {
    if addr % 8 != 0 || !is_kernel_addr(addr) {
        return None;
    }
    Some(unsafe { core::ptr::read_volatile(addr as *const u64) })
}

extern "sysv64" fn double_fault_handler(regs: *const SavedRegs) -> ! {
    let regs = unsafe { &*regs };
    let frame = regs.interrupt_frame();
    let cr2 = cpu::read_cr2();
    let cpu_id = percpu::cpu_id();
    let pid = percpu::current_pid();

    log!("DOUBLE FAULT on CPU {} (pid={:?})", cpu_id, pid);
    log!("  cr2={:#018x} (address that caused the fault chain)", cr2);
    log!("  rip={:#018x}  rsp={:#018x}  rbp={:#018x}", frame.rip, frame.rsp, regs.rbp);

    // Kernel backtrace (where the double fault actually fired)
    log!("  Kernel backtrace:");
    crate::symbols::resolve_kernel_nonblocking(frame.rip);
    let mut rbp = regs.rbp;
    for _ in 0..20 {
        let Some(saved_rbp) = safe_read_kernel(rbp) else { break };
        let Some(return_addr) = safe_read_kernel(rbp + 8) else { break };
        if return_addr == 0 { break; }
        crate::symbols::resolve_kernel_nonblocking(return_addr);
        rbp = saved_rbp;
    }

    // Scan the original kernel stack for the interrupt frame that started
    // the exception chain. Our exception entry stubs push SavedRegs (15 u64s)
    // then an error code, then the CPU's interrupt frame follows:
    //   [SavedRegs] [error_code] [RIP] [CS] [RFLAGS] [RSP] [SS]
    // We look for a slot where [CS] is a valid code segment selector (0x08 or 0x23).
    let kernel_rsp = frame.rsp;
    log!("  Scanning kernel stack at {:#x} for original exception context...", kernel_rsp);

    // Scan upward from where the double fault's RSP was (the old kernel stack).
    // The interrupt frame could be anywhere above, within a reasonable range.
    let scan_start = kernel_rsp;
    let scan_end = kernel_rsp + 4096; // kernel stacks are typically 16-64KB
    let mut addr = scan_start;

    while addr < scan_end {
        // Check if this looks like an interrupt frame: [error_code] [RIP] [CS] [RFLAGS] [RSP] [SS]
        // CS must be 0x08 (kernel) or 0x23 (user code64), and RFLAGS must have bit 1 set (always 1).
        let Some(maybe_rip) = safe_read_kernel(addr) else { break };
        let Some(maybe_cs) = safe_read_kernel(addr + 8) else { break };
        let Some(maybe_rflags) = safe_read_kernel(addr + 16) else { break };
        let Some(maybe_rsp) = safe_read_kernel(addr + 24) else { break };

        let valid_cs = maybe_cs == 0x08 || maybe_cs == 0x23;
        let valid_rflags = maybe_rflags & 2 != 0 && maybe_rflags & !0x3F_FFFF == 0;
        let valid_rip = maybe_rip > 0x1000; // not null

        if valid_cs && valid_rflags && valid_rip {
            let is_user = maybe_cs == 0x23;
            log!("  Found interrupt frame at stack offset +{:#x}:", addr - kernel_rsp);
            log!("    rip={:#018x}  cs={:#x}  rflags={:#x}", maybe_rip, maybe_cs, maybe_rflags);
            log!("    rsp={:#018x}", maybe_rsp);

            // Check if SavedRegs sit just below this interrupt frame
            // Layout: [SavedRegs (15*8=120 bytes)] [error_code (8)] [RIP] [CS] ...
            // So error_code is at addr - 8, and SavedRegs starts at addr - 8 - 15*8
            let error_code_addr = addr.wrapping_sub(8);
            let saved_regs_base = error_code_addr.wrapping_sub(15 * 8);
            if let Some(error_code) = safe_read_kernel(error_code_addr) {
                log!("    error_code={:#x}", error_code);
            }

            if is_user {
                // Try to recover user RBP from SavedRegs (rbp is at offset 6*8)
                let user_rbp_addr = saved_regs_base + 6 * 8;
                if let Some(user_rbp) = safe_read_kernel(user_rbp_addr) {
                    log!("  User context (pid={:?}):", pid);
                    log!("    rip={:#018x}  rsp={:#018x}  rbp={:#018x}", maybe_rip, maybe_rsp, user_rbp);

                    // Walk user backtrace through page tables
                    let pml4 = cpu::read_cr3() as *const u64;
                    log!("  User backtrace:");
                    if let Some(p) = pid {
                        if !process::resolve_user_symbol(p, maybe_rip) {
                            log!("    {:#x}", maybe_rip);
                        }
                    } else {
                        log!("    {:#x}", maybe_rip);
                    }
                    let mut ubp = user_rbp;
                    for _ in 0..20 {
                        if ubp == 0 || ubp % 8 != 0 { break; }
                        let Some(saved) = safe_read_u64(ubp, pml4) else { break };
                        let Some(ret) = safe_read_u64(ubp + 8, pml4) else { break };
                        if ret == 0 { break; }
                        if let Some(p) = pid {
                            if !process::resolve_user_symbol(p, ret) {
                                log!("    {:#x}", ret);
                            }
                        } else {
                            log!("    {:#x}", ret);
                        }
                        ubp = saved;
                    }
                }
            } else {
                log!("  Original fault was in kernel code");
                log!("  Kernel backtrace from original fault:");
                crate::symbols::resolve_kernel_nonblocking(maybe_rip);
                // Walk RBP chain from the saved regs
                let rbp_addr = saved_regs_base + 6 * 8;
                if let Some(orig_rbp) = safe_read_kernel(rbp_addr) {
                    let mut bp = orig_rbp;
                    for _ in 0..20 {
                        let Some(saved) = safe_read_kernel(bp) else { break };
                        let Some(ret) = safe_read_kernel(bp + 8) else { break };
                        if ret == 0 { break; }
                        crate::symbols::resolve_kernel_nonblocking(ret);
                        bp = saved;
                    }
                }
            }
            break;
        }

        addr += 8;
    }

    cpu::halt();
}

// --- Page fault (demand paging) ---

/// Returns normally if the fault was resolved (page mapped in).
/// Diverges (never returns) if the fault is fatal.
extern "sysv64" fn page_fault_handler(regs: *const SavedRegs) {
    let regs = unsafe { &*regs };
    let frame = regs.interrupt_frame();
    let fault_addr = cpu::read_cr2();

    // SMAP violation detection: kernel-mode protection fault on user address
    if frame.error_code & PF_PRESENT != 0 && frame.cs & RPL_MASK == 0
        && fault_addr < 0x0000_8000_0000_0000
    {
        log!("SMAP cr2={:#018x} rip={:#018x} err={:#018x} rflags={:#018x}",
            fault_addr, frame.rip, frame.error_code, frame.rflags);
    }

    // Only handle not-present faults — protection violations are always fatal
    if frame.error_code & PF_PRESENT == 0 {
        let is_user = frame.cs & RPL_MASK != 0;
        if is_user || percpu::current_pid().is_some() {
            if process::handle_page_fault(fault_addr, frame.error_code) {
                return;
            }
        }
    }

    // Fatal — build context and terminate
    let ctx = ExceptionContext {
        vector: Vector::PageFault,
        regs,
        frame,
        cr2: fault_addr,
    };
    fatal_exception(&ctx);
}

// ============================================================
// Exception diagnostics — all allocation-free
// ============================================================

/// Complete CPU state at the time of an exception.
struct ExceptionContext<'a> {
    vector: Vector,
    regs: &'a SavedRegs,
    frame: &'a InterruptFrame,
    cr2: u64,
}

impl ExceptionContext<'_> {
    /// Whether the exception occurred in user mode (Ring 3).
    fn is_user_mode(&self) -> bool {
        self.frame.cs & RPL_MASK != 0
    }

    /// Whether this fault should be attributed to a user process.
    /// True for Ring 3 faults, and also for kernel-mode faults on user memory
    /// during a syscall (e.g. bad pointer passed to write()).
    fn is_user_fault(&self) -> bool {
        self.is_user_mode()
            || (percpu::current_pid().is_some()
                && self.cr2 < 0x0000_8000_0000_0000
                && matches!(self.vector, Vector::PageFault | Vector::GeneralProtection))
    }

    /// PML4 for safe memory reads. Uses page table translation for user faults
    /// to avoid nested demand-paging faults inside the exception handler.
    fn pml4(&self) -> *const u64 {
        if self.is_user_fault() {
            cpu::read_cr3() as *const u64
        } else {
            core::ptr::null()
        }
    }
}

/// Log an address with symbol resolution (allocation-free).
fn log_addr(addr: u64, is_user: bool) {
    if is_user {
        if let Some(pid) = percpu::current_pid() {
            if process::resolve_user_symbol(pid, addr) {
                return;
            }
        }
        crate::log!("    {:#x}", addr);
    } else {
        crate::symbols::resolve_kernel(addr);
    }
}

/// Check if addr is a plausible kernel pointer (in identity-mapped RAM).
fn is_kernel_addr(addr: u64) -> bool {
    addr > 0x1000 && addr < 0x1_0000_0000_0000
}

/// Safely read a u64 from memory. For user addresses, translates through page
/// tables to avoid triggering demand-paging faults inside exception handlers.
/// For kernel addresses, reads directly via identity mapping.
fn safe_read_u64(addr: u64, user_pml4: *const u64) -> Option<u64> {
    if addr % 8 != 0 || addr == 0 {
        return None;
    }
    if !user_pml4.is_null() {
        let phys = paging::virt_to_phys(user_pml4, addr)?;
        Some(unsafe { *(phys as *const u64) })
    } else if is_kernel_addr(addr) {
        Some(unsafe { *(addr as *const u64) })
    } else {
        None
    }
}

/// Walk the frame-pointer chain and log return addresses.
fn dump_backtrace(rip: u64, rbp: u64, is_user: bool, pml4: *const u64) {
    log!("  Backtrace:");
    log_addr(rip, is_user);
    let mut rbp = rbp;
    for _ in 0..20 {
        if rbp == 0 || rbp % 8 != 0 { break; }
        let Some(saved_rbp) = safe_read_u64(rbp, pml4) else { break };
        let Some(return_addr) = safe_read_u64(rbp + 8, pml4) else { break };
        if return_addr == 0 { break; }
        log_addr(return_addr, is_user);
        rbp = saved_rbp;
    }
}

/// Dump raw stack values from RSP and around RBP.
fn dump_stack(rsp: u64, rbp: u64, pml4: *const u64) {
    if safe_read_u64(rsp, pml4).is_some() {
        log!("  Stack (from RSP):");
        for i in 0..8u64 {
            let addr = rsp + i * 8;
            let Some(val) = safe_read_u64(addr, pml4) else { break };
            log!("    [{:#x}] = {:#018x}", addr, val);
        }
    }
    if safe_read_u64(rbp, pml4).is_some() {
        log!("  Frame (around RBP={:#x}):", rbp);
        for offset in [-0x30i64, -0x28, -0x20, -0x18, -0x10, -0x8, 0, 8, 0x10, 0x18, 0x20, 0x28] {
            let addr = (rbp as i64 + offset) as u64;
            let Some(val) = safe_read_u64(addr, pml4) else { continue };
            log!("    [RBP{:+}] = {:#018x}", offset, val);
        }
    }
}

/// Fatal exception handler. Prints diagnostics, then kills the process (user fault)
/// or halts the kernel (kernel fault). Never returns.
///
/// All logging is allocation-free — log! writes directly to serial.
/// format!() is forbidden (allocates, will deadlock if allocator lock is held).
extern "sysv64" fn exception_handler(raw_vector: u64, regs: *const SavedRegs) -> ! {
    let regs = unsafe { &*regs };
    let vector = Vector::from_raw(raw_vector);
    let frame = regs.interrupt_frame();
    let cr2 = if vector == Vector::PageFault { cpu::read_cr2() } else { 0 };
    let ctx = ExceptionContext { vector, regs, frame, cr2 };
    fatal_exception(&ctx);
}

/// Core fatal exception logic. Shared by page_fault_handler (when unresolvable)
/// and exception_handler (for all other fatal exceptions).
fn fatal_exception(ctx: &ExceptionContext) -> ! {
    let is_user = ctx.is_user_fault();
    let pml4 = ctx.pml4();

    // --- Header ---
    if is_user {
        let pid = percpu::current_pid().unwrap_or(crate::process::Pid(0));
        match ctx.vector {
            Vector::PageFault => {
                let action = if ctx.frame.error_code & PF_INSTRUCTION_FETCH != 0 { "execute" }
                    else if ctx.frame.error_code & PF_WRITE != 0 { "write" }
                    else { "read" };
                let cause = if ctx.frame.error_code & PF_PRESENT != 0 { "protection violation" }
                    else { "page not mapped" };
                log!("SEGFAULT pid={}: {} at {:#x} ({})", pid, action, ctx.cr2, cause);
            }
            Vector::InvalidOpcode => log!("SIGILL pid={}: illegal instruction", pid),
            Vector::GeneralProtection => log!("SIGBUS pid={}: general protection fault (error_code={:#x})", pid, ctx.frame.error_code),
            Vector::DoubleFault => log!("FATAL pid={}: double fault", pid),
            Vector::Timer | Vector::Xhci | Vector::TlbFlush => unreachable!(),
        }
    } else {
        let cpu = percpu::cpu_id();
        match ctx.vector {
            Vector::PageFault => {
                let action = if ctx.frame.error_code & PF_INSTRUCTION_FETCH != 0 { "execute" }
                    else if ctx.frame.error_code & PF_WRITE != 0 { "write" }
                    else { "read" };
                let cause = if ctx.frame.error_code & PF_PRESENT != 0 { "protection violation" }
                    else { "page not mapped" };
                log!("KERNEL PANIC cpu={} pid={:?}: page fault: {} at {:#x} ({})",
                    cpu, percpu::current_pid(), action, ctx.cr2, cause);
            }
            _ => {
                let name = match ctx.vector {
                    Vector::InvalidOpcode => "invalid opcode",
                    Vector::GeneralProtection => "general protection fault",
                    Vector::DoubleFault => "double fault",
                    Vector::PageFault | Vector::Timer | Vector::Xhci | Vector::TlbFlush => unreachable!(),
                };
                log!("KERNEL PANIC cpu={} pid={:?}: {} (error_code={:#x})",
                    cpu, percpu::current_pid(), name, ctx.frame.error_code);
            }
        }
    }

    // --- Crash location ---
    log!("  rip:");
    log_addr(ctx.frame.rip, is_user);

    if ctx.vector == Vector::PageFault {
        paging::debug_page_walk(ctx.cr2);
    }

    // --- Registers ---
    log!("  Registers:");
    log!("    rax={:#018x}  rbx={:#018x}", ctx.regs.rax, ctx.regs.rbx);
    log!("    rcx={:#018x}  rdx={:#018x}", ctx.regs.rcx, ctx.regs.rdx);
    log!("    rsi={:#018x}  rdi={:#018x}", ctx.regs.rsi, ctx.regs.rdi);
    log!("    rbp={:#018x}  rsp={:#018x}", ctx.regs.rbp, ctx.frame.rsp);
    log!("     r8={:#018x}   r9={:#018x}", ctx.regs.r8, ctx.regs.r9);
    log!("    r10={:#018x}  r11={:#018x}", ctx.regs.r10, ctx.regs.r11);
    log!("    r12={:#018x}  r13={:#018x}", ctx.regs.r12, ctx.regs.r13);
    log!("    r14={:#018x}  r15={:#018x}", ctx.regs.r14, ctx.regs.r15);

    // --- Backtrace & stack ---
    dump_backtrace(ctx.frame.rip, ctx.regs.rbp, is_user, pml4);
    dump_stack(ctx.frame.rsp, ctx.regs.rbp, pml4);

    // --- Terminate ---
    if is_user {
        let crash_addr = if ctx.vector == Vector::PageFault { ctx.cr2 } else { 0 };
        process::dump_crash_diagnostics(crash_addr, ctx.frame.rip);
        syscall::kill_process(-1);
    }
    cpu::halt();
}

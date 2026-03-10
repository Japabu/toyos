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

// Exception vectors
const VECTOR_INVALID_OPCODE: u64 = 6;
const VECTOR_DOUBLE_FAULT: u64 = 8;
const VECTOR_GPF: u64 = 13;
const VECTOR_PAGE_FAULT: u64 = 14;

// Page fault error code bits
const PF_PRESENT: u64 = 1 << 0;
const PF_WRITE: u64 = 1 << 1;
const PF_INSTRUCTION_FETCH: u64 = 1 << 4;

// Timer vector
const VECTOR_TIMER: usize = 0x20;

// xHCI MSI-X vector
const VECTOR_XHCI: usize = 0x21;

// IPI vectors
const VECTOR_TLB_FLUSH: usize = 0xFE;

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

// Saved general-purpose registers (pushed by exception entry stubs)
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

// CPU-pushed interrupt/exception frame (follows saved regs + error code on stack)
#[repr(C)]
struct InterruptFrame {
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
        idt.entries[VECTOR_INVALID_OPCODE as usize] = IdtEntry::new(ud_entry as *const () as u64);
        idt.entries[VECTOR_DOUBLE_FAULT as usize] = IdtEntry::new(double_fault_entry as *const () as u64).with_ist(1);
        idt.entries[VECTOR_GPF as usize] = IdtEntry::new(gpf_entry as *const () as u64);
        idt.entries[VECTOR_PAGE_FAULT as usize] = IdtEntry::new(page_fault_entry as *const () as u64);
        idt.entries[VECTOR_TIMER] = IdtEntry::new(timer_entry as *const () as u64);
        idt.entries[VECTOR_XHCI] = IdtEntry::new(xhci_entry as *const () as u64);
        idt.entries[VECTOR_TLB_FLUSH] = IdtEntry::new(tlb_flush_entry as *const () as u64);
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

// --- xHCI MSI-X handler (vector 0x21) ---
// Minimal: set atomic flag + EOI. No lock acquisition.
#[unsafe(naked)]
extern "C" fn xhci_entry() {
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
extern "C" fn tlb_flush_entry() {
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
// Ring 3: save all registers (GPR + SSE/XMM), preempt, restore, iretq.
// Ring 0: just EOI and return (no preemption while kernel code runs).
#[unsafe(naked)]
extern "C" fn timer_entry() {
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

extern "C" fn timer_handler() {
    crate::arch::apic::eoi();
    CPU_BUSY_TICKS.fetch_add(1, Ordering::Relaxed);
    CPU_TOTAL_TICKS.fetch_add(1, Ordering::Relaxed);
    crate::scheduler::preempt();
}

// --- Exception entry stubs ---
// Exceptions from ring 3 need swapgs so kernel code sees the per-CPU GS base.
// Handler args: rdi=vector, rsi=regs_ptr, rdx=error_code, rcx=rip, r8=cs, r9=fault_addr

macro_rules! exception_entry {
    ($name:ident, $vector:literal, no_error_code) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            naked_asm!(
                "test dword ptr [rsp + 8], 3",
                "jz 1f",
                "swapgs",
                "1:",
                "push 0",
                "push r15", "push r14", "push r13", "push r12",
                "push r11", "push r10", "push r9",  "push r8",
                "push rbp", "push rdi", "push rsi", "push rdx",
                "push rcx", "push rbx", "push rax",
                concat!("mov rdi, ", $vector),
                "mov rsi, rsp",
                "mov rdx, [rsp + 15*8]",
                "mov rcx, [rsp + 16*8]",
                "mov r8,  [rsp + 17*8]",
                "xor r9, r9",
                "sub rsp, 8",
                "call {handler}", "cli", "hlt",
                handler = sym exception_handler,
            );
        }
    };
    ($name:ident, $vector:literal, error_code) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            naked_asm!(
                "test dword ptr [rsp + 16], 3",
                "jz 1f",
                "swapgs",
                "1:",
                "push r15", "push r14", "push r13", "push r12",
                "push r11", "push r10", "push r9",  "push r8",
                "push rbp", "push rdi", "push rsi", "push rdx",
                "push rcx", "push rbx", "push rax",
                concat!("mov rdi, ", $vector),
                "mov rsi, rsp",
                "mov rdx, [rsp + 15*8]",
                "mov rcx, [rsp + 16*8]",
                "mov r8,  [rsp + 17*8]",
                "xor r9, r9",
                "sub rsp, 8",
                "call {handler}", "cli", "hlt",
                handler = sym exception_handler,
            );
        }
    };
}

exception_entry!(ud_entry,         "6",  no_error_code);
exception_entry!(gpf_entry,        "13", error_code);

/// Page fault entry — can return to userspace if the fault is resolved by demand paging.
/// The kernel is compiled with +soft-float (no SSE), so the handler cannot clobber XMM.
/// GPR save/restore is still required since the handler uses general-purpose registers.
#[unsafe(naked)]
extern "C" fn page_fault_entry() {
    naked_asm!(
        // Error code is on stack. CS is at [rsp + 16].
        "test dword ptr [rsp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",

        // Args: rdi=error_code, rsi=rip, rdx=cs, rcx=cr2
        // 15 GPR pushes on stack, then error_code at [rsp + 15*8]
        "mov rdi, [rsp + 15*8]",  // error_code
        "mov rsi, [rsp + 16*8]",  // rip
        "mov rdx, [rsp + 17*8]",  // cs
        "mov rcx, cr2",           // fault_addr
        "call {handler}",
        "test eax, eax",
        "jnz 2f",

        // Resolved: restore GPRs and return
        "pop rax",  "pop rbx",  "pop rcx",  "pop rdx",
        "pop rsi",  "pop rdi",  "pop rbp",
        "pop r8",   "pop r9",   "pop r10",  "pop r11",
        "pop r12",  "pop r13",  "pop r14",  "pop r15",
        "add rsp, 8",  // skip error code
        "test dword ptr [rsp + 8], 3",
        "jz 3f",
        "swapgs",
        "3:",
        "iretq",

        // Fatal: fall through to exception_handler
        "2:",
        "mov rdi, 14",            // vector
        "mov rsi, rsp",           // regs (SavedRegs pointer)
        "mov rdx, [rsp + 15*8]",  // error_code
        "mov rcx, [rsp + 16*8]",  // rip
        "mov r8,  [rsp + 17*8]",  // cs
        "mov r9, cr2",
        "sub rsp, 8",             // 16-byte align for call
        "call {exc_handler}",
        "cli", "hlt",
        handler = sym page_fault_handler,
        exc_handler = sym exception_handler,
    );
}

/// Double fault handler — runs on IST1 with a dedicated stack.
/// Minimal: just log and halt. The original stack is unusable.
#[unsafe(naked)]
extern "C" fn double_fault_entry() {
    naked_asm!(
        // Double fault always comes from kernel (ring 0) — no swapgs needed.
        // CPU pushes error code (always 0) for #DF.
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",
        "mov rdi, rsp",       // pointer to saved regs
        "mov rsi, [rsp + 16*8]", // RIP from interrupt frame
        "mov rdx, cr2",       // faulting address (if page-fault triggered this)
        "call {handler}",
        "cli", "hlt",
        handler = sym double_fault_handler,
    );
}

// --- Double fault handler ---

extern "C" fn double_fault_handler(regs: *const SavedRegs, rip: u64, cr2: u64) {
    let regs = unsafe { &*regs };
    let frame = regs.interrupt_frame();
    let cpu = percpu::cpu_id();
    log!("DOUBLE FAULT on CPU {} (pid={:?})", cpu, percpu::current_pid());
    log!("  Likely cause: kernel stack overflow");
    log!("  rip={:#018x}  cr2={:#018x}", rip, cr2);
    log!("  rsp={:#018x}  rbp={:#018x}", frame.rsp, regs.rbp);
    log!("  Backtrace:");
    crate::symbols::resolve_kernel_nonblocking(rip);
    let mut rbp = regs.rbp;
    for _ in 0..20 {
        if rbp == 0 || rbp % 8 != 0 || !is_kernel_addr(rbp) { break; }
        let return_addr = unsafe { *((rbp + 8) as *const u64) };
        if return_addr == 0 { break; }
        crate::symbols::resolve_kernel_nonblocking(return_addr);
        rbp = unsafe { *(rbp as *const u64) };
    }
    cpu::halt();
}

// --- Page fault handler (demand paging) ---

/// Returns 0 if the fault was resolved (demand page mapped), nonzero if fatal.
extern "C" fn page_fault_handler(error_code: u64, _rip: u64, cs: u64, fault_addr: u64) -> u32 {
    let is_user = cs & RPL_MASK != 0;
    let not_present = error_code & PF_PRESENT == 0;

    // Only handle not-present faults via demand paging
    if !not_present {
        return 1;
    }

    // User-mode fault: resolve via VMA demand paging
    if is_user {
        if process::handle_page_fault(fault_addr, error_code) {
            return 0;
        }
        return 1;
    }

    // Kernel-mode fault on user address: syscall dereferencing a demand-paged user pointer.
    // Check if the faulting address is in the current process's VMA list.
    if percpu::current_pid().is_some() {
        if process::handle_page_fault(fault_addr, error_code) {
            return 0;
        }
    }

    1 // fatal
}

// --- Exception handler ---

/// Log an address with symbol resolution (allocation-free).
fn log_addr(addr: u64, is_user: bool) {
    if is_user {
        // Try resolving against the current process's symbols first
        if let Some(pid) = percpu::current_pid() {
            if process::resolve_user_symbol(pid, addr) {
                return;
            }
        }
        // No user symbols available — just print the raw address
        crate::log!("    {:#x}", addr);
    } else {
        crate::symbols::resolve_kernel(addr);
    }
}

/// Check if addr is a plausible kernel pointer (in identity-mapped RAM).
fn is_kernel_addr(addr: u64) -> bool {
    addr > 0x1000 && addr < 0x1_0000_0000_0000
}

extern "C" fn exception_handler(
    vector: u64,
    regs: *const SavedRegs,
    error_code: u64,
    rip: u64,
    cs: u64,
    fault_addr: u64,
) {
    let is_user = cs & RPL_MASK != 0;
    let regs = unsafe { &*regs };

    // All logging in this handler MUST be allocation-free (log! uses stack-based
    // formatting). format!() is forbidden — it allocates and will deadlock if the
    // exception occurred while the allocator lock was held.

    let name = match vector {
        VECTOR_INVALID_OPCODE => "Invalid Opcode",
        VECTOR_GPF => "General Protection Fault",
        VECTOR_PAGE_FAULT => "Page Fault",
        _ => "Exception",
    };

    if vector == VECTOR_PAGE_FAULT {
        let action = if error_code & PF_INSTRUCTION_FETCH != 0 {
            "execute"
        } else if error_code & PF_WRITE != 0 {
            "write"
        } else {
            "read"
        };
        let cause = if error_code & PF_PRESENT != 0 {
            "protection violation"
        } else {
            "page not mapped"
        };
        if is_user {
            let pid = percpu::current_pid().unwrap_or(crate::process::Pid(0));
            log!("Process {} crashed: {}: {} at {:#x} ({})", pid, name, action, fault_addr, cause);
        } else {
            let cpu = percpu::cpu_id();
            log!("KERNEL PANIC on CPU {} (pid={:?}): {}: {} at {:#x} ({})",
                cpu, percpu::current_pid(), name, action, fault_addr, cause);
        }
    } else {
        if is_user {
            let pid = percpu::current_pid().unwrap_or(crate::process::Pid(0));
            log!("Process {} crashed: {} (error_code={:#x})", pid, name, error_code);
        } else {
            let cpu = percpu::cpu_id();
            log!("KERNEL PANIC on CPU {} (pid={:?}): {} (error_code={:#x})",
                cpu, percpu::current_pid(), name, error_code);
        }
    }

    log!("  rip:");
    log_addr(rip, is_user);

    if vector == VECTOR_PAGE_FAULT {
        paging::debug_page_walk(fault_addr);
    }

    let frame = regs.interrupt_frame();

    // Register dump
    log!("  Registers:");
    log!("    rax={:#018x}  rbx={:#018x}", regs.rax, regs.rbx);
    log!("    rcx={:#018x}  rdx={:#018x}", regs.rcx, regs.rdx);
    log!("    rsi={:#018x}  rdi={:#018x}", regs.rsi, regs.rdi);
    log!("    rbp={:#018x}  rsp={:#018x}", regs.rbp, frame.rsp);
    log!("     r8={:#018x}   r9={:#018x}", regs.r8, regs.r9);
    log!("    r10={:#018x}  r11={:#018x}", regs.r10, regs.r11);
    log!("    r12={:#018x}  r13={:#018x}", regs.r12, regs.r13);
    log!("    r14={:#018x}  r15={:#018x}", regs.r14, regs.r15);

    // Backtrace
    log!("  Backtrace:");
    log_addr(rip, is_user);
    let mut rbp = regs.rbp;
    for _i in 1..20 {
        if rbp == 0 || rbp % 8 != 0 { break; }
        let valid = if is_user {
            process::is_valid_user_addr(rbp) && process::is_valid_user_addr(rbp + 8)
        } else {
            is_kernel_addr(rbp)
        };
        if !valid { break; }
        let saved_rbp = unsafe { *(rbp as *const u64) };
        let return_addr = unsafe { *((rbp + 8) as *const u64) };
        if return_addr == 0 { break; }
        log_addr(return_addr, is_user);
        rbp = saved_rbp;
    }

    // Stack dump — dump raw values from RSP and around RBP
    let dump_rsp = frame.rsp;
    let addr_valid = |a: u64| -> bool {
        a % 8 == 0 && a > 0 && (is_kernel_addr(a) || (is_user && process::is_valid_user_addr(a)))
    };
    if addr_valid(dump_rsp) {
        log!("  Stack (from RSP):");
        for i in 0..8u64 {
            let addr = dump_rsp + i * 8;
            if !addr_valid(addr) { break; }
            let val = unsafe { *(addr as *const u64) };
            log!("    [{:#x}] = {:#018x}", addr, val);
        }
    }
    // Dump around RBP (caller's frame)
    let dump_rbp = regs.rbp;
    if addr_valid(dump_rbp) {
        log!("  Frame (around RBP={:#x}):", dump_rbp);
        for offset in [-0x30i64, -0x28, -0x20, -0x18, -0x10, -0x8, 0, 8, 0x10, 0x18, 0x20, 0x28] {
            let addr = (dump_rbp as i64 + offset) as u64;
            if !addr_valid(addr) { continue; }
            let val = unsafe { *(addr as *const u64) };
            log!("    [RBP{:+}] = {:#018x}", offset, val);
        }
    }

    if is_user {
        let crash_addr = if vector == VECTOR_PAGE_FAULT { fault_addr } else { 0 };
        process::dump_crash_diagnostics(crash_addr, rip);
        syscall::kill_process(-1);
    } else {
        cpu::halt();
    }
}

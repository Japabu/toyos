use core::arch::naked_asm;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::cpu;
use super::cpu::{outb, io_wait};
use crate::arch::{paging, syscall, percpu};
use crate::{symbols, process, log};

use alloc::format;

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
// Ring 3: save all registers, preempt, restore, iretq.
// Ring 0: just EOI and return (no preemption while kernel code runs).
#[unsafe(naked)]
extern "C" fn timer_entry() {
    naked_asm!(
        // No error code for interrupts. CS is at [rsp + 8].
        "test dword ptr [rsp + 8], 3",
        "jz 2f",

        // Ring 3: preempt
        "swapgs",
        "push 0", // dummy error code for stack layout consistency
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",
        "call {handler}",
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
    ($name:ident, $vector:literal, error_code_cr2) => {
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
                "mov r9, cr2",
                "sub rsp, 8",
                "call {handler}", "cli", "hlt",
                handler = sym exception_handler,
            );
        }
    };
}

exception_entry!(ud_entry,         "6",  no_error_code);
exception_entry!(gpf_entry,        "13", error_code);
exception_entry!(page_fault_entry, "14", error_code_cr2);

// --- Exception handler ---

/// Resolve an address: process symbols (via process table), then kernel symbols.
fn format_addr(addr: u64, is_user: bool) -> alloc::string::String {
    if is_user {
        if let Some((name, offset)) = process::resolve_symbol(addr) {
            return format!("{:#x}  {}+{:#x}", addr, name, offset);
        }
    }
    symbols::format_kernel_addr(addr)
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

    let name = match vector {
        VECTOR_INVALID_OPCODE => "Invalid Opcode",
        VECTOR_GPF => "General Protection Fault",
        VECTOR_PAGE_FAULT => "Page Fault",
        _ => "Exception",
    };

    let detail = if vector == VECTOR_PAGE_FAULT {
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
        format!("{}: {} at {:#x} ({})", name, action, fault_addr, cause)
    } else {
        format!("{} (error_code={:#x})", name, error_code)
    };

    if is_user {
        let pid = percpu::current_pid();
        log!("Process {} crashed: {}", pid, detail);
    } else {
        let cpu = percpu::cpu_id();
        let pid = percpu::current_pid();
        let pid_str = if pid == u32::MAX { format!("idle") } else { format!("{}", pid) };
        log!("KERNEL PANIC on CPU {} (pid={}): {}", cpu, pid_str, detail);
    }
    log!("  rip: {}", format_addr(rip, is_user));

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
    log!("    0: {}", format_addr(rip, is_user));
    let mut rbp = regs.rbp;
    for i in 1..20 {
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
        log!("    {}: {}", i, format_addr(return_addr, is_user));
        rbp = saved_rbp;
    }

    // Stack dump
    let dump_rsp = frame.rsp;
    if dump_rsp % 8 == 0 {
        log!("  Stack:");
        for i in 0..16u64 {
            let addr = dump_rsp + i * 8;
            let valid = if is_user {
                process::is_valid_user_addr(addr)
            } else {
                is_kernel_addr(addr)
            };
            if !valid { break; }
            let val = unsafe { *(addr as *const u64) };
            let sym = if is_user {
                if let Some((name, off)) = process::resolve_symbol(val) {
                    format!("  <{}+{:#x}>", name, off)
                } else {
                    alloc::string::String::new()
                }
            } else if let Some((name, off)) = symbols::resolve_kernel(val) {
                format!("  <{}+{:#x}>", name, off)
            } else {
                alloc::string::String::new()
            };
            log!("    [{:#x}] = {:#018x}{}", addr, val, sym);
        }
    }

    if is_user {
        syscall::kill_process(-1);
    } else {
        cpu::halt();
    }
}

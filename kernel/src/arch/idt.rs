use core::arch::naked_asm;

use super::cpu;
use super::cpu::{outb, io_wait};
use crate::arch::syscall;
use crate::{symbols, log};

use alloc::format;

// PIC ports
const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

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

static mut IDT: Idt = Idt {
    entries: [IdtEntry::EMPTY; 256],
};

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

// Kernel base address for crash diagnostics
static mut KERNEL_BASE: u64 = 0;

pub fn set_kernel_base(base: u64) {
    unsafe { (&raw mut KERNEL_BASE).write(base); }
}

/// Remap the 8259 PIC: master IRQ 0-7 -> vectors 32-39, slave -> 40-47.
fn remap_pic() {
    // ICW1: begin init (bit 4=1, bit 0=ICW4 needed)
    outb(PIC1_CMD, 0x11);
    io_wait();
    outb(PIC2_CMD, 0x11);
    io_wait();

    // ICW2: vector offsets
    outb(PIC1_DATA, 32);
    io_wait();
    outb(PIC2_DATA, 40);
    io_wait();

    // ICW3: master/slave wiring
    outb(PIC1_DATA, 4); // slave on IRQ2
    io_wait();
    outb(PIC2_DATA, 2); // cascade identity
    io_wait();

    // ICW4: 8086 mode
    outb(PIC1_DATA, 0x01);
    io_wait();
    outb(PIC2_DATA, 0x01);
    io_wait();

    // Mask all IRQs (keyboard input is handled via USB polling)
    outb(PIC1_DATA, 0xFF);
    outb(PIC2_DATA, 0xFF);
}

pub fn init() {
    remap_pic();

    unsafe {
        // Register exception handlers
        IDT.entries[6] = IdtEntry::new(ud_entry as u64);
        IDT.entries[13] = IdtEntry::new(gpf_entry as u64);
        IDT.entries[14] = IdtEntry::new(page_fault_entry as u64);

        let ptr = IdtPointer {
            limit: (core::mem::size_of::<Idt>() - 1) as u16,
            base: &raw const IDT as *const Idt as u64,
        };

        cpu::lidt(&ptr as *const IdtPointer as *const u8);
        cpu::enable_interrupts();
    }
}

// --- Exception entry stubs ---
// For exceptions with error codes, the CPU pushes (on the kernel stack):
//   [SS] [RSP] [RFLAGS] [CS] [RIP] [error_code] <- RSP
// For exceptions WITHOUT error codes, we push a dummy 0 to unify the layout.
// After saving all GPRs, the handler is called with:
//   rdi=vector, rsi=regs_ptr, rdx=error_code, rcx=rip, r8=cs, r9=fault_addr

macro_rules! exception_entry {
    // No error code, no fault address (e.g. #UD)
    ($name:ident, $vector:literal, no_error_code) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            naked_asm!(
                "push 0", // dummy error code
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
                "call {handler}", "cli", "hlt",
                handler = sym exception_handler,
            );
        }
    };
    // Has error code, no fault address (e.g. #GP)
    ($name:ident, $vector:literal, error_code) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            naked_asm!(
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
                "call {handler}", "cli", "hlt",
                handler = sym exception_handler,
            );
        }
    };
    // Has error code + reads CR2 (page fault)
    ($name:ident, $vector:literal, error_code_cr2) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            naked_asm!(
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

fn format_addr(addr: u64) -> alloc::string::String {
    if let Some((name, offset)) = symbols::resolve(addr) {
        format!("{:#x}  {}+{:#x}", addr, name, offset)
    } else {
        let kernel_base = unsafe { *(&raw const KERNEL_BASE) };
        if kernel_base != 0 && addr >= kernel_base {
            format!("{:#x}  [kernel+{:#x}]", addr, addr - kernel_base)
        } else {
            format!("{:#x}", addr)
        }
    }
}

extern "C" fn exception_handler(
    vector: u64,
    regs: *const SavedRegs,
    error_code: u64,
    rip: u64,
    cs: u64,
    fault_addr: u64,
) {
    let is_user = cs & 3 != 0;
    let regs = unsafe { &*regs };

    let name = match vector {
        6 => "Invalid Opcode",
        13 => "General Protection Fault",
        14 => "Page Fault",
        _ => "Exception",
    };

    let detail = if vector == 14 {
        let action = if error_code & 16 != 0 {
            "execute"
        } else if error_code & 2 != 0 {
            "write"
        } else {
            "read"
        };
        let cause = if error_code & 1 != 0 {
            "protection violation"
        } else {
            "page not mapped"
        };
        format!("{}: {} at {:#x} ({})", name, action, fault_addr, cause)
    } else {
        format!("{} (error_code={:#x})", name, error_code)
    };

    let prefix = if is_user { "Process crashed" } else { "KERNEL PANIC" };
    log::println(&format!("{}: {}", prefix, detail));
    log::println(&format!("  rip: {}", format_addr(rip)));

    // User RSP is in the CPU's iret frame: 15 saved regs + error_code + RIP + CS + RFLAGS + RSP
    let user_rsp = unsafe { *((regs as *const SavedRegs as *const u64).add(19)) };
    let rsp = if is_user { user_rsp } else { regs.rbp }; // approximate for kernel

    // Instruction bytes at RIP (helps identify the faulting instruction)
    if is_user && symbols::is_valid_user_addr(rip) {
        let mut bytes_str = alloc::string::String::with_capacity(16 * 3);
        for i in 0..16u64 {
            let addr = rip + i;
            if !symbols::is_valid_user_addr(addr) { break; }
            let byte = unsafe { *(addr as *const u8) };
            if !bytes_str.is_empty() { bytes_str.push(' '); }
            bytes_str.push_str(&format!("{:02x}", byte));
        }
        log::println(&format!("  code: {}", bytes_str));
    }

    // Register dump
    log::println("  Registers:");
    log::println(&format!("    rax={:#018x}  rbx={:#018x}", regs.rax, regs.rbx));
    log::println(&format!("    rcx={:#018x}  rdx={:#018x}", regs.rcx, regs.rdx));
    log::println(&format!("    rsi={:#018x}  rdi={:#018x}", regs.rsi, regs.rdi));
    log::println(&format!("    rbp={:#018x}  rsp={:#018x}", regs.rbp, rsp));
    log::println(&format!("     r8={:#018x}   r9={:#018x}", regs.r8, regs.r9));
    log::println(&format!("    r10={:#018x}  r11={:#018x}", regs.r10, regs.r11));
    log::println(&format!("    r12={:#018x}  r13={:#018x}", regs.r12, regs.r13));
    log::println(&format!("    r14={:#018x}  r15={:#018x}", regs.r14, regs.r15));

    // Stack dump (8 words from RSP)
    if is_user && user_rsp % 8 == 0 {
        log::println("  Stack:");
        for i in 0..8u64 {
            let addr = user_rsp + i * 8;
            if !symbols::is_valid_user_addr(addr) && !symbols::is_valid_user_addr(addr + 7) { break; }
            let val = unsafe { *(addr as *const u64) };
            let sym = if let Some((name, off)) = symbols::resolve(val) {
                format!("  <{}+{:#x}>", name, off)
            } else {
                alloc::string::String::new()
            };
            log::println(&format!("    [{:#x}] = {:#018x}{}", addr, val, sym));
        }
    }

    // Stack backtrace
    if is_user {
        log::println("  Backtrace:");
        log::println(&format!("    0: {}", format_addr(rip)));
        let mut rbp = regs.rbp;
        for i in 1..20 {
            if rbp == 0 || rbp % 8 != 0 {
                break;
            }
            if !symbols::is_valid_user_addr(rbp) || !symbols::is_valid_user_addr(rbp + 8) {
                break;
            }
            let saved_rbp = unsafe { *(rbp as *const u64) };
            let return_addr = unsafe { *((rbp + 8) as *const u64) };
            if return_addr == 0 {
                break;
            }
            log::println(&format!("    {}: {}", i, format_addr(return_addr)));
            rbp = saved_rbp;
        }
    }

    if is_user {
        syscall::kill_process(-1);
    } else {
        cpu::halt();
    }
}

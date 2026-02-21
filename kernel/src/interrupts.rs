use core::arch::{asm, naked_asm};

use crate::io::{outb, io_wait};
use crate::{elf, log, serial, syscall};

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
        IDT.entries[13] = IdtEntry::new(gpf_entry as u64);
        IDT.entries[14] = IdtEntry::new(page_fault_entry as u64);

        let ptr = IdtPointer {
            limit: (core::mem::size_of::<Idt>() - 1) as u16,
            base: &raw const IDT as *const Idt as u64,
        };

        asm!("lidt [{}]", in(reg) &ptr);
        asm!("sti");
    }
}

// --- Exception entry stubs ---
// For exceptions with error codes, the CPU pushes (on the kernel stack, since
// IST=0 and TSS.RSP0 is set):
//   [SS] [RSP] [RFLAGS] [CS] [RIP] [error_code] <- RSP
//
// We save all GPRs, then call the handler with:
//   rdi=vector, rsi=regs_ptr, rdx=error_code, rcx=rip, r8=cs, r9=fault_addr

#[unsafe(naked)]
extern "C" fn gpf_entry() {
    naked_asm!(
        // Save all GPRs (reverse order so SavedRegs struct matches)
        "push r15",
        "push r14",
        "push r13",
        "push r12",
        "push r11",
        "push r10",
        "push r9",
        "push r8",
        "push rbp",
        "push rdi",
        "push rsi",
        "push rdx",
        "push rcx",
        "push rbx",
        "push rax",

        // Set up handler args
        "mov rdi, 13",              // vector
        "mov rsi, rsp",             // regs ptr (SavedRegs on stack)
        "mov rdx, [rsp + 15*8]",    // error_code (past 15 saved regs)
        "mov rcx, [rsp + 16*8]",    // RIP
        "mov r8,  [rsp + 17*8]",    // CS
        "xor r9, r9",               // no fault address for #GP

        "call {handler}",
        "cli",
        "hlt",
        handler = sym exception_handler,
    );
}

#[unsafe(naked)]
extern "C" fn page_fault_entry() {
    naked_asm!(
        "push r15",
        "push r14",
        "push r13",
        "push r12",
        "push r11",
        "push r10",
        "push r9",
        "push r8",
        "push rbp",
        "push rdi",
        "push rsi",
        "push rdx",
        "push rcx",
        "push rbx",
        "push rax",

        "mov rdi, 14",              // vector
        "mov rsi, rsp",             // regs ptr
        "mov rdx, [rsp + 15*8]",    // error_code
        "mov rcx, [rsp + 16*8]",    // RIP
        "mov r8,  [rsp + 17*8]",    // CS
        "mov r9, cr2",              // fault address

        "call {handler}",
        "cli",
        "hlt",
        handler = sym exception_handler,
    );
}

// --- Exception handler ---

fn format_addr(addr: u64) -> alloc::string::String {
    if let Some((name, offset)) = elf::resolve_symbol(addr) {
        format!("{:#x}  {}+{:#x}", addr, name, offset)
    } else {
        format!("{:#x}", addr)
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

    let print = |s: &str| {
        log::println(s);
        serial::println(s);
    };

    print(&format!("Process crashed: {}", detail));
    print(&format!("  rip: {}", format_addr(rip)));

    // User RSP is in the CPU's iret frame: 15 saved regs + error_code + RIP + CS + RFLAGS + RSP
    let user_rsp = unsafe { *((regs as *const SavedRegs as *const u64).add(19)) };

    // Register dump
    print("  Registers:");
    print(&format!("    rax={:#018x}  rbx={:#018x}", regs.rax, regs.rbx));
    print(&format!("    rcx={:#018x}  rdx={:#018x}", regs.rcx, regs.rdx));
    print(&format!("    rsi={:#018x}  rdi={:#018x}", regs.rsi, regs.rdi));
    print(&format!("    rbp={:#018x}  rsp={:#018x}", regs.rbp, user_rsp));
    print(&format!("     r8={:#018x}   r9={:#018x}", regs.r8, regs.r9));
    print(&format!("    r10={:#018x}  r11={:#018x}", regs.r10, regs.r11));
    print(&format!("    r12={:#018x}  r13={:#018x}", regs.r12, regs.r13));
    print(&format!("    r14={:#018x}  r15={:#018x}", regs.r14, regs.r15));

    // Stack backtrace
    if is_user {
        print("  Backtrace:");
        print(&format!("    0: {}", format_addr(rip)));
        let mut rbp = regs.rbp;
        for i in 1..20 {
            if rbp == 0 || rbp % 8 != 0 {
                break;
            }
            // Validate the RBP is in user-accessible memory (basic check)
            if !elf::is_valid_user_addr(rbp) || !elf::is_valid_user_addr(rbp + 8) {
                break;
            }
            let saved_rbp = unsafe { *(rbp as *const u64) };
            let return_addr = unsafe { *((rbp + 8) as *const u64) };
            if return_addr == 0 {
                break;
            }
            print(&format!("    {}: {}", i, format_addr(return_addr)));
            rbp = saved_rbp;
        }
    }

    if is_user {
        syscall::kill_process(-1);
    } else {
        serial::println(&format!("KERNEL PANIC: {}, rip={:#x}", detail, rip));
        loop {
            unsafe { asm!("cli; hlt"); }
        }
    }
}

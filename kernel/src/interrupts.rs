use core::arch::{asm, naked_asm};

use crate::io::{outb, io_wait};
use crate::{log, serial, syscall};

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
// For exceptions with error codes, the CPU pushes:
//   [SS] [RSP] [RFLAGS] [CS] [RIP] [error_code] <- RSP

#[unsafe(naked)]
extern "C" fn page_fault_entry() {
    naked_asm!(
        "mov rdi, 14",          // vector
        "mov rsi, [rsp]",      // error_code
        "mov rdx, [rsp + 8]",  // faulting RIP
        "mov rcx, [rsp + 16]", // CS
        "mov r8, cr2",         // fault address
        "call {handler}",
        "cli",
        "hlt",
        handler = sym exception_handler,
    );
}

#[unsafe(naked)]
extern "C" fn gpf_entry() {
    naked_asm!(
        "mov rdi, 13",          // vector
        "mov rsi, [rsp]",      // error_code
        "mov rdx, [rsp + 8]",  // faulting RIP
        "mov rcx, [rsp + 16]", // CS
        "xor r8, r8",          // no fault address for #GP
        "call {handler}",
        "cli",
        "hlt",
        handler = sym exception_handler,
    );
}

// --- Exception handler ---

extern "C" fn exception_handler(vector: u64, error_code: u64, rip: u64, cs: u64, addr: u64) {
    let is_user = cs & 3 != 0;

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
        format!("{}: {} at {:#x} ({})", name, action, addr, cause)
    } else {
        format!("{} (error_code={:#x})", name, error_code)
    };

    if is_user {
        log::println(&format!("Process crashed: {}, rip={:#x}", detail, rip));
        syscall::kill_process(-1);
    } else {
        // Kernel fault — unrecoverable
        serial::println(&format!("KERNEL PANIC: {}, rip={:#x}", detail, rip));
        loop {
            unsafe { asm!("cli; hlt"); }
        }
    }
}

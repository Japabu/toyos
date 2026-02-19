use core::arch::{asm, naked_asm};

use crate::gdt::KERNEL_CS;
use crate::io::{inb, outb, io_wait};

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

    fn set_handler(&mut self, handler: u64) {
        self.offset_low = handler as u16;
        self.offset_mid = (handler >> 16) as u16;
        self.offset_high = (handler >> 32) as u32;
        self.selector = KERNEL_CS;
        self.ist = 0;
        self.type_attr = 0x8E; // present, DPL=0, 64-bit interrupt gate
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

    // Mask all except IRQ1 (keyboard)
    outb(PIC1_DATA, 0xFD); // 1111_1101
    outb(PIC2_DATA, 0xFF);
}

// IRQ1 entry point (keyboard)
#[unsafe(naked)]
extern "C" fn irq1_entry() {
    naked_asm!(
        "push rax",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "call {handler}",
        "mov al, 0x20",
        "out 0x20, al",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",
        "iretq",
        handler = sym keyboard_irq_handler,
    );
}

extern "C" fn keyboard_irq_handler() {
    let scancode = inb(0x60);
    crate::keyboard::handle_scancode(scancode);
}

pub fn init() {
    remap_pic();

    unsafe {
        IDT.entries[33].set_handler(irq1_entry as u64);

        let ptr = IdtPointer {
            limit: (core::mem::size_of::<Idt>() - 1) as u16,
            base: &raw const IDT as *const Idt as u64,
        };

        asm!("lidt [{}]", in(reg) &ptr);
        asm!("sti");
    }
}

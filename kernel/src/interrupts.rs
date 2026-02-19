use core::arch::asm;

use crate::io::{outb, io_wait};

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
        let ptr = IdtPointer {
            limit: (core::mem::size_of::<Idt>() - 1) as u16,
            base: &raw const IDT as *const Idt as u64,
        };

        asm!("lidt [{}]", in(reg) &ptr);
        asm!("sti");
    }
}

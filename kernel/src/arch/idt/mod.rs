mod exceptions;
mod timer;
mod tlb;
pub mod virtio_net;
mod xhci;

use super::cpu;
use super::cpu::{outb, io_wait};
use crate::sync::Lock;

// PIC ports
const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// IDT vector assignments — CPU exceptions and hardware interrupts.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Vector {
    Debug = 0x01,
    InvalidOpcode = 0x06,
    DoubleFault = 0x08,
    GeneralProtection = 0x0D,
    PageFault = 0x0E,
    Timer = 0x20,
    Xhci = 0x21,
    VirtioNet = 0x22,
    TlbFlush = 0xFE,
}

impl Vector {
    fn from_raw(v: u64) -> Self {
        match v {
            0x01 => Self::Debug,
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

pub use timer::cpu_ticks;
pub use xhci::xhci_irq_pending;

pub fn init() {
    disable_pic();

    {
        let mut idt = IDT.lock();
        idt.entries[Vector::Debug as usize] = IdtEntry::new(exceptions::db_entry as *const () as u64);
        idt.entries[Vector::InvalidOpcode as usize] = IdtEntry::new(exceptions::ud_entry as *const () as u64);
        idt.entries[Vector::DoubleFault as usize] = IdtEntry::new(exceptions::double_fault_entry as *const () as u64).with_ist(1);
        idt.entries[Vector::GeneralProtection as usize] = IdtEntry::new(exceptions::gpf_entry as *const () as u64);
        idt.entries[Vector::PageFault as usize] = IdtEntry::new(exceptions::page_fault_entry as *const () as u64);
        idt.entries[Vector::Timer as usize] = IdtEntry::new(timer::timer_entry as *const () as u64);
        idt.entries[Vector::Xhci as usize] = IdtEntry::new(xhci::xhci_entry as *const () as u64);
        idt.entries[Vector::VirtioNet as usize] = IdtEntry::new(virtio_net::virtio_net_entry as *const () as u64);
        idt.entries[Vector::TlbFlush as usize] = IdtEntry::new(tlb::tlb_flush_entry as *const () as u64);
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

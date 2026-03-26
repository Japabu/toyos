pub(crate) mod exceptions;
mod timer;
mod tlb;
pub mod virtio_net;
mod xhci;

use core::arch::naked_asm;

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

// ============================================================
// Unified trap frame — contiguous struct for all exception state
// ============================================================

/// Complete CPU state at exception entry. Pushed by stub + common_entry + CPU.
/// Layout (lowest address = first field):
///   [GPRs: 15×8=120]  [vector: 8]  [error_code: 8]  [rip cs rflags rsp ss: 5×8=40]
#[repr(C)]
pub struct TrapFrame {
    // GPRs pushed by common_entry (lowest address first)
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
    // Pushed by stub
    pub vector: u64,
    // Pushed by CPU (or dummy 0 by stub for exceptions without error code)
    pub error_code: u64,
    // CPU interrupt frame
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

// ============================================================
// Exception entry stubs — tiny per-vector, jump to common_entry
// ============================================================

/// #DB — no CPU error code. Push dummy error code + vector, jump to common.
#[unsafe(naked)]
extern "sysv64" fn stub_db() {
    naked_asm!("push 0", "push 1", "jmp {common}", common = sym common_entry);
}

/// #UD — no CPU error code.
#[unsafe(naked)]
extern "sysv64" fn stub_ud() {
    naked_asm!("push 0", "push 6", "jmp {common}", common = sym common_entry);
}

/// #DF — CPU pushes error code (always 0). Push vector.
#[unsafe(naked)]
extern "sysv64" fn stub_df() {
    naked_asm!("push 8", "jmp {common}", common = sym common_entry);
}

/// #GP — CPU pushes error code. Push vector.
#[unsafe(naked)]
extern "sysv64" fn stub_gpf() {
    naked_asm!("push 13", "jmp {common}", common = sym common_entry);
}

/// #PF — CPU pushes error code. Push vector.
#[unsafe(naked)]
extern "sysv64" fn stub_pf() {
    naked_asm!("push 14", "jmp {common}", common = sym common_entry);
}

/// Common exception entry: save all GPRs, call Rust dispatcher, restore, iretq.
#[unsafe(naked)]
extern "sysv64" fn common_entry() {
    naked_asm!(
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",
        "mov rdi, rsp",        // &TrapFrame
        "sub rsp, 8",          // 16-byte align
        "call {dispatch}",
        "add rsp, 8",
        // If dispatch returns, fault was resolved — restore and iretq
        "pop rax",  "pop rbx",  "pop rcx",  "pop rdx",
        "pop rsi",  "pop rdi",  "pop rbp",
        "pop r8",   "pop r9",   "pop r10",  "pop r11",
        "pop r12",  "pop r13",  "pop r14",  "pop r15",
        "add rsp, 16",         // skip vector + error code
        "iretq",
        dispatch = sym trap_dispatch,
    );
}

/// Rust exception dispatcher — routes by vector to the appropriate handler.
extern "sysv64" fn trap_dispatch(frame: *mut TrapFrame) {
    let frame = unsafe { &mut *frame };
    match frame.vector {
        0x01 => exceptions::debug_handler(frame),
        0x06 | 0x0D => exceptions::exception_handler(frame),
        0x08 => exceptions::double_fault_handler(frame),
        0x0E => {
            cpu::enable_interrupts();
            exceptions::page_fault_handler(frame);
            unsafe { core::arch::asm!("cli", options(nomem, nostack)); }
        }
        v => panic!("unhandled exception vector {:#x}", v),
    }
}

// ============================================================
// PIC disable + IDT init
// ============================================================

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
        idt.entries[Vector::Debug as usize] = IdtEntry::new(stub_db as *const () as u64);
        idt.entries[Vector::InvalidOpcode as usize] = IdtEntry::new(stub_ud as *const () as u64);
        idt.entries[Vector::DoubleFault as usize] = IdtEntry::new(stub_df as *const () as u64).with_ist(1);
        idt.entries[Vector::GeneralProtection as usize] = IdtEntry::new(stub_gpf as *const () as u64);
        idt.entries[Vector::PageFault as usize] = IdtEntry::new(stub_pf as *const () as u64);
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

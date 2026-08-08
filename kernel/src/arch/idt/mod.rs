pub(crate) mod exceptions;
mod device_irq;
mod dma_fault;
mod hda;
mod i8042;
mod nmi;
mod timer;
mod tlb;
mod virtio_net;
mod virtio_sound;
mod xhci;

use core::arch::naked_asm;

use super::cpu;
use super::entry::{restore_user_state, ring3_naked_asm, save_user_state};
use super::cpu::{outb, io_wait};
use crate::sync::Lock;

// PIC ports
const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// The vector both PS/2 lines are routed to. Public because the driver has to
/// name it when it programs the I/O APIC.
pub const I8042_VECTOR: u8 = Vector::I8042 as u8;

/// The vector an IOMMU writes into its own `FEDATA`. Public for the same
/// reason: the unit is told which vector to raise, and only one place knows.
pub const DMA_FAULT_VECTOR: u8 = Vector::DmaFault as u8;

/// The vector the HDA controller's message-signalled interrupt carries. Public
/// for the same reason: the driver arms whichever of MSI-X and MSI the function
/// offers, and only one place knows the number.
pub const HDA_VECTOR: u8 = Vector::Hda as u8;

/// The vector the virtio-sound device's MSI-X entry carries, for the same
/// reason.
pub const VIRTIO_SOUND_VECTOR: u8 = Vector::VirtioSound as u8;

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

// Unified trap frame — contiguous struct for all exception state

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

/// A `dispatched` gate's entry point: push the vector, and where the CPU
/// pushes no error code, a zero in its place so [`TrapFrame`] has one shape.
///
/// Which vectors those are is SDM Vol. 3A Table 6-1 and nothing else: get it
/// wrong and every field above `error_code` is off by eight, so the handler
/// reads the vector as an error code and returns through whatever `iretq`
/// finds. The number lives in [`Vector`], so the slot a stub is installed in
/// and the number it pushes cannot disagree.
macro_rules! exception_stub {
    ($stub:ident, $variant:ident, no_error_code) => {
        #[unsafe(naked)]
        extern "sysv64" fn $stub() {
            naked_asm!(
                "push 0",
                "push {vector}",
                "jmp {common}",
                vector = const Vector::$variant as usize,
                common = sym common_entry,
            );
        }
    };
    ($stub:ident, $variant:ident, error_code) => {
        #[unsafe(naked)]
        extern "sysv64" fn $stub() {
            naked_asm!(
                "push {vector}",
                "jmp {common}",
                vector = const Vector::$variant as usize,
                common = sym common_entry,
            );
        }
    };
}

/// Declares the IDT: the vector numbers, their stubs, and the one function
/// that installs them.
///
/// One table, because the three statements a gate is made of have to agree and
/// nothing else makes them. A `dispatched` vector gets a generated stub, a slot
/// in [`install_gates`], and an arm in [`Vector::from_raw`] — so a gate the
/// dispatcher does not know cannot be installed, which matters because
/// `from_raw` runs on the crash path and that path may not panic.
///
/// A `direct` vector is its own naked entry and never reaches
/// [`trap_dispatch`]: the device IRQs, the halt IPI, the shootdown IPI, and the
/// NMI, whose handler must not touch the preempt count or reschedule.
macro_rules! idt_vectors {
    (
        dispatched { $($ex:ident = $exnum:literal, $stub:ident, $err:ident $(, ist $ist:literal)?;)* }
        direct { $($direct:ident = $dnum:literal, $entry:path;)* }
    ) => {
        /// IDT vector assignments — CPU exceptions and hardware interrupts.
        #[repr(usize)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Vector {
            $($ex = $exnum,)*
            $($direct = $dnum,)*
        }

        impl Vector {
            /// The number a stub pushed. Total over every dispatched gate, so
            /// the arm below is reachable only from a vector this module does
            /// not install — which the CPU cannot deliver.
            fn from_raw(v: u64) -> Self {
                match v {
                    $($exnum => Self::$ex,)*
                    _ => panic!("dispatch on vector {:#x}, which has no dispatched gate", v),
                }
            }
        }

        $(exception_stub!($stub, $ex, $err);)*

        fn install_gates(idt: &mut Idt) {
            $(
                idt.entries[Vector::$ex as usize] =
                    IdtEntry::new($stub as *const () as u64)$(.with_ist($ist))?;
            )*
            $(
                idt.entries[Vector::$direct as usize] =
                    IdtEntry::new($entry as *const () as u64);
            )*
        }
    };
}

// Every vector Intel names for 64-bit mode has a gate, because a vector without
// one does not fault the process: the CPU takes the missing gate as a second,
// contributory fault and escalates to #DF, which halts the machine. A userland
// `div` by zero did exactly that.
//
// The ones Intel reserves — 9, 15 and 22..=31 — are left out on purpose:
// nothing can deliver them, and `from_raw`'s panic is the honest answer if one
// ever arrives. Every gate is DPL 0, so `int n` from Ring 3 raises #GP against
// the gate rather than entering it.
idt_vectors! {
    dispatched {
        DivideError        = 0x00, stub_de, no_error_code;
        Debug              = 0x01, stub_db, no_error_code;
        Breakpoint         = 0x03, stub_bp, no_error_code;
        Overflow           = 0x04, stub_of, no_error_code;
        BoundRange         = 0x05, stub_br, no_error_code;
        InvalidOpcode      = 0x06, stub_ud, no_error_code;
        DeviceNotAvailable = 0x07, stub_nm, no_error_code;
        DoubleFault        = 0x08, stub_df, error_code, ist 1;
        InvalidTss         = 0x0A, stub_ts, error_code;
        SegmentNotPresent  = 0x0B, stub_np, error_code;
        StackSegment       = 0x0C, stub_ss, error_code;
        GeneralProtection  = 0x0D, stub_gp, error_code;
        PageFault          = 0x0E, stub_pf, error_code;
        X87FloatingPoint   = 0x10, stub_mf, no_error_code;
        AlignmentCheck     = 0x11, stub_ac, error_code;
        MachineCheck       = 0x12, stub_mc, no_error_code;
        SimdFloatingPoint  = 0x13, stub_xm, no_error_code;
        Virtualization     = 0x14, stub_ve, no_error_code;
        ControlProtection  = 0x15, stub_cp, error_code;
    }
    direct {
        // Diagnostic only, and sent by `sched::dump` alone — see `idt/nmi.rs`.
        Nmi          = 0x02, nmi::nmi_entry;
        Timer        = 0x20, timer::timer_entry;
        Xhci         = 0x21, xhci::xhci_entry;
        VirtioNet    = 0x22, virtio_net::virtio_net_entry;
        VirtioSound  = 0x23, virtio_sound::virtio_sound_entry;
        I8042        = 0x24, i8042::i8042_entry;
        DmaFault     = 0x25, dma_fault::dma_fault_entry;
        Hda          = 0x26, hda::hda_entry;
        HaltAll      = 0xFD, stub_halt_all;
        TlbFlush     = 0xFE, tlb::tlb_flush_entry;
    }
}

/// Halt IPI — received when another CPU calls halt_all_cpus(). Never returns.
#[unsafe(naked)]
extern "sysv64" fn stub_halt_all() {
    naked_asm!("cli", "2: hlt", "jmp 2b");
}

/// Every exception vector's second half, #PF included.
///
/// It reaches [`kernel_exit_to_user_check`] and therefore `do_preempt`, so a
/// fault taken from Ring 3 can return through another task — and until this
/// bracket existed it did so carrying whatever that task left in the registers.
/// A demand-paging fault corrupting XMM produces a wrong number rather than a
/// signal, which is why nothing had noticed (`specs/user-machine-state.md` §3).
///
/// `rdi` is taken before the bracket because the bracket moves `rsp`: the frame
/// [`trap_dispatch`] is handed is the one the pushes above built, and the CS
/// test after the call reads it back out of the bracket's stash.
#[unsafe(naked)]
extern "sysv64" fn common_entry() {
    ring3_naked_asm!(
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",
        "lock add dword ptr gs:[240], 1",
        "mov rdi, rsp",
        save_user_state!(),
        "call {dispatch}",
        "lock sub dword ptr gs:[240], 1",
        // Run exit-to-user epilogue before restoring GPRs — the call clobbers
        // scratch regs, which would otherwise leak kernel state into user.
        "mov r11, [rsp + {fp_bytes}]",
        "test dword ptr [r11 + 144], 3",
        "jz 9f",
        "cli",
        "call {exit_to_user}",
        "9:",
        restore_user_state!(),
        "pop rax",  "pop rbx",  "pop rcx",  "pop rdx",
        "pop rsi",  "pop rdi",  "pop rbp",
        "pop r8",   "pop r9",   "pop r10",  "pop r11",
        "pop r12",  "pop r13",  "pop r14",  "pop r15",
        "add rsp, 16",
        "iretq",
        dispatch = sym trap_dispatch,
        exit_to_user = sym kernel_exit_to_user_check,
    );
}

/// Deferred-preempt epilogue. Caller must have IF=0 on entry; returns IF=0.
/// Briefly enables interrupts only inside the yield, so the final
/// iretq/sysretq stays race-free without each caller juggling IF itself.
///
/// `do_preempt` owns the `need_resched` clear (see its doc) — clearing here
/// would silently drop requests its re-entry guard defers. A request that
/// survives `do_preempt` on this path means the IN_SCHEDULE guard leaked;
/// spinning on it would hang the CPU silently, so die loudly instead.
pub(crate) extern "sysv64" fn kernel_exit_to_user_check() {
    flush_ring0_timer_fires_to_trace();
    while crate::preempt::need_resched() {
        assert!(!crate::scheduler::in_schedule_self(),
            "exit-to-user inside a scheduler pass");
        unsafe { core::arch::asm!("sti", options(nomem, nostack)); }
        crate::scheduler::do_preempt();
        unsafe { core::arch::asm!("cli", options(nomem, nostack)); }
        flush_ring0_timer_fires_to_trace();
    }
}

fn flush_ring0_timer_fires_to_trace() {
    let cur: u32;
    let last: u32;
    unsafe {
        core::arch::asm!(
            "mov {cur:e}, gs:[248]",
            "mov {last:e}, gs:[252]",
            cur = out(reg) cur,
            last = out(reg) last,
            options(nomem, nostack, preserves_flags),
        );
    }
    let missed = cur.wrapping_sub(last);
    if missed > 0 {
        crate::trace::trace(crate::trace::Kind::TimerFireBurst, missed);
        unsafe {
            core::arch::asm!(
                "mov gs:[252], {cur:e}",
                cur = in(reg) cur,
                options(nomem, nostack, preserves_flags),
            );
        }
    }
}

/// Rust exception dispatcher — routes by vector to the appropriate handler.
///
/// The default arm is every other fault, and it is the one that decides on the
/// saved CS: from Ring 3 the process dies named, from Ring 0 the kernel says so
/// and halts. The three ahead of it are the exceptions that are not that —
/// #DB resumes, and #DF and #MC are aborts with no instruction to return to.
extern "sysv64" fn trap_dispatch(frame: *mut TrapFrame) {
    let frame = unsafe { &mut *frame };
    match Vector::from_raw(frame.vector) {
        Vector::Debug => exceptions::debug_handler(frame),
        Vector::DoubleFault => exceptions::double_fault_handler(frame),
        Vector::MachineCheck => exceptions::machine_check_handler(frame),
        Vector::PageFault => {
            cpu::enable_interrupts();
            exceptions::page_fault_handler(frame);
            unsafe { core::arch::asm!("cli", options(nomem, nostack)); }
        }
        _ => exceptions::exception_handler(frame),
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

    install_gates(&mut IDT.lock());

    let ptr = IdtPointer {
        limit: (core::mem::size_of::<Idt>() - 1) as u16,
        base: IDT.data_ptr() as u64,
    };

    unsafe {
        cpu::lidt(&ptr as *const IdtPointer as *const u8);
    }
}

/// Take IF=1 on this CPU. Split from `init` so `ioapic::init` can mask every
/// entry firmware left behind while exception handlers are already installed:
/// an unmasked entry aimed at a vector with no gate would otherwise become a
/// #GP the moment the boot enables interrupts.
pub fn enable_interrupts() {
    cpu::enable_interrupts();
}

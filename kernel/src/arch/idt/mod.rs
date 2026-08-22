pub(crate) mod exceptions;
mod device_irq;
mod dma_fault;
mod hda;
mod i8042;
#[cfg(feature = "boot-actuators")]
mod log_nest;
mod nmi;
mod timer;
mod tlb;
mod virtio_net;
mod virtio_sound;
mod xhci;

use core::arch::naked_asm;

use super::cpu;
use super::entry::{
    restore_user_state, ring3_naked_asm, save_user_state, Ring0Entry, Ring3Entry,
};
use super::cpu::{outb, io_wait};
use super::percpu;
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

/// The vector `log-nested-emit` sends itself (§9.2), and the one gate that is
/// not in the table below.
///
/// **It is installed only in a kernel built with `boot-actuators`**, after
/// `install_gates`, because nothing but that actuator can ever raise it: a
/// shipping IDT with a gate no interrupt reaches is a gate nothing deletes. It
/// is `direct` in every sense the table means — its own entry, never
/// `trap_dispatch` — and it sits one past the last device vector.
#[cfg(feature = "boot-actuators")]
pub const LOG_NEST_VECTOR: u8 = 0x27;

// Page fault error code bits
const PF_PRESENT: u64 = 1 << 0;
const PF_WRITE: u64 = 1 << 1;
const PF_INSTRUCTION_FETCH: u64 = 1 << 4;

// The ring a `cs` names is `toyos_userbound::Ring`'s to decide and nobody
// else's: a second reading of the RPL field beside it is a second place the
// crash path can be told the wrong privilege level.

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

    /// A gate for a handler that can reach another task.
    fn ring3(entry: Ring3Entry) -> Self {
        Self::at(entry.addr())
    }

    /// A gate for one that cannot.
    fn ring0(entry: Ring0Entry) -> Self {
        Self::at(entry.addr())
    }

    /// Private, so no slot can be filled by a pointer nobody classified.
    fn at(handler: u64) -> Self {
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
///
/// Each `direct` row also answers whether its handler can reach another task
/// before returning to Ring 3 — `ring3` if it can, and it must therefore
/// bracket the user machine state (`arch::entry`), `ring0` if it cannot. There
/// is no third spelling and no default, for the same reason the error-code
/// column has none. Every `dispatched` vector goes through `common_entry`,
/// which brackets, so their rows do not repeat the answer.
///
/// **The `ist` column is a claim about `rsp` and not about depth**: a vector
/// that carries one takes its frame on the stack `percpu::IST_STACKS` names
/// whatever `rsp` holds, which is the only answer for a vector that can arrive
/// while `rsp` is not a kernel stack at all. Both kinds of row may have one, and
/// [`IST_VECTORS`] is what the assertions below read.
macro_rules! idt_vectors {
    (
        dispatched { $($ex:ident = $exnum:literal, $stub:ident, $err:ident $(, ist $ist:literal)?;)* }
        direct { $($ring:tt $direct:ident = $dnum:literal, $entry:path $(, ist $dist:literal)?;)* }
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

        /// Every vector whose row carries an `ist` index, and the index.
        ///
        /// Read by the assertions under the table: which vectors need one is a
        /// property of where they can arrive, so it is stated once, checked at
        /// compile time, and never re-derived from the entries at runtime.
        const IST_VECTORS: &[(usize, u8)] = &[
            $($((Vector::$ex as usize, $ist),)?)*
            $($((Vector::$direct as usize, $dist),)?)*
        ];

        fn install_gates(idt: &mut Idt) {
            $(
                idt.entries[Vector::$ex as usize] =
                    IdtEntry::ring3(Ring3Entry::new($stub))$(.with_ist($ist))?;
            )*
            $(
                idt.entries[Vector::$direct as usize] =
                    direct_gate!($ring, $entry)$(.with_ist($dist))?;
            )*
        }
    };
}

/// The two answers a `direct` row may give, and the whole of what each means.
macro_rules! direct_gate {
    (ring3, $entry:path) => {
        IdtEntry::ring3(Ring3Entry::new($entry))
    };
    (ring0, $entry:path) => {
        IdtEntry::ring0(Ring0Entry::declare($entry))
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
        MachineCheck       = 0x12, stub_mc, no_error_code, ist 3;
        SimdFloatingPoint  = 0x13, stub_xm, no_error_code;
        Virtualization     = 0x14, stub_ve, no_error_code;
        ControlProtection  = 0x15, stub_cp, error_code;
    }
    direct {
        // Diagnostic only, and sent by `sched::dump` alone — see `idt/nmi.rs`.
        // Ring 0 because it arrives between arbitrary instructions, including
        // inside another entry's own save, and it reschedules nothing. IST2
        // because "arbitrary instructions" includes the three of `SYSCALL` entry
        // that run at CPL 0 on the user's stack, where a frame pushed at `rsp`
        // is a supervisor write to a user page: SMAP refuses it, the `#PF` lands
        // on the same stack, and the machine takes a `#DF`.
        ring0 Nmi          = 0x02, nmi::nmi_entry, ist 2;
        ring3 Timer        = 0x20, timer::timer_entry;
        ring3 Xhci         = 0x21, xhci::xhci_entry;
        ring3 VirtioNet    = 0x22, virtio_net::virtio_net_entry;
        ring3 VirtioSound  = 0x23, virtio_sound::virtio_sound_entry;
        ring3 I8042        = 0x24, i8042::i8042_entry;
        ring3 DmaFault     = 0x25, dma_fault::dma_fault_entry;
        ring3 Hda          = 0x26, hda::hda_entry;
        // Ring 0 because it never returns: `cli; hlt` forever.
        ring0 HaltAll      = 0xFD, stub_halt_all;
        ring3 TlbFlush     = 0xFE, tlb::tlb_flush_entry;
    }
}

/// The three vectors that can arrive while `rsp` is not a kernel stack, and
/// therefore the three that must carry an IST index.
///
/// **Checked here rather than tested, because a missing index is invisible
/// until the machine is already dying.** `SYSCALL` switches CPL and `RIP` and
/// nothing else, so `arch::syscall`'s entry runs three instructions at CPL 0 on
/// the user's stack and its exit one more between `pop rsp` and `sysretq`; a
/// frame the CPU builds there is a supervisor write to a user page, SMAP refuses
/// it, and the `#PF` escalates to `#DF` (measured 2026-08-22 with `TF`, then
/// masked). `#DF` is on the list because it is where that escalation lands, NMI
/// because nothing masks it, `#MC` because an abort with no report is a machine
/// that went down saying nothing.
///
/// A vector *without* an IST is not asserted about: an ordinary fault from Ring
/// 3 arrives after the CPU has already switched to `tss.rsp0`, and one from
/// Ring 0 arrives on a kernel stack by definition.
const _: () = {
    const fn ist_of(vector: usize) -> u8 {
        let mut i = 0;
        while i < IST_VECTORS.len() {
            if IST_VECTORS[i].0 == vector {
                return IST_VECTORS[i].1;
            }
            i += 1;
        }
        0
    }
    assert!(ist_of(0x08) == 1, "#DF must take its frame on IST1");
    assert!(ist_of(0x02) == 2, "an NMI must take its frame on IST2");
    assert!(ist_of(0x12) == 3, "#MC must take its frame on IST3");
    // Every index names a stack `percpu::alloc_ist_stacks` actually allocates,
    // and no two vectors share one: an IST stack is not re-entrant, so two
    // vectors on one index is a frame written over another frame.
    let mut i = 0;
    while i < IST_VECTORS.len() {
        let (_, ist) = IST_VECTORS[i];
        assert!(ist >= 1 && ist as usize <= percpu::IST_STACKS, "no stack for that IST index");
        let mut j = i + 1;
        while j < IST_VECTORS.len() {
            assert!(IST_VECTORS[j].1 != ist, "two vectors share one IST stack");
            j += 1;
        }
        i += 1;
    }
};

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
/// signal, which is why nothing had noticed.
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
///
/// **The kill is checked at the *last* boundary and not the first, and the
/// difference is a thread reaching Ring 3 after it was killed.** The loop below
/// re-enables interrupts and gives this CPU away for a whole pass; a retire
/// landing in that window — and the retire's kick is a targeted IPI aimed at
/// exactly this CPU — was observed by nothing, because the one check had
/// already run. So the check is the loop's own condition: it runs before every
/// pass and again after the last one, with IF=0, and the return to Ring 3 is
/// the statement immediately after it.
///
/// What remains is one instant wide and not one quantum: the kill bit is set
/// by a remote CPU's plain atomic, so it can be raised between this check and
/// the `iretq`. The bound that leaves is one interrupt delivery — the retire's
/// `Urgency::Preempt` kick is already on its way, and the thread takes it in
/// Ring 3 and comes straight back here.
pub(crate) extern "sysv64" fn kernel_exit_to_user_check() {
    flush_ring0_timer_fires_to_trace();
    loop {
        // A killed thread returns to Ring 3 exactly once more: never. Its
        // kernel stack is empty here by definition, so this is where the
        // unwind ends.
        crate::scheduler::exit_if_killed();
        if !crate::preempt::need_resched() {
            return;
        }
        assert!(!crate::scheduler::in_schedule_self(),
            "exit-to-user inside a scheduler pass");
        // The pair is `arch::cpu`'s, which is that exact `sti`/`cli` with those
        // exact options — not an `IrqGuard`, because both of this loop's exits
        // have to *set* IF rather than restore whatever the caller had.
        cpu::enable_interrupts();
        crate::scheduler::do_preempt();
        cpu::disable_interrupts();
        flush_ring0_timer_fires_to_trace();
    }
}

fn flush_ring0_timer_fires_to_trace() {
    let cur = percpu::ring0_timer_fires();
    let missed = cur.wrapping_sub(percpu::last_seen_ring0_fires());
    if missed > 0 {
        crate::trace::trace(crate::trace::Kind::TimerFireBurst, missed);
        percpu::set_last_seen_ring0_fires(cur);
    }
}

/// Rust exception dispatcher — routes by vector to the appropriate handler.
///
/// The default arm is every other fault, and it is the one that decides on the
/// saved CS: from Ring 3 the process dies named, from Ring 0 the kernel says so
/// and halts. The two ahead of it are the exceptions that are not that — #DF
/// and #MC are aborts with no instruction to return to.
///
/// **#DB is on the default arm and used to have one of its own.** Vector 1 is
/// reachable from Ring 3 by two ordinary instruction sequences — `INT1`, which
/// is not subject to `INT n`'s DPL check against the gate, and an `RFLAGS.TF`
/// a `popfq` sets — so it is a userland bug like `#BP` and `#UD` and ends the
/// process the same way. It used to reach a debugger-session aid that logged a
/// register dump, disarmed `DR7`/`DR6` and *returned to resume*: a Ring 3 trap
/// walked kernel state and then carried on, and with `TF` still set it did so
/// once per instruction for as long as the process ran. Nothing arms a
/// watchpoint in this kernel — `arch::debug`'s tools were deleted before this —
/// so the handler had no other caller and went with them. The gate stays: a
/// vector without one escalates to `#DF`, which halts the machine.
extern "sysv64" fn trap_dispatch(frame: *mut TrapFrame) {
    #[cfg(feature = "df-witness")]
    crate::arch::cpu::df_witness("trap_dispatch");
    // SAFETY: `frame` is `rsp` at the moment `common_entry` finished its pushes,
    // handed straight to this call — so it points at the `TrapFrame` those
    // pushes and the CPU's own interrupt frame just built, on this CPU's kernel
    // stack, which nothing else can be holding a reference to. Irreducible
    // because the frame is built by naked assembly and its `&mut` is the only
    // way a handler can write `rip`/`rsp` back for the `iretq`.
    let frame = unsafe { &mut *frame };
    match Vector::from_raw(frame.vector) {
        Vector::DoubleFault => exceptions::double_fault_handler(frame),
        Vector::MachineCheck => exceptions::machine_check_handler(frame),
        Vector::PageFault => {
            cpu::enable_interrupts();
            exceptions::page_fault_handler(frame);
            cpu::disable_interrupts();
        }
        _ => exceptions::exception_handler(frame),
    }
}

/// Disable the legacy 8259 PIC.
///
/// The four ports at the top of this file are the pair's fixed architectural
/// addresses and the only I/O ports this module names.
fn disable_pic() {
    // SAFETY: `outb` asks its caller to own the port and the byte it sends.
    // Every port here is one of the 8259 pair's four, which no other device
    // decodes on any machine this kernel targets; the bytes are the documented
    // ICW1..ICW4 initialisation followed by `0xFF` in both mask registers, which
    // silences the device rather than arming it, and none of them reaches memory.
    //
    // **One block, because the sequence is the safety argument.** A PIC left
    // half-initialised keeps delivering its power-on vectors — 8..15 and 112..119
    // — into an IDT whose gates at those numbers belong to `#DF`, `#GP`, `#PF`
    // and the rest, and there is no point between these writes where that is a
    // state the machine may be left in.
    unsafe {
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
}

pub fn init() {
    disable_pic();

    install_gates(&mut IDT.lock());
    #[cfg(feature = "boot-actuators")]
    install_actuator_gates(&mut IDT.lock());
    // **The kernel this tree had until 2026-08-22**, and the negative control on
    // vector 2's `ist 2`: the gate keeps its handler and its ring and loses the
    // one byte that decides which stack the CPU builds the frame on.
    // Nothing on the host side can reach that state — the IDT is the guest's own
    // memory, and no QEMU device or machine property edits it.
    #[cfg(feature = "boot-actuators")]
    if crate::actuator::nmi_without_ist() {
        IDT.lock().entries[Vector::Nmi as usize].ist = 0;
    }

    let ptr = IdtPointer {
        limit: (core::mem::size_of::<Idt>() - 1) as u16,
        base: IDT.data_ptr() as u64,
    };

    // SAFETY: `lidt` asks for a valid IDT descriptor. `ptr` is one, built two
    // lines above out of `size_of::<Idt>()` and the address of the `static IDT`
    // this function has just filled — a `'static` whose entries `install_gates`
    // wrote from the one table, so every slot is either a classified handler or
    // `IdtEntry::EMPTY`. The descriptor itself is a local, which is what `lidt`
    // wants: the CPU copies the base and limit out of it.
    unsafe {
        cpu::lidt(&ptr as *const IdtPointer as *const u8);
    }
}

/// The one gate outside the table, and the reason it is outside it: nothing but
/// an actuator raises [`LOG_NEST_VECTOR`], so a shipping kernel installs no
/// entry for it and has no handler to install.
#[cfg(feature = "boot-actuators")]
fn install_actuator_gates(idt: &mut Idt) {
    idt.entries[LOG_NEST_VECTOR as usize] =
        IdtEntry::ring3(Ring3Entry::new(log_nest::log_nest_entry));
}

/// Take IF=1 on this CPU. Split from `init` so `ioapic::init` can mask every
/// entry firmware left behind while exception handlers are already installed:
/// an unmasked entry aimed at a vector with no gate would otherwise become a
/// #GP the moment the boot enables interrupts.
pub fn enable_interrupts() {
    cpu::enable_interrupts();
}

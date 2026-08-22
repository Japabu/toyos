use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicU32, AtomicU8};

use alloc::alloc::alloc_zeroed;
use core::alloc::Layout;

use super::cpu;
use crate::log;

const MSR_GS_BASE: u32 = 0xC000_0101;

// GDT selectors (must match entry order)
pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;
/// The two selectors a thread runs userland with. RPL 3 is part of the value.
pub const USER_DS: u16 = 0x1B;
pub const USER_CS: u16 = 0x23;
const TSS_SEL: u16 = 0x28;

/// `STAR[63:48]`, which `SYSRET` derives both user selectors from: SS is this
/// plus 8 and CS is this plus 16.
///
/// **RPL 3 belongs in this value rather than to the CPU, because the two
/// vendors disagree about who supplies it.** Intel's SDM forces it into both —
/// SYSRET's operation reads `SS.Selector := (IA32_STAR[63:48]+8) OR 3` — while
/// AMD's APM forces it into CS alone and takes SS's straight from this field.
/// So a bare [`KERNEL_DS`] here runs every user thread on an AMD machine with
/// `SS = 0x18`, and the first interrupt taken from one dies on the handler's
/// `iretq`: a return to an outer privilege level requires `SS.RPL == CS.RPL`,
/// and 0 is not 3. `#GP(0x18)`, naming the selector.
pub const STAR_SYSRET_BASE: u16 = USER_DS - 8;
const _: () = assert!(STAR_SYSRET_BASE + 8 == USER_DS);
const _: () = assert!(STAR_SYSRET_BASE + 16 == USER_CS);

/// 64-bit TSS (104 bytes).
#[repr(C, packed)]
pub struct Tss {
    reserved0: u32,
    pub rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iopb_offset: u16,
}

impl Tss {
    const fn new() -> Self {
        Self {
            reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            iopb_offset: size_of::<Tss>() as u16,
        }
    }
}

/// Per-CPU fault state machine. Encodes the escalation policy for nested faults.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CpuFaultState {
    Normal = 0,
    PageFault = 1,   // demand paging in progress
    Fatal = 2,       // fatal exception handler running
    Panic = 3,       // panic handler running
}

/// Per-CPU data. Accessed via GS segment in kernel mode.
/// Field offsets are hardcoded in assembly — do not reorder.
#[repr(C)]
pub struct PerCpu {
    self_ptr: u64,      // offset 0: points to self (for gs:0 self-reference)
    cpu_id: u32,        // offset 8
    lapic_id: u32,      // offset 12
    pub kernel_rsp: u64, // offset 16: syscall entry loads this as kernel stack
    pub user_rsp: u64,   // offset 24: syscall entry saves user RSP here
    pub tss: Tss,        // offset 32 (104 bytes)
    current_tid: u32,    // offset 136: TID of thread running on this CPU (u32::MAX = idle)
    current_pid: u32,    // offset 140: PID of process running on this CPU (u32::MAX = idle)
    gdt: [u64; 7],      // offset 144 (56 bytes)
    idle_rsp: u64,       // offset 200: saved RSP for idle context (for context_switch)
    idle_stack_top: u64, // offset 208: top of per-CPU idle stack
    /// Saved user RIP at last syscall entry (for panic diagnostics).
    pub syscall_rip: u64,  // offset 216
    /// Saved syscall number (for panic diagnostics).
    pub syscall_num: u64,  // offset 224
    /// Saved user RBP at last syscall entry (for panic diagnostics).
    pub syscall_rbp: u64,  // offset 232
    /// `lock add/sub` because IRQ entry/exit and Rust kernel code mutate it
    /// on the same CPU.
    pub preempt_count: AtomicU32,          // offset 240
    pub need_resched: AtomicU8,            // offset 244
    _pad245: [u8; 3],                      // offset 245..248
    /// Writes use plain `inc`: only the Ring 0 timer stub writes, with IF=0.
    pub ring0_timer_fires: AtomicU32,      // offset 248
    pub last_seen_ring0_fires: u32,        // offset 252
    fault_state: u8,                       // offset 256
    _pad257: [u8; 3],                      // offset 257..260
    /// Ticks the Ring 0 timer asm re-arms with (gs:[260]). Per-CPU: one-shot
    /// timers are armed independently on every CPU; a shared value would let
    /// any CPU's arm/stop clobber every other CPU's re-arm fallback.
    pub last_armed_ticks: AtomicU32,       // offset 260
    /// This CPU's [`log::Shard`], reached by [`reserve_log_slot`].
    ///
    /// **Never null on a live CPU**: [`alloc_percpu`] fills it for cpu0 and for
    /// every AP, and the BSP allocates an AP's whole `PerCpu` before that AP
    /// executes an instruction. That is why `emit` needs no check — an absent
    /// shard is not a state this field can be in.
    log_shard: u64,                        // offset 264
    /// Non-zero while this CPU is inside its NMI handler, written by
    /// `arch::idt::nmi`'s entry and by nothing else.
    ///
    /// **The checked half of a proof.** IST2 is not re-entrant, so a second NMI
    /// entered while the first is still on that stack would write its frame over
    /// the first's; the architecture blocks NMI delivery from entry until the
    /// handler's `iretq` (SDM Vol. 3A §6.7.1), and the handler cannot fault, so
    /// no such entry exists — and this word is what turns that argument into an
    /// observation rather than an assumption.
    nmi_active: u32,                       // offset 272
    _pad276: [u8; 4],                      // offset 276..280
}

// GDT layout:
//   0x00: null
//   0x08: kernel code64 (DPL=0)
//   0x10: kernel data   (DPL=0)
//   0x18: user data     (DPL=3)
//   0x20: user code64   (DPL=3)
//   0x28: TSS low       (filled at init)
//   0x30: TSS high      (filled at init)
const GDT_ENTRIES: [u64; 7] = [
    0x0000_0000_0000_0000, // null
    0x00AF_9A00_0000_FFFF, // kernel code64
    0x00CF_9200_0000_FFFF, // kernel data
    0x00CF_F200_0000_FFFF, // user data
    0x00AF_FA00_0000_FFFF, // user code64
    0,                      // TSS low (runtime)
    0,                      // TSS high (runtime)
];

#[repr(C, packed)]
struct GdtPointer {
    limit: u16,
    base: u64,
}

impl PerCpu {
    /// Build the TSS descriptor and write it into gdt[5..7].
    fn init_tss_descriptor(&mut self) {
        let tss_addr = &self.tss as *const Tss as u64;
        let tss_limit = (size_of::<Tss>() - 1) as u64;

        let low = (tss_limit & 0xFFFF)
            | ((tss_addr & 0xFFFF) << 16)
            | (((tss_addr >> 16) & 0xFF) << 32)
            | (0x89u64 << 40)
            | (((tss_limit >> 16) & 0xF) << 48)
            | (((tss_addr >> 24) & 0xFF) << 56);
        let high = tss_addr >> 32;

        self.gdt[5] = low;
        self.gdt[6] = high;
    }

    /// Load this CPU's GDT, reload segment registers, and load TSS.
    ///
    /// # Safety
    /// Must be called exactly once per CPU during init.
    unsafe fn load_gdt(&self) {
        let ptr = GdtPointer {
            limit: (size_of::<[u64; 7]>() - 1) as u16,
            base: self.gdt.as_ptr() as u64,
        };

        core::arch::asm!(
            "lgdt [{}]",
            "push {cs}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {ds:x}",
            "mov es, {ds:x}",
            "mov fs, {ds:x}",
            // Skip GS — its base is managed via IA32_GS_BASE MSR.
            // Writing the selector would zero the cached base.
            "mov ss, {ds:x}",
            in(reg) &ptr,
            cs = in(reg) KERNEL_CS as u64,
            ds = in(reg) KERNEL_DS as u64,
            tmp = lateout(reg) _,
        );

        cpu::ltr(TSS_SEL);
    }
}

// Where each field this kernel reaches through `gs:` sits inside `PerCpu`.
//
// **Derived from the type and asserted against the number the assembly
// hardcodes.** `arch::entry`'s stubs, `arch::syscall`'s entry and the Ring 0
// timer stub all write the displacement as a literal, so the assertion is the
// whole of what keeps the two sides in step — a reordered or resized field
// would otherwise move only the Rust half. Every GS access written in Rust
// names one of these constants; none of them names a number.
const OFF_SELF_PTR: u32 = offset_of!(PerCpu, self_ptr) as u32;
const OFF_CPU_ID: u32 = offset_of!(PerCpu, cpu_id) as u32;
const OFF_USER_RSP: u32 = offset_of!(PerCpu, user_rsp) as u32;
const OFF_CURRENT_TID: u32 = offset_of!(PerCpu, current_tid) as u32;
const OFF_CURRENT_PID: u32 = offset_of!(PerCpu, current_pid) as u32;
const OFF_IDLE_STACK_TOP: u32 = offset_of!(PerCpu, idle_stack_top) as u32;
const OFF_SYSCALL_RIP: u32 = offset_of!(PerCpu, syscall_rip) as u32;
const OFF_SYSCALL_NUM: u32 = offset_of!(PerCpu, syscall_num) as u32;
const OFF_SYSCALL_RBP: u32 = offset_of!(PerCpu, syscall_rbp) as u32;
const OFF_RING0_TIMER_FIRES: u32 = offset_of!(PerCpu, ring0_timer_fires) as u32;
const OFF_LAST_SEEN_RING0_FIRES: u32 = offset_of!(PerCpu, last_seen_ring0_fires) as u32;
const OFF_LAST_ARMED_TICKS: u32 = offset_of!(PerCpu, last_armed_ticks) as u32;
/// `arch::syscall`'s and `arch::idt`'s entry stubs open and close this count in
/// naked assembly, and `preempt` is the Rust half of the same word.
pub(crate) const OFF_PREEMPT_COUNT: u32 = offset_of!(PerCpu, preempt_count) as u32;
/// Set by the timer ISR, cleared by the deferred-preempt epilogue — `preempt`.
pub(crate) const OFF_NEED_RESCHED: u32 = offset_of!(PerCpu, need_resched) as u32;
/// Read by `preempt::enable`'s slow path, which declines to reschedule a CPU
/// that is inside a fault or panic report.
pub(crate) const OFF_FAULT_STATE: u32 = offset_of!(PerCpu, fault_state) as u32;
/// Set and cleared by `arch::idt::nmi`'s naked entry, which is the only writer
/// and hardcodes this displacement like every other stub.
pub(crate) const OFF_NMI_ACTIVE: u32 = offset_of!(PerCpu, nmi_active) as u32;

const _: () = assert!(OFF_SELF_PTR == 0);
const _: () = assert!(OFF_CPU_ID == 8);
const _: () = assert!(offset_of!(PerCpu, kernel_rsp) == 16);
const _: () = assert!(OFF_USER_RSP == 24);
const _: () = assert!(offset_of!(PerCpu, tss) == 32);
const _: () = assert!(OFF_CURRENT_TID == 136);
const _: () = assert!(OFF_CURRENT_PID == 140);
const _: () = assert!(OFF_IDLE_STACK_TOP == 208);
const _: () = assert!(OFF_SYSCALL_RIP == 216);
const _: () = assert!(OFF_SYSCALL_NUM == 224);
const _: () = assert!(OFF_SYSCALL_RBP == 232);
const _: () = assert!(OFF_PREEMPT_COUNT == 240);
const _: () = assert!(OFF_NEED_RESCHED == 244);
const _: () = assert!(OFF_RING0_TIMER_FIRES == 248);
const _: () = assert!(OFF_LAST_SEEN_RING0_FIRES == 252);
const _: () = assert!(OFF_FAULT_STATE == 256);
const _: () = assert!(OFF_LAST_ARMED_TICKS == 260);
const _: () = assert!(offset_of!(PerCpu, log_shard) == 264);
const _: () = assert!(OFF_NMI_ACTIVE == 272);

/// Every GS-relative access this kernel makes, as `const`-generic primitives.
///
/// **Here because this module owns the layout they index into.** The offsets
/// above are declared and asserted a few lines up, the entry stubs hardcode the
/// same numbers, and a GS access written anywhere else is a third place that has
/// to know both. `preempt` carried six of these of its own, spelled against
/// three hand-copied literals; they are these, and its literals are now
/// `OFF_PREEMPT_COUNT`, `OFF_NEED_RESCHED` and `OFF_FAULT_STATE`.
///
/// The offset is a `const` operand, so each still assembles to the
/// immediate-displacement form a hand-written `asm!` string produced —
/// `mov %gs:8, %eax`, `lock addl $1, %gs:240` — and no register is spent
/// reaching it.
///
/// **What none of them can check, and every caller owes**: `GS_BASE` must
/// already point at this CPU's `PerCpu`. [`init_bsp`] writes it on the BSP and
/// the trampoline in `arch::smp` writes it before an AP executes any Rust;
/// `preempt`'s callers ask `percpu_ready()` first, because they run on the BSP
/// before `init_bsp` does.
///
/// Each is irreducible in the same way: a GS-relative access is a machine
/// facility with no Rust operation behind it, and `PerCpu` cannot be a `static`
/// because every CPU needs a different one under the same name.
pub(crate) mod gs {
    use core::arch::asm;

    /// One naturally aligned per-CPU `u64` load.
    #[inline]
    pub fn read_u64<const OFF: u32>() -> u64 {
        let v: u64;
        // SAFETY: `OFF` is one of this module's asserted field offsets, so
        // `GS_BASE + OFF` is a live, naturally aligned word of this CPU's own
        // `PerCpu` once the caller's half of the contract above holds. `nomem`
        // because the access reaches no memory any Rust value names;
        // `preserves_flags` because `mov` writes none.
        unsafe {
            asm!("mov {v}, gs:[{off}]", v = out(reg) v, off = const OFF,
                options(nomem, nostack, preserves_flags));
        }
        v
    }

    /// One naturally aligned per-CPU `u32` load.
    #[inline]
    pub fn read_u32<const OFF: u32>() -> u32 {
        let v: u32;
        // SAFETY: `read_u64`'s argument, for four bytes.
        unsafe {
            asm!("mov {v:e}, gs:[{off}]", v = out(reg) v, off = const OFF,
                options(nomem, nostack, preserves_flags));
        }
        v
    }

    /// One naturally aligned per-CPU `u32` store. No `lock` prefix: a 32-bit
    /// store to an aligned address is atomic on x86, and a same-CPU IRQ cannot
    /// land inside one instruction.
    #[inline]
    pub fn write_u32<const OFF: u32>(v: u32) {
        // SAFETY: `read_u64`'s argument, minus `nomem` — this one does write the
        // word, and every other writer of it is on this CPU, so the store's own
        // atomicity is the whole of the synchronization.
        unsafe {
            asm!("mov gs:[{off}], {v:e}", off = const OFF, v = in(reg) v,
                options(nostack, preserves_flags));
        }
    }

    /// One per-CPU byte load.
    #[inline]
    pub fn read_u8<const OFF: u32>() -> u8 {
        let v: u8;
        // SAFETY: `read_u64`'s argument, for one byte.
        unsafe {
            asm!("mov {v}, gs:[{off}]", v = out(reg_byte) v, off = const OFF,
                options(nomem, nostack, preserves_flags));
        }
        v
    }

    /// One per-CPU byte store, from a register. Single-byte stores are
    /// naturally atomic on x86 — no `lock` prefix needed.
    #[inline]
    pub fn write_u8<const OFF: u32>(v: u8) {
        // SAFETY: `write_u32`'s argument, for one byte.
        unsafe {
            asm!("mov gs:[{off}], {v}", off = const OFF, v = in(reg_byte) v,
                options(nostack, preserves_flags));
        }
    }

    /// One per-CPU byte store, from an *immediate*.
    ///
    /// Its own primitive rather than [`write_u8`] called with a constant,
    /// because the instruction differs: this is the one-instruction
    /// `mov byte ptr gs:[244], 1`, where the register form would first
    /// materialise the value.
    #[inline]
    pub fn write_u8_imm<const OFF: u32, const VAL: u8>() {
        // SAFETY: `write_u32`'s argument, for one byte.
        unsafe {
            asm!("mov byte ptr gs:[{off}], {val}", off = const OFF, val = const VAL,
                options(nostack, preserves_flags));
        }
    }

    /// One `lock`-prefixed increment of a per-CPU `u32`.
    ///
    /// The prefix is not optional and is why the two counter primitives are
    /// their own: both kernel code and IRQ entry read-modify-write
    /// `preempt_count` on the same CPU, and an interrupt landing between the
    /// load and the store of a plain `add` loses whichever side went second.
    ///
    /// Increment and decrement stay two functions rather than one taking a
    /// delta, so the instruction is still the immediate-form `lock add`/`lock
    /// sub` every description of this path names — `arch::syscall`'s and
    /// `arch::idt`'s entry stubs open and close the same count with the same two
    /// instructions.
    #[inline]
    pub fn lock_inc_u32<const OFF: u32>() {
        // SAFETY: `write_u32`'s argument. **No `preserves_flags`**, and that is
        // a fix rather than an omission: `lock add` writes OF, SF, ZF, AF, CF
        // and PF, so a caller claiming it would be telling the compiler it could
        // keep a comparison's result live across a preempt-count change.
        unsafe {
            asm!("lock add dword ptr gs:[{off}], 1", off = const OFF, options(nostack));
        }
    }

    /// One `lock`-prefixed decrement of a per-CPU `u32`. See [`lock_inc_u32`].
    #[inline]
    pub fn lock_dec_u32<const OFF: u32>() {
        // SAFETY: `lock_inc_u32`'s argument exactly, including why
        // `preserves_flags` is absent.
        unsafe {
            asm!("lock sub dword ptr gs:[{off}], 1", off = const OFF, options(nostack));
        }
    }
}

/// **One stack size, so "which stack am I on" is not a question kernel code
/// has to answer.**
///
/// It was 16 KiB, and that number was never a decision about the work this
/// stack carries. The idle loop runs a scheduler pass, `drain_irqs` — which
/// reaches USB enumeration — and `object::drain_zero_handles`, which releases
/// arbitrary kernel objects. **It also ran `log_file::poll` until log
/// architecture L6**: a filesystem write down to a block device, whose measured
/// high water was **11,505 bytes of the 16,384** with the USB command path still
/// below the probe. That caller is gone; the number stays because it is what
/// established the depth this stack can be driven to, and `drain_irqs` still
/// reaches a device from here.
///
/// That last one is why this is the same number a task's kernel stack is.
/// `kobject!` classifies each object `deferred` or `immediate`, and an
/// `immediate` row's promise is that its destructor runs on the dropping
/// thread's 128 KiB stack rather than here — which `6d81a73` bought at 147
/// collateral reds after a killed process's file flush wrote through the guard
/// page below. **A `deferred` object may own an `immediate` one**: a `File`
/// sent over a connection whose peer dies is released from the drain, so the
/// classification is defeated by nesting and the macro cannot see it. Nothing
/// expressible in the object layer fixes that, because the entries are dropped
/// wherever the drain runs — so the drain gets a stack, and the invariant
/// becomes one every release path already has.
///
/// The cost is 112 KiB per CPU of a machine's physical memory, 14 MiB at 128
/// cores.
const IDLE_STACK_SIZE: usize = crate::process::KERNEL_STACK_SIZE;

/// One unmapped 4 KiB page below every idle stack.
///
/// The idle stack is ordinary physical memory, so without this an overflow does
/// not fault — it rewrites whatever is underneath, and the damage surfaces
/// later and elsewhere.
///
/// Unmapped rather than [`IST_GUARD_SIZE`]'s fill pattern, and the difference
/// is the stack, not the taste: #PF has no IST, so a frame pushed past the
/// bottom faults again on the same stack and the CPU takes a #DF — which
/// *does* have a stack, and reports. On IST1 there is no such second chance,
/// which is why that guard detects after the fact instead of trapping.
///
/// Either way the machine halts: a fault on a kernel address is a kernel bug
/// and `fatal_exception` treats it as fatal. The change is that it is reported
/// at all — an overflow used to land in the heap and be found later, somewhere
/// else, as a corrupted allocation.
const IDLE_GUARD_SIZE: usize = 4096;

/// **Every IST stack this machine has, and which vector takes which.**
///
/// The index is the `ist` column of `arch::idt`'s table, and the reason each row
/// has one is the same: the CPU builds the frame on the stack this names
/// *whatever `rsp` holds*, so a vector that can arrive while `rsp` is not a
/// kernel stack is a vector that must have one (SDM Vol. 3A §6.14.5). Three
/// instructions of `SYSCALL` entry and one of its exit run at CPL 0 on the
/// user's stack, and an exception taken there writes its frame to a user page
/// from CPL 0 — which SMAP refuses, so the `#PF` lands on the same stack and
/// escalates to `#DF`. That was measured on this tree
/// (`arch::syscall::init`'s `TF` note carries the capture).
///
/// - **IST1, `#DF`** — the crash report's own stack, and the reason the number
///   below is what it is.
/// - **IST2, NMI** — vector 2 arrives between arbitrary instructions and is not
///   maskable, so the window above is reachable by it whenever anything sends
///   one; `sched::dump` does, on Ctrl+Alt+D.
/// - **IST3, `#MC`** — an abort, so the machine is going down either way; the
///   stack is what lets it say so instead of triple-faulting on the way.
///
/// `Tss::ist` is a seven-entry array and a gate's `ist` byte indexes it from 1,
/// so IST*n* is `ist[n - 1]`.
pub(crate) const IST_STACKS: usize = 3;

/// The double fault stack, and now every IST stack: one size, for the reason
/// [`IDLE_STACK_SIZE`] gives — which stack a handler is on is not a question its
/// code should have to answer. What runs on IST1 is the whole crash report plus
/// `halt_all_cpus` — render, then `panic_flush` — and the nested-NMI report on
/// IST2 ends in the same `halt_all_cpus`, so the deepest of the three decides
/// the number for all of them.
///
/// It was 4096, and the byte ring's `drain_to_serial` put a 4096-byte buffer on it, so the
/// report overflowed the stack it was being written from and corrupted the
/// heap underneath while producing the evidence for the fault that had just
/// happened.
///
/// Both numbers here are `ist1_report`'s, off a real #DF, not estimates:
/// **9968 bytes** used before the drain buffers were cut to `DRAIN_CHUNK`, and
/// **4512** after. So the overrun was 5872 bytes — four times the ~1.4 KiB
/// first estimated — and, more to the point, cutting the buffers was
/// never going to be sufficient on its own: 4512 still does not fit 4096. The
/// stack had to grow whatever happened to the buffers.
///
/// 16384 is then the smallest power of two that leaves the report room to
/// double, which is the margin `double_fault_stack` asserts. It costs 20 KiB
/// per CPU with the guard, against the 16 KiB each already pays for an idle
/// stack.
///
/// **The record ring widened this path and the number is re-measured, not
/// re-argued: 6,688 bytes**, `ist1_report` off a real #DF on a
/// `double_fault_stack` run, guard intact. It is taken after `render` and after
/// `panic_flush`, so it covers the deepest the report goes — the record merge
/// and the paint included. The margin the gate asserts still holds: 6,688
/// doubled is 13,376 of 16,384.
///
/// **What is large on that path — type sizes, not a decomposition of the
/// measurement.** These are what `size_of` says, not what `ist1_report`
/// counted, and they come to 4,352 against the measured 6,688; the 2,336
/// between them is frames, spills, alignment and everything the path does that
/// is not one of these. Largest first, at `RECORD_BYTES` of 1024:
/// `log::console`'s rendered line (1,152); `emit`'s `LogRecord` (1,024) beside
/// `snapshot_committed`'s one materialised record (1,024) and its eight
/// `Descent`s (384); `paint`'s row table (768). The elision's tail buffer
/// (452 — its head is streamed and buffers nothing) is on a branch no symbol in
/// this tree reaches.
///
/// **It was 7,488 with the byte ring, and both halves of that difference are
/// deletions.** `commit` no longer stages a 1,016-byte `Body` here (the slot's
/// words are written directly), which the measurement did not notice — so that
/// frame was never the deepest one; and `SerialWriter`'s 1,024-byte line buffer
/// and `drain_to_serial`'s 512-byte chunk went with the ring, which it did.
///
/// It was 4,512 before the record ring, so the ring's net cost is 2,176 bytes
/// of a stack with 9,696 still free.
///
/// At [`IST_STACKS`] stacks and a guard each it costs 60 KiB per CPU, against
/// the 20 KiB one stack cost while `#DF` was the only vector with one.
const IST_STACK_SIZE: usize = 16384;

/// Filled with [`STACK_FILL`] and never written by anything legitimate, so an
/// overflow is observable after the fact.
///
/// Deliberately not an unmapped guard page: a page fault taken while already
/// on the double fault stack is a triple fault, which resets the machine and
/// takes the report with it. Detecting the overflow is worth more here than
/// trapping it, because the report is the entire reason this stack exists.
const IST_GUARD_SIZE: usize = 4096;

/// Chosen so a zeroed or ASCII byte cannot be mistaken for untouched stack.
const STACK_FILL: u8 = 0xA5;
const STACK_FILL_WORD: u64 = u64::from_ne_bytes([STACK_FILL; 8]);

/// Allocate and initialize PerCpu for a CPU. Returns a raw pointer (lives forever).
fn alloc_percpu(cpu_id: u32, lapic_id: u32) -> *mut PerCpu {
    let layout = Layout::from_size_align(size_of::<PerCpu>(), 16).unwrap();
    // SAFETY: `size_of::<PerCpu>()` is non-zero and 16 is a power of two, which
    // is the whole of `alloc_zeroed`'s contract. Irreducible because the block
    // is never freed and is published into `IA32_GS_BASE`, so no owning handle
    // — `Box`, `Vec`, `OwnedAlloc` — can hold it: the machine's lifetime is the
    // allocation's, and a drop would be a bug rather than a release.
    let ptr = unsafe { alloc_zeroed(layout) } as *mut PerCpu;
    assert!(!ptr.is_null(), "percpu: alloc failed");

    // SAFETY: the allocation above succeeded (asserted), is `size_of::<PerCpu>()`
    // bytes at 16-byte alignment, and is zeroed — which is a valid `PerCpu`,
    // every field being an integer, an array of them or an atomic over one. It
    // is not published anywhere until this function returns, so this `&mut` is
    // the only reference to it in the machine.
    let percpu = unsafe { &mut *ptr };
    percpu.self_ptr = ptr as u64;
    percpu.cpu_id = cpu_id;
    percpu.lapic_id = lapic_id;
    percpu.current_tid = u32::MAX;
    percpu.current_pid = u32::MAX;
    percpu.tss = Tss::new();
    percpu.gdt = GDT_ENTRIES;
    percpu.init_tss_descriptor();
    percpu.log_shard = alloc_log_shard(cpu_id);
    ptr
}

/// This CPU's log shard: cpu0's is the boot shard, and every other is a fresh
/// zeroed one.
///
/// **Here rather than in [`init_ap`], which is where an earlier draft of the
/// spec put it.** `init_ap` calls `control_regs::init` and `fpu::log_state`,
/// both of which log — so an AP whose shard were allocated there would log into
/// a shard that did not exist yet, and the only candidate is cpu0's, which
/// another CPU is writing. The whole `PerCpu` is BSP-allocated before the AP
/// runs an instruction, so allocating here closes that window rather than
/// narrowing it.
///
/// The slots stay zeroed, while [`log::Shard::initialize_zeroed`] writes the
/// nonzero first reservation number into `head`. cpu0 gets the same state from
/// [`log::Shard::new`] in `.bss`.
fn alloc_log_shard(cpu_id: u32) -> u64 {
    if cpu_id == 0 {
        return &raw const log::BOOT_SHARD as u64;
    }
    let layout = Layout::from_size_align(size_of::<log::Shard>(), 64).unwrap();
    // SAFETY: `alloc_percpu`'s argument — a non-zero size at a power-of-two
    // alignment, never freed, and the allocation this CPU logs into for the life
    // of the machine.
    let ptr = unsafe { alloc_zeroed(layout) } as *mut log::Shard;
    assert!(!ptr.is_null(), "percpu: log shard alloc failed for cpu{cpu_id}");
    // SAFETY: this is a fresh zeroed, 64-byte-aligned allocation which is not
    // published into `PerCpu` until this function returns.
    unsafe { log::Shard::initialize_zeroed(ptr) };
    // A writer finds its shard through `gs:` and a reader cannot, so the shard
    // is published to `log::shards` here — before the CPU it belongs to has
    // executed an instruction, which is the same window `PerCpu` itself is
    // built in.
    //
    // SAFETY: the allocation above is live for the life of the machine and is
    // initialised.
    unsafe { log::publish_ap_shard(cpu_id, ptr) };
    ptr as u64
}

/// This CPU's shard, its identity, and one sequence number out of that shard.
///
/// The `xadd` has **no `lock` prefix**. It is atomic against an interrupt on its
/// own CPU because instructions retire whole, and it is not atomic against
/// another CPU — which is sound only while this CPU owns the shard. The live
/// [`crate::arch::LogCommitGuard`] proves that neither migration nor a
/// single-step #DB can happen from this pointer read through publication.
///
/// **Four reads in one `asm!` block rather than four [`gs`] calls, and the
/// difference is the absent `nomem`.** Without it the block is an implicit
/// memory clobber, which is what keeps the shard *selection* on the closed side
/// of the compiler barrier the guard opened — the same reason
/// [`crate::arch::LogCommitGuard::close`] spells its `pushfq` without `nomem`.
/// A `gs::read_u64` here would carry `nomem` and let the selection float.
pub fn reserve_log_slot(
    guard: &crate::arch::LogCommitGuard,
) -> (*const log::Shard, u64, u32, u32, u32) {
    let shard: u64;
    let seq: u64;
    let cpu: u32;
    let tid: u32;
    let pid: u32;
    // SAFETY: the four `mov`s are `gs`'s contract — `GS_BASE` points at this
    // CPU's `PerCpu`, which the live `guard` also proves has not migrated. The
    // `reserve` below dereferences `log_shard`, which `alloc_percpu` fills for
    // cpu0 and for every AP before that AP executes an instruction, so it is
    // never null on a live CPU (the field's own doc carries that argument), and
    // the shard is a `'static` allocation that is never freed. `guard` is passed
    // through to `reserve`, which is where the unlocked `xadd`'s soundness
    // condition is stated.
    unsafe {
        core::arch::asm!(
            "mov {shard}, gs:[{shard_off}]",
            "mov {cpu:e}, gs:[{cpu_off}]",
            "mov {tid:e}, gs:[{tid_off}]",
            "mov {pid:e}, gs:[{pid_off}]",
            shard = out(reg) shard,
            cpu = out(reg) cpu,
            tid = out(reg) tid,
            pid = out(reg) pid,
            shard_off = const offset_of!(PerCpu, log_shard),
            cpu_off = const OFF_CPU_ID,
            tid_off = const OFF_CURRENT_TID,
            pid_off = const OFF_CURRENT_PID,
            options(preserves_flags),
        );
        seq = (&*(shard as *const log::Shard)).reserve(guard);
    }
    (shard as *const log::Shard, seq, cpu, tid, pid)
}

/// One idle stack and the guard page under it.
const IDLE_SLOT: usize = IDLE_GUARD_SIZE + IDLE_STACK_SIZE;

/// Idle stacks come out of 2 MiB pages of their own, not the kernel heap.
///
/// The guard is a hole in the direct map, and punching one costs the whole
/// 2 MiB leaf its large page. From the heap that leaf also held hot kernel
/// structures, and they went from one TLB entry to 512 — measured against the
/// same tree with the guard as the only difference, `i8042_mouse` fell from
/// 1006 pointer events to 27 under the full suite, three runs to one. An arena
/// the stacks alone share keeps that cost where it belongs: 15 of them per
/// leaf, and nothing else in it.
///
/// Never freed, which is what makes the permanent split sound — a leaf handed
/// back to the PMM would be reissued with a hole in its direct map.
static IDLE_STACKS: crate::sync::Lock<IdleArena> = crate::sync::Lock::new(IdleArena {
    pages: alloc::vec::Vec::new(),
    stacks: alloc::vec::Vec::new(),
    next: 0,
    left: 0,
});

struct IdleArena {
    pages: alloc::vec::Vec<crate::mm::pmm::PhysPage>,
    /// The bottom of every idle stack this machine has, so the deepest any of
    /// them has ever gone can be read from one CPU. Without it the measurement
    /// is per-CPU and the CPU that ran deepest is the one that is not asking.
    stacks: alloc::vec::Vec<u64>,
    /// Direct-map address of the next free slot.
    next: u64,
    left: usize,
}

/// A 4 KiB-aligned `IDLE_SLOT` from the arena.
fn alloc_idle_slot() -> u64 {
    let mut arena = IDLE_STACKS.lock();
    if arena.left < IDLE_SLOT {
        let page = crate::mm::pmm::alloc_page(crate::mm::pmm::Category::KernelHeap)
            .expect("percpu: no physical page for an idle stack");
        arena.next = page.direct_map().as_mut_ptr::<u8>() as u64;
        arena.left = crate::mm::PAGE_2M as usize;
        arena.pages.push(page);
    }
    let base = arena.next;
    arena.next += IDLE_SLOT as u64;
    arena.left -= IDLE_SLOT;
    arena.stacks.push(base + IDLE_GUARD_SIZE as u64);
    base
}

fn alloc_idle_stack(percpu: &mut PerCpu) {
    let base = alloc_idle_slot();
    crate::mm::paging::guard_kernel_page(base);
    // Filled rather than zeroed, for [`idle_stack_high_water`]: a zero is a
    // value the stack legitimately holds, so it cannot tell untouched from
    // written. After the guard, because the guard's page is no longer mapped.
    //
    // SAFETY: `alloc_idle_slot` returned `IDLE_SLOT` bytes of direct-mapped
    // physical memory it owns forever, `guard_kernel_page` has since unmapped
    // the first `IDLE_GUARD_SIZE` of them, and this writes the `IDLE_STACK_SIZE`
    // above that — the whole of what is left, and nothing else. Irreducible in
    // that the region is a stack about to be entered by an `iretq`, not a Rust
    // value: `&mut [u8]` over it would be a borrow of memory a CPU is about to
    // start pushing frames onto (`issues/kernel/pagealloc-has-no-checked-window.md`
    // is the same shape, filed by the root-file sweep).
    unsafe {
        core::ptr::write_bytes(
            (base + IDLE_GUARD_SIZE as u64) as *mut u8,
            STACK_FILL,
            IDLE_STACK_SIZE,
        )
    };
    percpu.idle_stack_top = base + IDLE_SLOT as u64;
    percpu.idle_rsp = percpu.idle_stack_top;
}

/// How big one idle stack is. Read by `SYS_DEBUG`, so the high water below is
/// a fraction of something rather than a number with no scale.
#[cfg(feature = "test-actuators")]
pub fn idle_stack_size() -> usize {
    IDLE_STACK_SIZE
}

/// The deepest any CPU's idle stack has ever been, in bytes.
///
/// **The instrument that says whether [`IDLE_STACK_SIZE`] is a decision or a
/// hope.** The guard page below turns an overflow into a reported fault, which
/// is a machine that stopped; this answers before it, from a running one, and
/// is what a churn test asserts against so a release path that grows deep is a
/// red rather than a halt.
///
/// Read from the bottom up, so it is the high water and not the current depth:
/// nothing legitimate writes [`STACK_FILL`], and a frame that reached a byte
/// leaves it changed for the rest of the boot.
#[cfg(feature = "test-actuators")]
pub fn idle_stack_high_water() -> usize {
    let arena = IDLE_STACKS.lock();
    arena
        .stacks
        .iter()
        .map(|&bottom| {
            let untouched =
                words(bottom, IDLE_STACK_SIZE).take_while(|&w| w == STACK_FILL_WORD).count() * 8;
            IDLE_STACK_SIZE - untouched
        })
        .max()
        .unwrap_or(0)
}

/// One stack per [`IST_STACKS`] row, filled and guarded alike.
///
/// **Every one of them, in one loop, because a vector whose row says `ist n` and
/// whose `ist[n - 1]` is zero is a vector that faults to address 0.** The CPU
/// does not check: it loads the TSS word and pushes there.
fn alloc_ist_stacks(percpu: &mut PerCpu) {
    let total = IST_GUARD_SIZE + IST_STACK_SIZE;
    for slot in 0..IST_STACKS {
        let layout = Layout::from_size_align(total, 4096).unwrap();
        // SAFETY: `alloc_percpu`'s argument — non-zero size, power-of-two
        // alignment, never freed. The 4096 is not decoration: `IST_GUARD_SIZE`
        // is a page and the guard's detection rests on it starting at one.
        let base = unsafe { alloc_zeroed(layout) };
        assert!(!base.is_null(), "percpu: IST{} stack alloc failed", slot + 1);
        // SAFETY: `total` bytes from `base`, which is exactly the allocation just
        // made and asserted non-null. Same irreducibility as `alloc_idle_stack`'s
        // fill: what is being written is a stack the CPU will switch to on a
        // fault, not a Rust value that could hold a borrow.
        unsafe { core::ptr::write_bytes(base, STACK_FILL, total) };
        let top = base as u64 + total as u64;
        // SAFETY: `Tss` is `repr(C, packed)`, so `&raw mut percpu.tss.ist[slot]`
        // is a well-formed but possibly unaligned pointer into a live `PerCpu`
        // this function holds `&mut` to — which is precisely `write_unaligned`'s
        // domain and why the plain assignment it replaces would be undefined.
        // `slot < IST_STACKS <= 7`, which is `Tss::ist`'s length.
        unsafe { core::ptr::write_unaligned(&raw mut percpu.tss.ist[slot], top); }
    }
}

const _: () = assert!(IST_STACKS <= 7, "a TSS has seven IST slots");

/// The IST1 stack top this CPU's TSS holds, if it looks like one.
///
/// Read through GS like everything else here, and checked rather than trusted:
/// the callers are on the panic path, where a corrupted percpu block is one of
/// the things that could have brought us here.
fn ist1_top() -> Option<u64> {
    let percpu = gs::read_u64::<OFF_SELF_PTR>() as *const PerCpu;
    if !crate::mm::is_kernel_addr(percpu as u64) {
        return None;
    }
    // SAFETY: the address came out of this CPU's own `self_ptr` and has just
    // been checked to be a kernel one, which is as far as a panic-path reader
    // can get — the whole point of this function is that the block may be
    // corrupt, so the read is checked afterwards rather than trusted. Unaligned
    // because `Tss` is `repr(C, packed)`.
    let top = unsafe { core::ptr::read_unaligned(&raw const (*percpu).tss.ist[0]) };
    let total = (IST_GUARD_SIZE + IST_STACK_SIZE) as u64;
    let base = top.checked_sub(total)?;
    (crate::mm::is_kernel_addr(base) && top % 4096 == 0).then_some(top)
}

/// Report how much of the double fault stack the crash report actually used,
/// straight to the UART.
///
/// Called from `halt_all_cpus` *after* `panic_flush`, which is the deepest the
/// path ever gets, and only when this CPU is running on IST1 — so it says
/// nothing on the ordinary fatal paths, which are on an ordinary stack.
///
/// It bypasses the log ring on purpose. The ring has just been drained and the
/// machine is about to halt, so anything queued there would never come out;
/// and if this reports damage, the ring is exactly what may have been
/// corrupted. The whole point is a channel that does not depend on the thing
/// under suspicion.
pub fn ist1_report() {
    let Some(top) = ist1_top() else { return };
    let rsp = cpu::read_rsp();
    let stack_bottom = top - IST_STACK_SIZE as u64;
    if rsp < stack_bottom || rsp > top {
        return;
    }

    let guard_base = stack_bottom - IST_GUARD_SIZE as u64;
    let intact = words(guard_base, IST_GUARD_SIZE).all(|w| w == STACK_FILL_WORD);
    let untouched = words(stack_bottom, IST_STACK_SIZE)
        .take_while(|&w| w == STACK_FILL_WORD)
        .count()
        * 8;
    let used = IST_STACK_SIZE - untouched;

    crate::drivers::serial::panic_raw(b"\n[ist1] used ");
    crate::drivers::serial::panic_raw_dec(used as u64);
    crate::drivers::serial::panic_raw(b" of ");
    crate::drivers::serial::panic_raw_dec(IST_STACK_SIZE as u64);
    crate::drivers::serial::panic_raw(if intact {
        b" bytes, guard intact\n"
    } else {
        b" bytes, GUARD CORRUPTED\n"
    });
}

/// Sequential u64s from `base`. Every address is inside the allocation the
/// caller just bounds-checked, so there is nothing here that can fault.
fn words(base: u64, len: usize) -> impl Iterator<Item = u64> {
    // SAFETY: `i < len / 8`, so every address is inside `[base, base + len)`,
    // which both callers have already established is a live stack allocation of
    // theirs — `ist1_report` after bounding `rsp` inside it, `idle_stack_high_water`
    // off the arena's own record. `read_volatile` because a fill pattern being
    // read back is exactly the observation the optimiser is entitled to remove.
    (0..len / 8).map(move |i| unsafe { core::ptr::read_volatile((base as *const u64).add(i)) })
}

/// Initialize per-CPU data for the BSP. Call after paging + allocator but before IDT/syscall.
pub fn init_bsp(lapic_id: u32) {
    let ptr = alloc_percpu(0, lapic_id);
    // SAFETY: `alloc_percpu` just returned a live, initialised, never-freed
    // `PerCpu` that nothing else has a reference to — it is not published into
    // `IA32_GS_BASE` until the `wrmsr` below.
    let percpu = unsafe { &mut *ptr };

    percpu.kernel_rsp = cpu::read_rsp();
    // SAFETY: `Tss` is `repr(C, packed)`, so `rsp0` may be unaligned; the
    // pointer is into the `&mut PerCpu` above.
    unsafe { core::ptr::write_unaligned(&raw mut percpu.tss.rsp0, cpu::read_rsp()); }
    alloc_idle_stack(percpu);
    alloc_ist_stacks(percpu);

    // SAFETY: `load_gdt` asks to be called exactly once per CPU during init, and
    // this is the BSP's one call — `init_ap` is every AP's. The GDT and TSS it
    // loads are this `PerCpu`'s own, filled by `alloc_percpu` and
    // `alloc_ist_stacks` above.
    unsafe { percpu.load_gdt(); }
    super::control_regs::init(0);
    super::fpu::init();

    // SAFETY: `wrmsr` asks its caller to own the MSR it names and the value it
    // writes. `IA32_GS_BASE` is where every `gs:` access in this kernel lands,
    // and this write is what makes those accesses mean anything at all on the
    // BSP — before it, `gs:` is whatever firmware left. `ptr` is the live,
    // never-freed `PerCpu` `alloc_percpu` returned above, whose `&mut` ended at
    // the `load_gdt` line, so publishing it hands the CPU the only reference
    // there is. Nothing between the allocation and here may read `gs:`, which is
    // why `PERCPU_READY` is stored on the line below rather than earlier.
    unsafe { cpu::wrmsr(MSR_GS_BASE, ptr as u64) };

    // GS base is now valid — enable CPU/TID context in log! macro
    crate::log::PERCPU_READY.store(true, core::sync::atomic::Ordering::Release);

    log!("percpu: BSP cpu_id=0 lapic_id={lapic_id}");
    super::fpu::log_state();
}

/// Allocate percpu for an AP on the BSP. Returns the raw pointer for the trampoline
/// to write into IA32_GS_BASE before loading the IDT.
pub fn alloc_ap(cpu_id: u32, lapic_id: u32) -> *mut PerCpu {
    let ptr = alloc_percpu(cpu_id, lapic_id);
    // SAFETY: `init_bsp`'s argument — a live, never-freed `PerCpu` nothing else
    // references. This one runs on the BSP for an AP that has not been sent its
    // INIT-SIPI yet, so the CPU it belongs to has executed no instruction.
    let percpu = unsafe { &mut *ptr };
    alloc_idle_stack(percpu);
    alloc_ist_stacks(percpu);
    ptr
}

/// Finish AP percpu initialization (called from ap_entry after GS base is set by trampoline).
///
/// `control_regs::init` and `fpu::log_state` are the two things here that print,
/// and each says at its own definition why it may not assume this CPU answers
/// like the BSP. Everything else is silent: `boot_aps` already logs one line per
/// AP that came up.
pub fn init_ap(percpu_ptr: *mut PerCpu) {
    // SAFETY: the pointer is this CPU's own `PerCpu`, read back out of `gs:[0]`
    // by the one caller (`smp::ap_entry`) after the trampoline put it in
    // `IA32_GS_BASE`; the BSP built it in `alloc_ap` and dropped its `&mut`
    // before sending the SIPI, so this is again the only reference to it.
    let percpu = unsafe { &mut *percpu_ptr };
    // SAFETY: `load_gdt` asks to be called exactly once per CPU during init, and
    // this is that call for this AP — `init_bsp` is the BSP's.
    unsafe { percpu.load_gdt(); }
    super::control_regs::init(percpu.cpu_id);
    super::fpu::init();
    super::fpu::log_state();
}

/// Update both the percpu kernel_rsp (for syscall entry) and tss.rsp0 (for interrupts).
/// Called during context switch when switching to a new process.
///
/// # Safety
/// Must be called from the CPU whose GS base points to the relevant PerCpu.
pub unsafe fn set_kernel_stack(rsp: u64) {
    let percpu = gs::read_u64::<OFF_SELF_PTR>() as *mut PerCpu;
    (*percpu).kernel_rsp = rsp;
    core::ptr::write_unaligned(&raw mut (*percpu).tss.rsp0, rsp);
}

/// The two words [`set_kernel_stack`] writes: the stack `syscall` switches to
/// (`kernel_rsp`) and the one an interrupt from Ring 3 switches to (`tss.rsp0`).
///
/// **Read only by an instrument, and it reads both because they are written
/// together and used apart.** Every Ring 3 → Ring 0 entry in the machine takes
/// its stack from one of these two, so a value here that is not the running
/// task's stack top is a stack pointer aimed at memory some other execution
/// owns — and the entry that uses it writes a return address there, which is the
/// shape a stray-write class chased across 2026-08-19..21 kept finding in kernel
/// data. It never was that: across 25,123 storm boots these two words always
/// agreed with the running task's own stack top, and the text-in-data came from
/// a `memcpy` running backwards (`arch::entry`'s `cld`).
///
/// # Safety
/// Must be called from the CPU whose GS base points to the relevant PerCpu.
#[cfg(feature = "stack-witness")]
pub unsafe fn entry_stacks() -> (u64, u64) {
    let percpu = gs::read_u64::<OFF_SELF_PTR>() as *const PerCpu;
    (
        (*percpu).kernel_rsp,
        core::ptr::read_unaligned(&raw const (*percpu).tss.rsp0),
    )
}

/// Read this CPU's ID from GS-relative percpu data.
pub fn cpu_id() -> u32 {
    gs::read_u32::<OFF_CPU_ID>()
}

/// Read the Tid of the thread currently running on this CPU. None means idle.
pub fn current_tid() -> Option<crate::process::Tid> {
    let raw = gs::read_u32::<OFF_CURRENT_TID>();
    if raw == u32::MAX { None } else { Some(crate::process::Tid::from_raw(raw)) }
}

/// Set the Tid of the thread running on this CPU. None sets idle (u32::MAX).
pub fn set_current_tid(tid: Option<crate::process::Tid>) {
    gs::write_u32::<OFF_CURRENT_TID>(tid.map_or(u32::MAX, |t| t.raw()));
}

/// Read the Pid of the process running on this CPU. None means idle.
pub fn current_pid() -> Option<crate::process::Pid> {
    let raw = gs::read_u32::<OFF_CURRENT_PID>();
    if raw == u32::MAX { None } else { Some(crate::process::Pid::from_raw(raw)) }
}

/// Set the Pid of the process running on this CPU. None sets idle (u32::MAX).
pub fn set_current_pid(pid: Option<crate::process::Pid>) {
    gs::write_u32::<OFF_CURRENT_PID>(pid.map_or(u32::MAX, |p| p.raw()));
}

/// This CPU's `PerCpu`, reached through its own self-reference at `gs:[0]`.
///
/// Only [`init_ap`]'s caller needs it: every field this module publishes is read
/// through [`gs`] at the field's own offset, one instruction, with no pointer
/// materialised at all.
pub fn percpu_ptr() -> *mut PerCpu {
    gs::read_u64::<OFF_SELF_PTR>() as *mut PerCpu
}

/// The count of Ring 0 timer fires the assembly stub has taken, and the count
/// the trace has already accounted for.
///
/// Written by the Ring 0 timer stub with a plain `inc` (IF is clear there, and
/// only that stub writes); read and reconciled by `arch::idt`'s exit-to-user
/// path, which owns the difference.
pub fn ring0_timer_fires() -> u32 {
    gs::read_u32::<OFF_RING0_TIMER_FIRES>()
}

pub fn last_seen_ring0_fires() -> u32 {
    gs::read_u32::<OFF_LAST_SEEN_RING0_FIRES>()
}

pub fn set_last_seen_ring0_fires(v: u32) {
    gs::write_u32::<OFF_LAST_SEEN_RING0_FIRES>(v);
}

/// Remember the one-shot count this CPU just armed, for the Ring 0 timer stub's
/// reload — `arch::apic` is the only caller, and the stub reads the same word
/// from `gs:` with no Rust in its path.
pub fn set_last_armed_ticks(ticks: u32) {
    gs::write_u32::<OFF_LAST_ARMED_TICKS>(ticks);
}

/// The byte immediately below this CPU's idle stack — the last byte of its
/// guard page, and the first thing an overflowing frame reaches.
#[cfg(feature = "test-actuators")]
pub fn idle_guard_byte() -> u64 {
    idle_stack_top() - IDLE_STACK_SIZE as u64 - 1
}

/// Top of this CPU's idle stack.
pub fn idle_stack_top() -> u64 {
    gs::read_u64::<OFF_IDLE_STACK_TOP>()
}

/// User RIP saved at last syscall entry (for panic diagnostics).
pub fn syscall_rip() -> u64 {
    gs::read_u64::<OFF_SYSCALL_RIP>()
}

/// Syscall number saved at last syscall entry (for panic diagnostics).
pub fn syscall_num() -> u64 {
    gs::read_u64::<OFF_SYSCALL_NUM>()
}

/// User RSP saved at last syscall entry.
pub fn user_rsp() -> u64 {
    gs::read_u64::<OFF_USER_RSP>()
}

/// User RBP saved at last syscall entry (for panic diagnostics).
pub fn syscall_rbp() -> u64 {
    gs::read_u64::<OFF_SYSCALL_RBP>()
}

/// Swap the per-CPU fault state. Returns the previous state.
/// Not atomic — safe because only exception/panic entry points read or write
/// fault_state, and they all run with interrupts disabled (interrupt gate for
/// exceptions, explicit cli for panics). The timer handler never touches it.
///
/// Read and written through [`gs`], which is also how `preempt::faulting` reads
/// the same byte — one way to reach a per-CPU field, not two.
pub fn swap_fault_state(new: CpuFaultState) -> CpuFaultState {
    let old = gs::read_u8::<OFF_FAULT_STATE>();
    gs::write_u8::<OFF_FAULT_STATE>(new as u8);
    match old {
        0 => CpuFaultState::Normal,
        1 => CpuFaultState::PageFault,
        2 => CpuFaultState::Fatal,
        3 => CpuFaultState::Panic,
        _ => CpuFaultState::Panic, // corrupted → treat as nested
    }
}

/// Set the per-CPU fault state.
pub fn set_fault_state(new: CpuFaultState) {
    gs::write_u8::<OFF_FAULT_STATE>(new as u8);
}

use core::mem::size_of;
use core::sync::atomic::{AtomicU32, AtomicU8};

use alloc::alloc::alloc_zeroed;
use core::alloc::Layout;

use super::cpu;
use crate::log;

const MSR_GS_BASE: u32 = 0xC000_0101;

// GDT selectors (must match entry order)
pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;
const TSS_SEL: u16 = 0x28;

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

// Compile-time checks: assembly uses hardcoded GS-relative offsets into PerCpu.
// If any field is reordered or resized, these will fail at compile time.
const _: () = assert!(core::mem::offset_of!(PerCpu, self_ptr) == 0);
const _: () = assert!(core::mem::offset_of!(PerCpu, cpu_id) == 8);
const _: () = assert!(core::mem::offset_of!(PerCpu, kernel_rsp) == 16);
const _: () = assert!(core::mem::offset_of!(PerCpu, user_rsp) == 24);
const _: () = assert!(core::mem::offset_of!(PerCpu, tss) == 32);
const _: () = assert!(core::mem::offset_of!(PerCpu, current_tid) == 136);
const _: () = assert!(core::mem::offset_of!(PerCpu, current_pid) == 140);
const _: () = assert!(core::mem::offset_of!(PerCpu, syscall_rbp) == 232);
const _: () = assert!(core::mem::offset_of!(PerCpu, preempt_count) == 240);
const _: () = assert!(core::mem::offset_of!(PerCpu, need_resched) == 244);
const _: () = assert!(core::mem::offset_of!(PerCpu, ring0_timer_fires) == 248);
const _: () = assert!(core::mem::offset_of!(PerCpu, last_seen_ring0_fires) == 252);
const _: () = assert!(core::mem::offset_of!(PerCpu, fault_state) == 256);
const _: () = assert!(core::mem::offset_of!(PerCpu, last_armed_ticks) == 260);

const IDLE_STACK_SIZE: usize = 16384; // 16KB

/// The double fault stack. Only #DF uses IST1, and what runs on it is the
/// whole crash report plus `halt_all_cpus` — render, then `panic_flush`.
///
/// It was 4096, and `drain_to_serial` put a 4096-byte buffer on it, so the
/// report overflowed the stack it was being written from and corrupted the
/// heap underneath while producing the evidence for the fault that had just
/// happened.
///
/// Both numbers here are `ist1_report`'s, off a real #DF, not estimates:
/// **9968 bytes** used before the drain buffers were cut to `DRAIN_CHUNK`, and
/// **4512** after. So the overrun was 5872 bytes — four times the ~1.4 KiB
/// known issues estimated — and, more to the point, cutting the buffers was
/// never going to be sufficient on its own: 4512 still does not fit 4096. The
/// stack had to grow whatever happened to the buffers.
///
/// 16384 is then the smallest power of two that leaves the report room to
/// double, which is the margin `double_fault_stack` asserts. It costs 20 KiB
/// per CPU with the guard, against the 16 KiB each already pays for an idle
/// stack.
const IST1_STACK_SIZE: usize = 16384;

/// Filled with [`STACK_FILL`] and never written by anything legitimate, so an
/// overflow is observable after the fact.
///
/// Deliberately not an unmapped guard page: a page fault taken while already
/// on the double fault stack is a triple fault, which resets the machine and
/// takes the report with it. Detecting the overflow is worth more here than
/// trapping it, because the report is the entire reason this stack exists.
const IST1_GUARD_SIZE: usize = 4096;

/// Chosen so a zeroed or ASCII byte cannot be mistaken for untouched stack.
const STACK_FILL: u8 = 0xA5;
const STACK_FILL_WORD: u64 = u64::from_ne_bytes([STACK_FILL; 8]);

/// Allocate and initialize PerCpu for a CPU. Returns a raw pointer (lives forever).
fn alloc_percpu(cpu_id: u32, lapic_id: u32) -> *mut PerCpu {
    let layout = Layout::from_size_align(size_of::<PerCpu>(), 16).unwrap();
    let ptr = unsafe { alloc_zeroed(layout) } as *mut PerCpu;
    assert!(!ptr.is_null(), "percpu: alloc failed");

    let percpu = unsafe { &mut *ptr };
    percpu.self_ptr = ptr as u64;
    percpu.cpu_id = cpu_id;
    percpu.lapic_id = lapic_id;
    percpu.current_tid = u32::MAX;
    percpu.current_pid = u32::MAX;
    percpu.tss = Tss::new();
    percpu.gdt = GDT_ENTRIES;
    percpu.init_tss_descriptor();
    ptr
}

fn alloc_idle_stack(percpu: &mut PerCpu) {
    let layout = Layout::from_size_align(IDLE_STACK_SIZE, 4096).unwrap();
    let base = unsafe { alloc_zeroed(layout) };
    assert!(!base.is_null(), "percpu: idle stack alloc failed");
    percpu.idle_stack_top = base as u64 + IDLE_STACK_SIZE as u64;
    percpu.idle_rsp = percpu.idle_stack_top;
}

fn alloc_ist1_stack(percpu: &mut PerCpu) {
    let total = IST1_GUARD_SIZE + IST1_STACK_SIZE;
    let layout = Layout::from_size_align(total, 4096).unwrap();
    let base = unsafe { alloc_zeroed(layout) };
    assert!(!base.is_null(), "percpu: IST1 stack alloc failed");
    unsafe { core::ptr::write_bytes(base, STACK_FILL, total) };
    let top = base as u64 + total as u64;
    unsafe { core::ptr::write_unaligned(&raw mut percpu.tss.ist[0], top); }
}

/// The IST1 stack top this CPU's TSS holds, if it looks like one.
///
/// Read through GS like everything else here, and checked rather than trusted:
/// the callers are on the panic path, where a corrupted percpu block is one of
/// the things that could have brought us here.
fn ist1_top() -> Option<u64> {
    let percpu: *const PerCpu;
    unsafe { core::arch::asm!("mov {}, gs:[0]", out(reg) percpu, options(nomem, nostack, preserves_flags)); }
    if !crate::mm::is_kernel_addr(percpu as u64) {
        return None;
    }
    let top = unsafe { core::ptr::read_unaligned(&raw const (*percpu).tss.ist[0]) };
    let total = (IST1_GUARD_SIZE + IST1_STACK_SIZE) as u64;
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
    let stack_bottom = top - IST1_STACK_SIZE as u64;
    if rsp < stack_bottom || rsp > top {
        return;
    }

    let guard_base = stack_bottom - IST1_GUARD_SIZE as u64;
    let intact = words(guard_base, IST1_GUARD_SIZE).all(|w| w == STACK_FILL_WORD);
    let untouched = words(stack_bottom, IST1_STACK_SIZE)
        .take_while(|&w| w == STACK_FILL_WORD)
        .count()
        * 8;
    let used = IST1_STACK_SIZE - untouched;

    crate::drivers::serial::panic_raw(b"\n[ist1] used ");
    crate::drivers::serial::panic_raw_dec(used as u64);
    crate::drivers::serial::panic_raw(b" of ");
    crate::drivers::serial::panic_raw_dec(IST1_STACK_SIZE as u64);
    crate::drivers::serial::panic_raw(if intact {
        b" bytes, guard intact\n"
    } else {
        b" bytes, GUARD CORRUPTED\n"
    });
}

/// Sequential u64s from `base`. Every address is inside the allocation the
/// caller just bounds-checked, so there is nothing here that can fault.
fn words(base: u64, len: usize) -> impl Iterator<Item = u64> {
    (0..len / 8).map(move |i| unsafe { core::ptr::read_volatile((base as *const u64).add(i)) })
}

/// Initialize per-CPU data for the BSP. Call after paging + allocator but before IDT/syscall.
pub fn init_bsp(lapic_id: u32) {
    let ptr = alloc_percpu(0, lapic_id);
    let percpu = unsafe { &mut *ptr };

    percpu.kernel_rsp = cpu::read_rsp();
    unsafe { core::ptr::write_unaligned(&raw mut percpu.tss.rsp0, cpu::read_rsp()); }
    alloc_idle_stack(percpu);
    alloc_ist1_stack(percpu);

    unsafe { percpu.load_gdt(); }
    cpu::enable_sse();
    let smep = cpu::enable_smep();
    let smap = cpu::enable_smap();
    require_fsgsbase(cpu::enable_fsgsbase());
    let pcid = crate::mm::paging::enable_pcid();

    cpu::wrmsr(MSR_GS_BASE, ptr as u64);

    // GS base is now valid — enable CPU/TID context in log! macro
    crate::log::PERCPU_READY.store(true, core::sync::atomic::Ordering::Release);

    // One line for the whole machine. Every CPU runs the identical sequence
    // against identical silicon, so the per-CPU repetition this replaces was
    // 4 lines times the core count of noise carrying one bit each — 28 of the
    // T14's ~150 boot lines, on a screen that holds 67. Reported rather than
    // asserted: on TCG none of the three is available, and a line claiming
    // otherwise would be a diagnostic that lies on the only machine most of
    // this tree's boots happen on.
    log!(
        "percpu: BSP cpu_id=0 lapic_id={} smep={} smap={} pcid={}",
        lapic_id, on(smep), on(smap), on(pcid)
    );
}

fn on(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

/// Unlike SMEP, SMAP and PCID, FSGSBASE is not optional here: `hw.rs` saves and
/// restores the user TLS base with `rdfsbase`/`wrfsbase` on every context
/// switch, so a CPU without it would #UD at the first one. Said out loud rather
/// than discovered, because the alternative is a fault on a path that has no
/// business faulting, on some future machine, with no line explaining it.
fn require_fsgsbase(enabled: bool) {
    assert!(
        enabled,
        "cpu: FSGSBASE is required — every context switch uses rdfsbase/wrfsbase",
    );
}

/// Allocate percpu for an AP on the BSP. Returns the raw pointer for the trampoline
/// to write into IA32_GS_BASE before loading the IDT.
pub fn alloc_ap(cpu_id: u32, lapic_id: u32) -> *mut PerCpu {
    let ptr = alloc_percpu(cpu_id, lapic_id);
    let percpu = unsafe { &mut *ptr };
    alloc_idle_stack(percpu);
    alloc_ist1_stack(percpu);
    ptr
}

/// Finish AP percpu initialization (called from ap_entry after GS base is set by trampoline).
///
/// Silent throughout: `boot_aps` already logs one line per AP that came up,
/// and this CPU's answers to the feature questions are the BSP's answers.
pub fn init_ap(percpu_ptr: *mut PerCpu) {
    let percpu = unsafe { &mut *percpu_ptr };
    unsafe { percpu.load_gdt(); }
    cpu::enable_sse();
    cpu::enable_smep();
    cpu::enable_smap();
    require_fsgsbase(cpu::enable_fsgsbase());
    crate::mm::paging::enable_pcid();
}

/// Update both the percpu kernel_rsp (for syscall entry) and tss.rsp0 (for interrupts).
/// Called during context switch when switching to a new process.
///
/// # Safety
/// Must be called from the CPU whose GS base points to the relevant PerCpu.
pub unsafe fn set_kernel_stack(rsp: u64) {
    let percpu: *mut PerCpu;
    core::arch::asm!("mov {}, gs:[0]", out(reg) percpu, options(nomem, nostack, preserves_flags));
    (*percpu).kernel_rsp = rsp;
    core::ptr::write_unaligned(&raw mut (*percpu).tss.rsp0, rsp);
}

/// Read this CPU's ID from GS-relative percpu data.
pub fn cpu_id() -> u32 {
    let id: u32;
    unsafe { core::arch::asm!("mov {:e}, gs:[8]", out(reg) id, options(nomem, nostack, preserves_flags)); }
    id
}

/// Read the Tid of the thread currently running on this CPU. None means idle.
pub fn current_tid() -> Option<crate::process::Tid> {
    let raw: u32;
    unsafe { core::arch::asm!("mov {:e}, gs:[136]", out(reg) raw, options(nomem, nostack, preserves_flags)); }
    if raw == u32::MAX { None } else { Some(crate::process::Tid::from_raw(raw)) }
}

/// Set the Tid of the thread running on this CPU. None sets idle (u32::MAX).
pub fn set_current_tid(tid: Option<crate::process::Tid>) {
    let raw = tid.map_or(u32::MAX, |t| t.raw());
    unsafe { core::arch::asm!("mov gs:[136], {:e}", in(reg) raw, options(nostack, preserves_flags)); }
}

/// Read the Pid of the process running on this CPU. None means idle.
pub fn current_pid() -> Option<crate::process::Pid> {
    let raw: u32;
    unsafe { core::arch::asm!("mov {:e}, gs:[140]", out(reg) raw, options(nomem, nostack, preserves_flags)); }
    if raw == u32::MAX { None } else { Some(crate::process::Pid::from_raw(raw)) }
}

/// Set the Pid of the process running on this CPU. None sets idle (u32::MAX).
pub fn set_current_pid(pid: Option<crate::process::Pid>) {
    let raw = pid.map_or(u32::MAX, |p| p.raw());
    unsafe { core::arch::asm!("mov gs:[140], {:e}", in(reg) raw, options(nostack, preserves_flags)); }
}

pub fn percpu_ptr() -> *mut PerCpu {
    let p: *mut PerCpu;
    unsafe { core::arch::asm!("mov {}, gs:[0]", out(reg) p, options(nomem, nostack, preserves_flags)); }
    p
}

/// Read the saved idle RSP for this CPU.
pub fn idle_rsp() -> u64 {
    unsafe { (*percpu_ptr()).idle_rsp }
}

/// Pointer to the idle_rsp field (for context_switch to save into).
pub fn idle_rsp_ptr() -> *mut u64 {
    unsafe { &raw mut (*percpu_ptr()).idle_rsp }
}

/// Top of this CPU's idle stack.
pub fn idle_stack_top() -> u64 {
    unsafe { (*percpu_ptr()).idle_stack_top }
}

/// User RIP saved at last syscall entry (for panic diagnostics).
pub fn syscall_rip() -> u64 {
    unsafe { (*percpu_ptr()).syscall_rip }
}

/// Syscall number saved at last syscall entry (for panic diagnostics).
pub fn syscall_num() -> u64 {
    unsafe { (*percpu_ptr()).syscall_num }
}

/// User RSP saved at last syscall entry.
pub fn user_rsp() -> u64 {
    unsafe { (*percpu_ptr()).user_rsp }
}

/// User RBP saved at last syscall entry (for panic diagnostics).
pub fn syscall_rbp() -> u64 {
    unsafe { (*percpu_ptr()).syscall_rbp }
}

/// Swap the per-CPU fault state. Returns the previous state.
/// Not atomic — safe because only exception/panic entry points read or write
/// fault_state, and they all run with interrupts disabled (interrupt gate for
/// exceptions, explicit cli for panics). The timer handler never touches it.
pub fn swap_fault_state(new: CpuFaultState) -> CpuFaultState {
    let p = unsafe { &mut (*percpu_ptr()).fault_state };
    let old = *p;
    *p = new as u8;
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
    unsafe { (*percpu_ptr()).fault_state = new as u8; }
}


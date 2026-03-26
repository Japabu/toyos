use core::mem::size_of;

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
    _pad: [u8; 4],      // offset 140: align GDT to 16 bytes
    gdt: [u64; 7],      // offset 144 (56 bytes)
    idle_rsp: u64,       // offset 200: saved RSP for idle context (for context_switch)
    idle_stack_top: u64, // offset 208: top of per-CPU idle stack
    /// Saved user RIP at last syscall entry (for panic diagnostics).
    pub syscall_rip: u64,  // offset 216
    /// Saved syscall number (for panic diagnostics).
    pub syscall_num: u64,  // offset 224
    /// Saved user RBP at last syscall entry (for panic diagnostics).
    pub syscall_rbp: u64,  // offset 232
    /// Per-CPU guard against recursive page faults.
    pub in_page_fault: bool,
    /// Per-CPU guard against re-entry into fatal_exception.
    pub in_fatal: bool,
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

const IDLE_STACK_SIZE: usize = 16384; // 16KB
const IST1_STACK_SIZE: usize = 4096;  // 4KB — only used by double fault handler

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
    let layout = Layout::from_size_align(IST1_STACK_SIZE, 4096).unwrap();
    let base = unsafe { alloc_zeroed(layout) };
    assert!(!base.is_null(), "percpu: IST1 stack alloc failed");
    let top = base as u64 + IST1_STACK_SIZE as u64;
    unsafe { core::ptr::write_unaligned(&raw mut percpu.tss.ist[0], top); }
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
    cpu::enable_smap();

    cpu::wrmsr(MSR_GS_BASE, ptr as u64);

    // GS base is now valid — enable CPU/TID context in log! macro
    crate::log::PERCPU_READY.store(true, core::sync::atomic::Ordering::Release);

    log!("percpu: BSP cpu_id=0 lapic_id={}", lapic_id);
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
pub fn init_ap(percpu_ptr: *mut PerCpu) {
    let percpu = unsafe { &mut *percpu_ptr };
    unsafe { percpu.load_gdt(); }
    cpu::enable_sse();
    cpu::enable_smap();

    log!("percpu: AP cpu_id={} lapic_id={}", percpu.cpu_id, percpu.lapic_id);
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

/// Swap the per-CPU page fault recursion guard. Returns the previous value.
pub fn swap_in_page_fault(val: bool) -> bool {
    let p = unsafe { &mut (*percpu_ptr()).in_page_fault };
    let old = *p;
    *p = val;
    old
}

/// Set the per-CPU page fault recursion guard.
pub fn set_in_page_fault(val: bool) {
    unsafe { (*percpu_ptr()).in_page_fault = val; }
}

/// Swap the per-CPU fatal exception recursion guard. Returns the previous value.
pub fn swap_in_fatal(val: bool) -> bool {
    let p = unsafe { &mut (*percpu_ptr()).in_fatal };
    let old = *p;
    *p = val;
    old
}

/// Set the per-CPU fatal exception recursion guard.
pub fn set_in_fatal(val: bool) {
    unsafe { (*percpu_ptr()).in_fatal = val; }
}


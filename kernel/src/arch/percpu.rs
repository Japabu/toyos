use core::mem::size_of;

use alloc::alloc::alloc_zeroed;
use core::alloc::Layout;

use super::cpu;
use crate::log;

const MSR_GS_BASE: u32 = 0xC000_0101;
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;

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
    _pad: [u8; 8],      // offset 136: align GDT to 16 bytes
    gdt: [u64; 7],      // offset 144
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
            "mov gs, {ds:x}",
            "mov ss, {ds:x}",
            in(reg) &ptr,
            cs = in(reg) KERNEL_CS as u64,
            ds = in(reg) KERNEL_DS as u64,
            tmp = lateout(reg) _,
        );

        cpu::ltr(TSS_SEL);
    }
}

/// Allocate and initialize PerCpu for a CPU. Returns a raw pointer (lives forever).
fn alloc_percpu(cpu_id: u32, lapic_id: u32) -> *mut PerCpu {
    let layout = Layout::from_size_align(size_of::<PerCpu>(), 16).unwrap();
    let ptr = unsafe { alloc_zeroed(layout) } as *mut PerCpu;
    assert!(!ptr.is_null(), "percpu: alloc failed");

    let percpu = unsafe { &mut *ptr };
    percpu.self_ptr = ptr as u64;
    percpu.cpu_id = cpu_id;
    percpu.lapic_id = lapic_id;
    percpu.tss = Tss::new();
    percpu.gdt = GDT_ENTRIES;
    percpu.init_tss_descriptor();
    ptr
}

/// Initialize per-CPU data for the BSP. Call after paging + allocator but before IDT/syscall.
pub fn init_bsp(lapic_id: u32) {
    let ptr = alloc_percpu(0, lapic_id);
    let percpu = unsafe { &mut *ptr };

    // Set initial kernel RSP (will be overwritten by process init)
    percpu.kernel_rsp = cpu::read_rsp();
    unsafe { core::ptr::write_unaligned(&raw mut percpu.tss.rsp0, cpu::read_rsp()); }

    // Load GDT from percpu
    unsafe { percpu.load_gdt(); }

    // Set GS base to percpu (kernel mode)
    cpu::wrmsr(MSR_GS_BASE, ptr as u64);
    // KERNEL_GS_BASE is swapped in on syscall entry (swapgs)
    cpu::wrmsr(MSR_KERNEL_GS_BASE, ptr as u64);

    log!("percpu: BSP cpu_id=0 lapic_id={}", lapic_id);
}

/// Initialize per-CPU data for an AP. Called from ap_entry.
pub fn init_ap(cpu_id: u32, lapic_id: u32) {
    let ptr = alloc_percpu(cpu_id, lapic_id);
    let percpu = unsafe { &mut *ptr };

    unsafe { percpu.load_gdt(); }

    cpu::wrmsr(MSR_GS_BASE, ptr as u64);
    cpu::wrmsr(MSR_KERNEL_GS_BASE, ptr as u64);

    log!("percpu: AP cpu_id={} lapic_id={}", cpu_id, lapic_id);
}

/// Update both the percpu kernel_rsp (for syscall entry) and tss.rsp0 (for interrupts).
/// Called during context switch when switching to a new process.
///
/// # Safety
/// Must be called from the CPU whose GS base points to the relevant PerCpu.
pub unsafe fn set_kernel_stack(rsp: u64) {
    // Read percpu pointer from gs:0
    let percpu: *mut PerCpu;
    core::arch::asm!("mov {}, gs:[0]", out(reg) percpu, options(nomem, nostack, preserves_flags));
    (*percpu).kernel_rsp = rsp;
    core::ptr::write_unaligned(&raw mut (*percpu).tss.rsp0, rsp);
}

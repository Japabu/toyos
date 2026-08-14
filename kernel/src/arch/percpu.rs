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
const _: () = assert!(core::mem::offset_of!(PerCpu, log_shard) == 264);

/// **One stack size, so "which stack am I on" is not a question kernel code
/// has to answer.**
///
/// It was 16 KiB, and that number was never a decision about the work this
/// stack carries. The idle loop runs a scheduler pass, `drain_irqs` — which
/// reaches USB enumeration — `log_file::poll`, a filesystem write down to a
/// block device whose measured high water was **11,505 bytes of the 16,384**
/// with the USB command path still below the probe, and
/// `object::drain_zero_handles`, which releases arbitrary kernel objects.
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
/// Unmapped rather than [`IST1_GUARD_SIZE`]'s fill pattern, and the difference
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
/// first estimated — and, more to the point, cutting the buffers was
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

/// This CPU's number and its current thread, for the legacy byte ring's prefix.
///
/// Unbracketed, which is what the `log!` macro did before the record ring and
/// is why it is only good enough for a line that is about to be rendered as
/// text. The record's own identity is read inside [`reserve_log_slot`]'s
/// bracket. Both this and the caller go with the byte ring at L3.
pub fn log_identity() -> (u32, u32) {
    let cpu: u32;
    let tid: u32;
    unsafe {
        core::arch::asm!(
            "mov {cpu:e}, gs:[8]",
            "mov {tid:e}, gs:[136]",
            cpu = out(reg) cpu,
            tid = out(reg) tid,
            options(nomem, nostack, preserves_flags),
        );
    }
    (cpu, tid)
}

/// This CPU's shard, its identity, and one sequence number out of that shard.
///
/// The `xadd` has **no `lock` prefix**. It is atomic against an interrupt on its
/// own CPU because instructions retire whole, and it is not atomic against
/// another CPU — which is sound only while this CPU owns the shard. The live
/// [`crate::arch::LogCommitGuard`] proves that neither migration nor a
/// single-step #DB can happen from this pointer read through publication.
pub fn reserve_log_slot(
    guard: &crate::arch::LogCommitGuard,
) -> (*const log::Shard, u64, u32, u32, u32) {
    let shard: u64;
    let seq: u64;
    let cpu: u32;
    let tid: u32;
    let pid: u32;
    unsafe {
        core::arch::asm!(
            "mov {shard}, gs:[{shard_off}]",
            "mov {cpu:e}, gs:[8]",
            "mov {tid:e}, gs:[136]",
            "mov {pid:e}, gs:[140]",
            shard = out(reg) shard,
            cpu = out(reg) cpu,
            tid = out(reg) tid,
            pid = out(reg) pid,
            shard_off = const core::mem::offset_of!(PerCpu, log_shard),
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
    super::control_regs::init(0);
    super::fpu::init();

    cpu::wrmsr(MSR_GS_BASE, ptr as u64);

    // GS base is now valid — enable CPU/TID context in log! macro
    crate::log::PERCPU_READY.store(true, core::sync::atomic::Ordering::Release);

    log!("percpu: BSP cpu_id=0 lapic_id={lapic_id}");
    super::fpu::log_state();
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
/// `control_regs::init` and `fpu::log_state` are the two things here that print,
/// and each says at its own definition why it may not assume this CPU answers
/// like the BSP. Everything else is silent: `boot_aps` already logs one line per
/// AP that came up.
pub fn init_ap(percpu_ptr: *mut PerCpu) {
    let percpu = unsafe { &mut *percpu_ptr };
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

/// The byte immediately below this CPU's idle stack — the last byte of its
/// guard page, and the first thing an overflowing frame reaches.
#[cfg(feature = "test-actuators")]
pub fn idle_guard_byte() -> u64 {
    idle_stack_top() - IDLE_STACK_SIZE as u64 - 1
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

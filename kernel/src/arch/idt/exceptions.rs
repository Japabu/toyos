use core::arch::naked_asm;

use crate::arch::{cpu, debug, syscall, percpu};
use crate::{log, process};

use core::sync::atomic::{AtomicBool, Ordering};

use super::{Vector, SavedRegs, InterruptFrame, RPL_MASK, PF_PRESENT, PF_WRITE, PF_INSTRUCTION_FETCH};

/// Guards against recursive page faults (e.g. from debug-mode ptr::add precondition checks).
static IN_PAGE_FAULT: AtomicBool = AtomicBool::new(false);
/// Guards against re-entry into fatal_exception (log!() triggering an unresolvable fault).
static IN_FATAL: AtomicBool = AtomicBool::new(false);

/// Write bytes to serial port 0x3F8 using raw port I/O.
/// No fmt, no allocation, no ptr::add — cannot cause recursive faults.
pub fn raw_serial(bytes: &[u8]) {
    for &b in bytes {
        unsafe { core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b, options(nomem, nostack)); }
    }
}

/// Write a u64 as hex to serial using raw port I/O.
pub fn raw_serial_hex(prefix: &[u8], value: u64) {
    raw_serial(prefix);
    for i in (0..16).rev() {
        let nibble = ((value >> (i * 4)) & 0xF) as u8;
        let ch = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
        unsafe { core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") ch, options(nomem, nostack)); }
    }
}

/// #DB (debug exception, vector 1) — no error code. Fires as a TRAP after the
/// instruction that triggered a data watchpoint. DR6 tells us which DR0-DR3 fired.
#[unsafe(naked)]
pub(super) extern "sysv64" fn db_entry() {
    naked_asm!(
        "test dword ptr [rsp + 8], 3",
        "jz 1f",
        "swapgs",
        "1:",
        "push 0", // dummy error code
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",
        "mov rdi, rsp",
        "sub rsp, 8",
        "call {handler}",
        "add rsp, 8",
        // If handler returns, resume execution
        "pop rax",  "pop rbx",  "pop rcx",  "pop rdx",
        "pop rsi",  "pop rdi",  "pop rbp",
        "pop r8",   "pop r9",   "pop r10",  "pop r11",
        "pop r12",  "pop r13",  "pop r14",  "pop r15",
        "add rsp, 8", // skip dummy error code
        "test dword ptr [rsp + 8], 3",
        "jz 3f",
        "swapgs",
        "3:",
        "iretq",
        handler = sym debug_handler,
    );
}

/// #DB handler — logs full context when a hardware watchpoint fires.
extern "sysv64" fn debug_handler(regs: *const SavedRegs) {
    // Raw serial output first — bypasses all abstractions
    unsafe {
        for &b in b"\n!!! DB TRAP !!!\n" {
            core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b);
        }
    }

    let regs = unsafe { &*regs };
    let frame = regs.interrupt_frame();
    let dr6 = debug::read_dr6();

    let is_user = frame.cs & RPL_MASK != 0;
    let pid = percpu::current_tid();

    log!("=== HARDWARE WATCHPOINT HIT ===");
    log!("  DR6={:#x} ({})", dr6,
        if dr6 & 1 != 0 { "DR0" }
        else if dr6 & 2 != 0 { "DR1" }
        else if dr6 & 4 != 0 { "DR2" }
        else if dr6 & 8 != 0 { "DR3" }
        else { "unknown" });
    log!("  context_tag={:#x}", debug::context());
    log!("  mode={} pid={:?}", if is_user { "user" } else { "kernel" }, pid);
    log!("  rip={:#018x}  rsp={:#018x}  rbp={:#018x}", frame.rip, frame.rsp, regs.rbp);
    log!("  rax={:#018x}  rbx={:#018x}  rcx={:#018x}", regs.rax, regs.rbx, regs.rcx);
    log!("  rdx={:#018x}  rsi={:#018x}  rdi={:#018x}", regs.rdx, regs.rsi, regs.rdi);

    // Symbol resolution
    log!("  Instruction that wrote:");
    if is_user {
        if let Some(p) = pid {
            if !process::resolve_user_symbol(p, frame.rip) {
                log!("    {:#x}", frame.rip);
            }
        }
    } else {
        crate::symbols::resolve_kernel(frame.rip);
    }

    // Backtrace
    log!("  Backtrace:");
    let pml4 = if is_user { crate::DirectMap::new(cpu::read_cr3()).as_ptr::<u64>() as *const u64 } else { core::ptr::null() };
    let mut rbp = regs.rbp;
    for _ in 0..20 {
        let Some(saved_rbp) = safe_read_u64(rbp, pml4) else { break };
        let Some(return_addr) = safe_read_u64(rbp + 8, pml4) else { break };
        if return_addr == 0 { break; }
        if is_user {
            if let Some(p) = pid {
                if !process::resolve_user_symbol(p, return_addr) {
                    log!("    {:#x}", return_addr);
                }
            } else {
                log!("    {:#x}", return_addr);
            }
        } else {
            crate::symbols::resolve_kernel(return_addr);
        }
        rbp = saved_rbp;
    }

    // Read the watched address to see what was written
    let watched_addr: u64;
    unsafe { core::arch::asm!("mov {}, dr0", out(reg) watched_addr); }
    if crate::mm::is_kernel_addr(watched_addr) && watched_addr % 8 == 0 {
        let val = unsafe { *(watched_addr as *const u64) };
        log!("  Value at watched addr {:#x} = {:#018x}", watched_addr, val);
    }

    log!("=== END WATCHPOINT ===");

    // Clear DR6 so we don't re-trigger, then disable watchpoint
    unsafe {
        core::arch::asm!("mov dr6, {}", in(reg) 0u64);
        core::arch::asm!("mov dr7, {}", in(reg) 0u64);
    }

    // Don't halt — let execution continue so we can see the aftermath
}

/// #UD — no error code pushed by CPU, so CS is at [rsp + 8].
#[unsafe(naked)]
pub(super) extern "sysv64" fn ud_entry() {
    naked_asm!(
        "test dword ptr [rsp + 8], 3",
        "jz 1f",
        "swapgs",
        "1:",
        "push 0", // dummy error code for uniform stack layout
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",
        "mov rdi, 6",
        "mov rsi, rsp",
        "sub rsp, 8",
        "call {handler}",
        "cli", "hlt",
        handler = sym exception_handler,
    );
}

/// #GP — CPU pushes error code, so CS is at [rsp + 16].
#[unsafe(naked)]
pub(super) extern "sysv64" fn gpf_entry() {
    naked_asm!(
        "test dword ptr [rsp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",
        "mov rdi, 13",
        "mov rsi, rsp",
        "sub rsp, 8",
        "call {handler}",
        "cli", "hlt",
        handler = sym exception_handler,
    );
}

/// Page fault entry — ring check, swapgs, save GPRs, call Rust, restore, iretq.
/// If the Rust handler returns, the fault was resolved. If fatal, it diverges.
#[unsafe(naked)]
pub(super) extern "sysv64" fn page_fault_entry() {
    naked_asm!(
        // Error code on stack. CS is at [rsp + 16].
        "test dword ptr [rsp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",

        // One arg: pointer to saved regs
        "mov rdi, rsp",
        "sub rsp, 8", // 16-byte align (15 GPR pushes + error code = 16 pushes = aligned, but sub 8 for call)
        "sti",         // re-enable interrupts so timer/deadlines fire during page fault handling
        "call {handler}",
        "cli",
        "add rsp, 8",

        // Handler returned — fault was resolved. Restore and return.
        "pop rax",  "pop rbx",  "pop rcx",  "pop rdx",
        "pop rsi",  "pop rdi",  "pop rbp",
        "pop r8",   "pop r9",   "pop r10",  "pop r11",
        "pop r12",  "pop r13",  "pop r14",  "pop r15",
        "add rsp, 8", // skip error code
        "test dword ptr [rsp + 8], 3",
        "jz 3f",
        "swapgs",
        "3:",
        "iretq",
        handler = sym page_fault_handler,
    );
}

/// Double fault — runs on IST1 with a dedicated stack. Always from kernel (no swapgs).
#[unsafe(naked)]
pub(super) extern "sysv64" fn double_fault_entry() {
    naked_asm!(
        // CPU pushes error code (always 0) for #DF.
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",
        "mov rdi, rsp",
        "sub rsp, 8", // 16-byte align
        "call {handler}",
        "cli", "hlt",
        handler = sym double_fault_handler,
    );
}

// ============================================================
// Rust handlers — all logic lives here, zero asm
// ============================================================

// --- Double fault ---
//
// The double fault handler runs on IST1 — a dedicated stack that is always valid.
// All memory reads go through safe_read_kernel() to prevent triple faults.
// After printing the kernel backtrace, it scans the original kernel stack for the
// interrupt frame that triggered the chain, recovering the user context if present.

/// Safe kernel memory read for the double fault handler.
/// Only reads kernel direct-map addresses. Returns None for anything suspect.
fn safe_read_kernel(addr: u64) -> Option<u64> {
    if addr % 8 != 0 || !crate::mm::is_kernel_addr(addr) {
        return None;
    }
    Some(unsafe { core::ptr::read_volatile(addr as *const u64) })
}

extern "sysv64" fn double_fault_handler(regs: *const SavedRegs) -> ! {
    let regs = unsafe { &*regs };
    let frame = regs.interrupt_frame();
    let cr2 = cpu::read_cr2();
    let cpu_id = percpu::cpu_id();
    let pid = percpu::current_tid();

    log!("DOUBLE FAULT on CPU {} (pid={:?})", cpu_id, pid);
    log!("  cr2={:#018x} (address that caused the fault chain)", cr2);
    log!("  rip={:#018x}  rsp={:#018x}  rbp={:#018x}", frame.rip, frame.rsp, regs.rbp);
    crate::mm::paging::debug_page_walk(cr2);

    // Kernel backtrace (where the double fault actually fired)
    log!("  Kernel backtrace:");
    crate::symbols::resolve_kernel_nonblocking(frame.rip);
    let mut rbp = regs.rbp;
    for _ in 0..20 {
        let Some(saved_rbp) = safe_read_kernel(rbp) else { break };
        let Some(return_addr) = safe_read_kernel(rbp + 8) else { break };
        if return_addr == 0 { break; }
        crate::symbols::resolve_kernel_nonblocking(return_addr);
        rbp = saved_rbp;
    }

    // Scan the original kernel stack for the interrupt frame that started
    // the exception chain. Our exception entry stubs push SavedRegs (15 u64s)
    // then an error code, then the CPU's interrupt frame follows:
    //   [SavedRegs] [error_code] [RIP] [CS] [RFLAGS] [RSP] [SS]
    // We look for a slot where [CS] is a valid code segment selector (0x08 or 0x23).
    let kernel_rsp = frame.rsp;
    log!("  Scanning kernel stack at {:#x} for original exception context...", kernel_rsp);

    // Scan upward from where the double fault's RSP was (the old kernel stack).
    // The interrupt frame could be anywhere above, within a reasonable range.
    let scan_start = kernel_rsp;
    let scan_end = kernel_rsp.saturating_add(4096); // kernel stacks are typically 16-64KB
    let mut addr = scan_start;

    while addr < scan_end {
        // Check if this looks like an interrupt frame: [error_code] [RIP] [CS] [RFLAGS] [RSP] [SS]
        // CS must be 0x08 (kernel) or 0x23 (user code64), and RFLAGS must have bit 1 set (always 1).
        let Some(maybe_rip) = safe_read_kernel(addr) else { break };
        let Some(maybe_cs) = safe_read_kernel(addr + 8) else { break };
        let Some(maybe_rflags) = safe_read_kernel(addr + 16) else { break };
        let Some(maybe_rsp) = safe_read_kernel(addr + 24) else { break };

        let valid_cs = maybe_cs == 0x08 || maybe_cs == 0x23;
        let valid_rflags = maybe_rflags & 2 != 0 && maybe_rflags & !0x3F_FFFF == 0;
        let valid_rip = maybe_rip > 0x1000; // not null

        if valid_cs && valid_rflags && valid_rip {
            let is_user = maybe_cs == 0x23;
            log!("  Found interrupt frame at stack offset +{:#x}:", addr - kernel_rsp);
            log!("    rip={:#018x}  cs={:#x}  rflags={:#x}", maybe_rip, maybe_cs, maybe_rflags);
            log!("    rsp={:#018x}", maybe_rsp);

            // Check if SavedRegs sit just below this interrupt frame
            // Layout: [SavedRegs (15*8=120 bytes)] [error_code (8)] [RIP] [CS] ...
            // So error_code is at addr - 8, and SavedRegs starts at addr - 8 - 15*8
            let error_code_addr = addr.wrapping_sub(8);
            let saved_regs_base = error_code_addr.wrapping_sub(15 * 8);
            if let Some(error_code) = safe_read_kernel(error_code_addr) {
                log!("    error_code={:#x}", error_code);
            }

            if is_user {
                // Try to recover user RBP from SavedRegs (rbp is at offset 6*8)
                let user_rbp_addr = saved_regs_base + 6 * 8;
                if let Some(user_rbp) = safe_read_kernel(user_rbp_addr) {
                    log!("  User context (pid={:?}):", pid);
                    log!("    rip={:#018x}  rsp={:#018x}  rbp={:#018x}", maybe_rip, maybe_rsp, user_rbp);

                    // Walk user backtrace through page tables
                    let pml4 = crate::DirectMap::new(cpu::read_cr3()).as_ptr::<u64>();
                    log!("  User backtrace:");
                    if let Some(p) = pid {
                        if !process::resolve_user_symbol(p, maybe_rip) {
                            log!("    {:#x}", maybe_rip);
                        }
                    } else {
                        log!("    {:#x}", maybe_rip);
                    }
                    let mut ubp = user_rbp;
                    for _ in 0..20 {
                        if ubp == 0 || ubp % 8 != 0 { break; }
                        let Some(saved) = safe_read_u64(ubp, pml4) else { break };
                        let Some(ret) = safe_read_u64(ubp + 8, pml4) else { break };
                        if ret == 0 { break; }
                        if let Some(p) = pid {
                            if !process::resolve_user_symbol(p, ret) {
                                log!("    {:#x}", ret);
                            }
                        } else {
                            log!("    {:#x}", ret);
                        }
                        ubp = saved;
                    }
                }
            } else {
                log!("  Original fault was in kernel code");
                log!("  Kernel backtrace from original fault:");
                crate::symbols::resolve_kernel_nonblocking(maybe_rip);
                // Walk RBP chain from the saved regs
                let rbp_addr = saved_regs_base + 6 * 8;
                if let Some(orig_rbp) = safe_read_kernel(rbp_addr) {
                    let mut bp = orig_rbp;
                    for _ in 0..20 {
                        let Some(saved) = safe_read_kernel(bp) else { break };
                        let Some(ret) = safe_read_kernel(bp + 8) else { break };
                        if ret == 0 { break; }
                        crate::symbols::resolve_kernel_nonblocking(ret);
                        bp = saved;
                    }
                }
            }
            break;
        }

        addr += 8;
    }

    cpu::halt();
}

// --- Page fault (demand paging) ---

/// Returns normally if the fault was resolved (page mapped in).
/// Diverges (never returns) if the fault is fatal.
extern "sysv64" fn page_fault_handler(regs: *const SavedRegs) {
    if IN_PAGE_FAULT.swap(true, Ordering::SeqCst) {
        // Recursive page fault — we're already handling one. This happens when
        // log!() in fatal_exception triggers a debug-mode ptr::add precondition
        // check fault. Don't try to handle it; let the fault fall through to
        // fatal_exception which will detect re-entry via IN_FATAL.
        let regs = unsafe { &*regs };
        let frame = regs.interrupt_frame();
        let ctx = ExceptionContext {
            vector: Vector::PageFault,
            regs,
            frame,
            cr2: cpu::read_cr2(),
        };
        fatal_exception(&ctx);
    }

    let regs = unsafe { &*regs };
    let frame = regs.interrupt_frame();
    let fault_addr = cpu::read_cr2();

    // Raw serial spam: log every page fault
    raw_serial(b"PF cr2=");
    raw_serial_hex(b"", fault_addr);
    raw_serial_hex(b" rip=", frame.rip);
    raw_serial_hex(b" err=", frame.error_code);
    raw_serial_hex(b" cr3=", cpu::read_cr3());
    raw_serial(b"\n");

    // SMAP violation detection: kernel-mode protection fault on a kernel direct-map address.
    // Enable stac immediately so diagnostics don't cascade into another SMAP fault.
    if frame.error_code & PF_PRESENT != 0 && frame.cs & RPL_MASK == 0
        && crate::mm::is_kernel_addr(fault_addr)
    {
        log!("SMAP cr2={:#018x} rip={:#018x} err={:#018x} rflags={:#018x}",
            fault_addr, frame.rip, frame.error_code, frame.rflags);
        log!("  SMAP kernel backtrace:");
        crate::symbols::resolve_kernel(frame.rip);
        let mut rbp = regs.rbp;
        for _ in 0..20 {
            if rbp == 0 || rbp % 8 != 0 || !crate::mm::is_kernel_addr(rbp) { break; }
            let saved_rbp = unsafe { *(rbp as *const u64) };
            let return_addr = unsafe { *((rbp + 8) as *const u64) };
            if return_addr == 0 { break; }
            crate::symbols::resolve_kernel(return_addr);
            rbp = saved_rbp;
        }
    }

    // Only handle not-present faults — protection violations are always fatal
    if frame.error_code & PF_PRESENT == 0 {
        let is_user = frame.cs & RPL_MASK != 0;
        if is_user || percpu::current_tid().is_some() {
            if process::handle_page_fault(fault_addr, frame.error_code) {
                IN_PAGE_FAULT.store(false, Ordering::Relaxed);
                return;
            }
            // Demand paging failed
            log!("#PF UNHANDLED: cr2={:#x} rip={:#x} err={:#x} user={} tid={:?}",
                fault_addr, frame.rip, frame.error_code, is_user, percpu::current_tid());
        } else {
            log!("#PF SKIP: cr2={:#x} rip={:#x} err={:#x} (no tid, not user)",
                fault_addr, frame.rip, frame.error_code);
        }
    } else {
        log!("#PF PRESENT: cr2={:#x} rip={:#x} err={:#x} cs={:#x}",
            fault_addr, frame.rip, frame.error_code, frame.cs);
    }

    // Fatal — build context and terminate
    let ctx = ExceptionContext {
        vector: Vector::PageFault,
        regs,
        frame,
        cr2: fault_addr,
    };
    fatal_exception(&ctx);
}

// ============================================================
// Exception diagnostics — all allocation-free
// ============================================================

/// Complete CPU state at the time of an exception.
struct ExceptionContext<'a> {
    vector: Vector,
    regs: &'a SavedRegs,
    frame: &'a InterruptFrame,
    cr2: u64,
}

impl ExceptionContext<'_> {
    /// Whether the exception occurred in user mode (Ring 3).
    fn is_user_mode(&self) -> bool {
        self.frame.cs & RPL_MASK != 0
    }

    /// Whether this fault should be attributed to a user process.
    /// True for Ring 3 faults, and also for kernel-mode page faults on user memory
    /// during a syscall (e.g. bad pointer passed to write()).
    /// GPFs use cr2 only for page faults — for other vectors, cr2 is stale/zero
    /// and must not be used to classify the fault.
    fn is_user_fault(&self) -> bool {
        self.is_user_mode()
            || (self.vector == Vector::PageFault
                && percpu::current_tid().is_some()
                && self.cr2 < 0x0000_8000_0000_0000)
    }
}

/// Safely read a u64 from memory. For user addresses, translates through page
/// tables to avoid triggering demand-paging faults inside exception handlers.
/// For kernel addresses, reads directly via the kernel direct map.
fn safe_read_u64(addr: u64, user_pml4: *const u64) -> Option<u64> {
    if addr % 8 != 0 || addr == 0 {
        return None;
    }
    if !user_pml4.is_null() {
        // Inline page table walk — can't take locks in exception handlers.
        let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
        let pd_idx = ((addr >> 21) & 0x1FF) as usize;
        let pml4e = unsafe { *user_pml4.add(pml4_idx) };
        if pml4e & 1 == 0 { return None; }
        let pdpt = crate::DirectMap::new(pml4e & 0x000F_FFFF_FFFF_F000).as_ptr::<u64>();
        let pdpte = unsafe { *pdpt.add(pdpt_idx) };
        if pdpte & 1 == 0 { return None; }
        let pd = crate::DirectMap::new(pdpte & 0x000F_FFFF_FFFF_F000).as_ptr::<u64>();
        let pde = unsafe { *pd.add(pd_idx) };
        if pde & 1 == 0 { return None; }
        let page_phys = pde & 0x000F_FFFF_FFE0_0000;
        let offset = addr & (crate::mm::PAGE_2M - 1);
        Some(unsafe { *crate::DirectMap::new(page_phys + offset).as_ptr::<u64>() })
    } else if crate::mm::is_kernel_addr(addr) {
        Some(unsafe { *(addr as *const u64) })
    } else {
        None
    }
}

/// Fatal exception handler. Prints diagnostics, then kills the process (user fault)
/// or halts the kernel (kernel fault). Never returns.
///
/// All logging is allocation-free — log! writes directly to serial.
/// format!() is forbidden (allocates, will deadlock if allocator lock is held).
extern "sysv64" fn exception_handler(raw_vector: u64, regs: *const SavedRegs) -> ! {
    let regs = unsafe { &*regs };
    let vector = Vector::from_raw(raw_vector);
    let frame = regs.interrupt_frame();
    let cr2 = if vector == Vector::PageFault { cpu::read_cr2() } else { 0 };
    let ctx = ExceptionContext { vector, regs, frame, cr2 };
    fatal_exception(&ctx);
}

/// Core fatal exception logic. Shared by page_fault_handler (when unresolvable)
/// and exception_handler (for all other fatal exceptions).
///
/// Step 1: Raw serial preamble (guaranteed output, cannot fail).
/// Step 2: Rich diagnostics via log!() (best-effort; if it triggers an
///         unresolvable fault, we re-enter via IN_FATAL and skip to step 3).
/// Step 3: Terminate (kill process for user faults, halt for kernel faults).
fn fatal_exception(ctx: &ExceptionContext) -> ! {
    let is_user = ctx.is_user_fault();
    let recursive = IN_FATAL.swap(true, Ordering::SeqCst);

    // === Step 1: Raw serial preamble (always, even on recursive entry) ===
    let tid_raw = percpu::current_tid().map_or(u32::MAX, |t| t.raw());
    raw_serial_hex(b"\n!!! FAULT rip=", ctx.frame.rip);
    raw_serial_hex(b" cr2=", ctx.cr2);
    raw_serial_hex(b" err=", ctx.frame.error_code);
    raw_serial_hex(b" cr3=", cpu::read_cr3());
    raw_serial_hex(b" rsp=", ctx.frame.rsp);
    raw_serial_hex(b" tid=", tid_raw as u64);
    if recursive { raw_serial(b" RECURSIVE"); }
    raw_serial(b"\n");

    // Raw serial stack dump — when RIP=0 (null call), the return address is at RSP
    raw_serial(b" stack:");
    let rsp = ctx.frame.rsp;
    for i in 0..8u64 {
        let addr = rsp.wrapping_add(i * 8);
        if !crate::mm::is_kernel_addr(addr) { break; }
        let val = unsafe { *(addr as *const u64) };
        raw_serial_hex(b" ", val);
    }
    raw_serial(b"\n");

    if recursive {
        // Re-entered fatal_exception (log!() triggered an unresolvable fault).
        // Step 1 already printed the basics on both entries. Just terminate.
        if is_user {
            IN_FATAL.store(false, Ordering::SeqCst);
            IN_PAGE_FAULT.store(false, Ordering::SeqCst);
            syscall::kill_process(-1);
        }
        cpu::halt();
    }

    // Reset IN_PAGE_FAULT so log!() can trigger normal (resolvable) page faults
    // like demand paging. Unresolvable faults will re-enter fatal_exception
    // where IN_FATAL catches the recursion.
    IN_PAGE_FAULT.store(false, Ordering::SeqCst);

    // === Step 2: Rich diagnostics via log!() ===
    let tid = percpu::current_tid().unwrap_or(crate::process::Tid(0));
    let pml4 = if is_user { crate::DirectMap::new(cpu::read_cr3()).as_ptr::<u64>() as *const u64 } else { core::ptr::null() };

    // Header
    if is_user {
        match ctx.vector {
            Vector::PageFault => {
                let action = if ctx.frame.error_code & PF_INSTRUCTION_FETCH != 0 { "execute" }
                    else if ctx.frame.error_code & PF_WRITE != 0 { "write" }
                    else { "read" };
                let cause = if ctx.frame.error_code & PF_PRESENT != 0 { "protection violation" }
                    else { "unmapped address" };
                log!("SEGFAULT tid={}: {} {} at {:#x}", tid, action, cause, ctx.cr2);
            }
            Vector::InvalidOpcode => log!("SIGILL tid={}: illegal instruction", tid),
            Vector::GeneralProtection => log!("SIGBUS tid={}: general protection fault (error_code={:#x})", tid, ctx.frame.error_code),
            Vector::DoubleFault => log!("FATAL tid={}: double fault", tid),
            _ => log!("FATAL tid={}: exception {:?}", tid, ctx.vector),
        }
    } else {
        match ctx.vector {
            Vector::PageFault => {
                let action = if ctx.frame.error_code & PF_INSTRUCTION_FETCH != 0 { "execute" }
                    else if ctx.frame.error_code & PF_WRITE != 0 { "write" }
                    else { "read" };
                let cause = if ctx.frame.error_code & PF_PRESENT != 0 { "protection violation" }
                    else { "unmapped address" };
                log!("KERNEL PANIC: {} {} at {:#x}", action, cause, ctx.cr2);
            }
            _ => {
                let name = match ctx.vector {
                    Vector::InvalidOpcode => "invalid opcode",
                    Vector::GeneralProtection => "general protection fault",
                    Vector::DoubleFault => "double fault",
                    _ => "exception",
                };
                log!("KERNEL PANIC: {} (error_code={:#x})", name, ctx.frame.error_code);
            }
        }
    }

    // Crash location with symbol resolution
    log!("  rip:");
    if is_user {
        if !process::resolve_user_symbol(tid, ctx.frame.rip) {
            log!("    {:#x}", ctx.frame.rip);
        }
    } else {
        crate::symbols::resolve_kernel(ctx.frame.rip);
    }

    if ctx.vector == Vector::PageFault {
        crate::mm::paging::debug_page_walk(ctx.cr2);
    }

    // Full register dump
    log!("  Registers:");
    log!("    rax={:#018x}  rbx={:#018x}", ctx.regs.rax, ctx.regs.rbx);
    log!("    rcx={:#018x}  rdx={:#018x}", ctx.regs.rcx, ctx.regs.rdx);
    log!("    rsi={:#018x}  rdi={:#018x}", ctx.regs.rsi, ctx.regs.rdi);
    log!("    rbp={:#018x}  rsp={:#018x}", ctx.regs.rbp, ctx.frame.rsp);
    log!("     r8={:#018x}   r9={:#018x}", ctx.regs.r8, ctx.regs.r9);
    log!("    r10={:#018x}  r11={:#018x}", ctx.regs.r10, ctx.regs.r11);
    log!("    r12={:#018x}  r13={:#018x}", ctx.regs.r12, ctx.regs.r13);
    log!("    r14={:#018x}  r15={:#018x}", ctx.regs.r14, ctx.regs.r15);

    // Backtrace with symbol resolution
    log!("  Backtrace:");
    if is_user {
        if !process::resolve_user_symbol(tid, ctx.frame.rip) {
            log!("    {:#x}", ctx.frame.rip);
        }
    } else {
        crate::symbols::resolve_kernel(ctx.frame.rip);
    }
    let mut rbp = ctx.regs.rbp;
    for _ in 0..32 {
        if rbp == 0 || rbp % 8 != 0 { break; }
        let Some(saved_rbp) = safe_read_u64(rbp, pml4) else { break };
        let Some(ret_addr) = safe_read_u64(rbp + 8, pml4) else { break };
        if ret_addr == 0 { break; }
        if is_user {
            if !process::resolve_user_symbol(tid, ret_addr) {
                log!("    {:#x}", ret_addr);
            }
        } else {
            crate::symbols::resolve_kernel(ret_addr);
        }
        rbp = saved_rbp;
    }

    // Stack dump
    if safe_read_u64(ctx.frame.rsp, pml4).is_some() {
        log!("  Stack (from RSP):");
        for i in 0..8u64 {
            let addr = ctx.frame.rsp + i * 8;
            let Some(val) = safe_read_u64(addr, pml4) else { break };
            log!("    [{:#x}] = {:#018x}", addr, val);
        }
    }

    // Full crash diagnostics for user faults
    if is_user {
        let crash_addr = if ctx.vector == Vector::PageFault { ctx.cr2 } else { 0 };
        process::dump_crash_diagnostics(crash_addr, ctx.frame.rip);
    }

    // === Step 3: Terminate ===
    if is_user {
        IN_FATAL.store(false, Ordering::SeqCst);
        IN_PAGE_FAULT.store(false, Ordering::SeqCst);
        syscall::kill_process(-1);
    }
    cpu::halt();
}

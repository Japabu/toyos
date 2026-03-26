use crate::arch::{cpu, debug, syscall, percpu};
use crate::{log, mm, process, symbols};

use super::{Vector, TrapFrame, RPL_MASK, PF_PRESENT, PF_WRITE, PF_INSTRUCTION_FETCH};

// ============================================================
// Raw serial output — allocation-free, cannot cause recursive faults
// ============================================================

/// Write bytes to serial port 0x3F8 using raw port I/O.
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

// ============================================================
// Shared backtrace functions — allocation-free
// ============================================================

/// Walk RBP chain for kernel backtrace with symbol resolution.
pub(crate) fn kernel_backtrace(start_rbp: u64, max_frames: usize) {
    let mut rbp = start_rbp;
    for _ in 0..max_frames {
        if rbp == 0 || rbp % 8 != 0 || !mm::is_kernel_addr(rbp) { break; }
        let saved_rbp = unsafe { *(rbp as *const u64) };
        let return_addr = unsafe { *((rbp + 8) as *const u64) };
        if return_addr == 0 || !mm::is_kernel_addr(return_addr) { break; }
        symbols::resolve_kernel(return_addr);
        rbp = saved_rbp;
    }
}

/// Walk RBP chain for user backtrace through page tables.
fn user_backtrace(tid: crate::process::Tid, start_rbp: u64, pml4: *const u64, max_frames: usize) {
    let mut rbp = start_rbp;
    for _ in 0..max_frames {
        if rbp == 0 || rbp % 8 != 0 { break; }
        let Some(saved_rbp) = safe_read_u64(rbp, pml4) else { break };
        let Some(return_addr) = safe_read_u64(rbp + 8, pml4) else { break };
        if return_addr == 0 { break; }
        if !process::resolve_user_symbol(tid, return_addr) {
            log!("    {:#x}", return_addr);
        }
        rbp = saved_rbp;
    }
}

/// Walk RBP chain using safe kernel reads only (for double fault handler on IST stack).
fn kernel_backtrace_safe(start_rbp: u64, max_frames: usize) {
    let mut rbp = start_rbp;
    for _ in 0..max_frames {
        let Some(saved_rbp) = safe_read_kernel(rbp) else { break };
        let Some(return_addr) = safe_read_kernel(rbp + 8) else { break };
        if return_addr == 0 { break; }
        symbols::resolve_kernel_nonblocking(return_addr);
        rbp = saved_rbp;
    }
}

// ============================================================
// Safe memory reads — for exception handlers
// ============================================================

/// Safe kernel memory read. Only reads kernel direct-map addresses.
fn safe_read_kernel(addr: u64) -> Option<u64> {
    if addr % 8 != 0 || !mm::is_kernel_addr(addr) {
        return None;
    }
    Some(unsafe { core::ptr::read_volatile(addr as *const u64) })
}

/// Safely read a u64 from memory. For user addresses, translates through page
/// tables to avoid triggering demand-paging faults inside exception handlers.
fn safe_read_u64(addr: u64, user_pml4: *const u64) -> Option<u64> {
    if addr % 8 != 0 || addr == 0 {
        return None;
    }
    if !user_pml4.is_null() {
        let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
        let pd_idx = ((addr >> 21) & 0x1FF) as usize;
        let pml4e = unsafe { *user_pml4.add(pml4_idx) };
        if pml4e & 1 == 0 { return None; }
        let pdpt = crate::DirectMap::from_phys(pml4e & 0x000F_FFFF_FFFF_F000).as_ptr::<u64>();
        let pdpte = unsafe { *pdpt.add(pdpt_idx) };
        if pdpte & 1 == 0 { return None; }
        let pd = crate::DirectMap::from_phys(pdpte & 0x000F_FFFF_FFFF_F000).as_ptr::<u64>();
        let pde = unsafe { *pd.add(pd_idx) };
        if pde & 1 == 0 { return None; }
        let page_phys = pde & 0x000F_FFFF_FFE0_0000;
        let offset = addr & (mm::PAGE_2M - 1);
        Some(unsafe { *crate::DirectMap::from_phys(page_phys + offset).as_ptr::<u64>() })
    } else if mm::is_kernel_addr(addr) {
        Some(unsafe { *(addr as *const u64) })
    } else {
        None
    }
}

// ============================================================
// Exception handlers — called from trap_dispatch in mod.rs
// ============================================================

/// #DB handler — logs full context when a hardware watchpoint fires.
/// Returns to resume execution.
pub(super) fn debug_handler(frame: &TrapFrame) {
    unsafe {
        for &b in b"\n!!! DB TRAP !!!\n" {
            core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b);
        }
    }

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
    log!("  rip={:#018x}  rsp={:#018x}  rbp={:#018x}", frame.rip, frame.rsp, frame.rbp);
    log!("  rax={:#018x}  rbx={:#018x}  rcx={:#018x}", frame.rax, frame.rbx, frame.rcx);
    log!("  rdx={:#018x}  rsi={:#018x}  rdi={:#018x}", frame.rdx, frame.rsi, frame.rdi);

    log!("  Instruction that wrote:");
    if is_user {
        if let Some(p) = pid {
            if !process::resolve_user_symbol(p, frame.rip) {
                log!("    {:#x}", frame.rip);
            }
        }
    } else {
        symbols::resolve_kernel(frame.rip);
    }

    log!("  Backtrace:");
    if is_user {
        let pml4 = crate::DirectMap::from_phys(cpu::read_cr3()).as_ptr::<u64>() as *const u64;
        if let Some(p) = pid {
            user_backtrace(p, frame.rbp, pml4, 20);
        }
    } else {
        kernel_backtrace(frame.rbp, 20);
    }

    // Read the watched address to see what was written
    let watched_addr: u64;
    unsafe { core::arch::asm!("mov {}, dr0", out(reg) watched_addr); }
    if mm::is_kernel_addr(watched_addr) && watched_addr % 8 == 0 {
        let val = unsafe { *(watched_addr as *const u64) };
        log!("  Value at watched addr {:#x} = {:#018x}", watched_addr, val);
    }

    log!("=== END WATCHPOINT ===");

    // Clear DR6 so we don't re-trigger, then disable watchpoint
    unsafe {
        core::arch::asm!("mov dr6, {}", in(reg) 0u64);
        core::arch::asm!("mov dr7, {}", in(reg) 0u64);
    }
}

/// Double fault handler — runs on IST1. Always from kernel. Never returns.
pub(super) fn double_fault_handler(frame: &TrapFrame) -> ! {
    let cr2 = cpu::read_cr2();
    let cpu_id = percpu::cpu_id();
    let pid = percpu::current_tid();

    log!("DOUBLE FAULT on CPU {} (pid={:?})", cpu_id, pid);
    log!("  cr2={:#018x} (address that caused the fault chain)", cr2);
    log!("  rip={:#018x}  rsp={:#018x}  rbp={:#018x}", frame.rip, frame.rsp, frame.rbp);
    crate::mm::paging::debug_page_walk(cr2);

    log!("  Kernel backtrace:");
    symbols::resolve_kernel_nonblocking(frame.rip);
    kernel_backtrace_safe(frame.rbp, 20);

    // Scan the original kernel stack for the interrupt frame that started
    // the exception chain. Our entry stubs push [error_code] [vector] then
    // common_entry pushes GPRs. The CPU interrupt frame sits above:
    //   [GPRs (15×8)] [vector (8)] [error_code (8)] [RIP] [CS] [RFLAGS] [RSP] [SS]
    let kernel_rsp = frame.rsp;
    log!("  Scanning kernel stack at {:#x} for original exception context...", kernel_rsp);

    let scan_start = kernel_rsp;
    let scan_end = kernel_rsp.saturating_add(4096);
    let mut addr = scan_start;

    while addr < scan_end {
        let Some(maybe_rip) = safe_read_kernel(addr) else { break };
        let Some(maybe_cs) = safe_read_kernel(addr + 8) else { break };
        let Some(maybe_rflags) = safe_read_kernel(addr + 16) else { break };
        let Some(maybe_rsp) = safe_read_kernel(addr + 24) else { break };

        let valid_cs = maybe_cs == 0x08 || maybe_cs == 0x23;
        let valid_rflags = maybe_rflags & 2 != 0 && maybe_rflags & !0x3F_FFFF == 0;
        let valid_rip = maybe_rip > 0x1000;

        if valid_cs && valid_rflags && valid_rip {
            let is_user = maybe_cs == 0x23;
            log!("  Found interrupt frame at stack offset +{:#x}:", addr - kernel_rsp);
            log!("    rip={:#018x}  cs={:#x}  rflags={:#x}", maybe_rip, maybe_cs, maybe_rflags);
            log!("    rsp={:#018x}", maybe_rsp);

            // error_code is at addr - 8, vector at addr - 16,
            // GPRs start at addr - 16 - 15*8
            let error_code_addr = addr.wrapping_sub(8);
            let saved_regs_base = addr.wrapping_sub(16 + 15 * 8);
            if let Some(error_code) = safe_read_kernel(error_code_addr) {
                log!("    error_code={:#x}", error_code);
            }

            if is_user {
                // Try to recover user RBP from saved GPRs (rbp is at offset 6*8)
                let user_rbp_addr = saved_regs_base + 6 * 8;
                if let Some(user_rbp) = safe_read_kernel(user_rbp_addr) {
                    log!("  User context (pid={:?}):", pid);
                    log!("    rip={:#018x}  rsp={:#018x}  rbp={:#018x}", maybe_rip, maybe_rsp, user_rbp);

                    let pml4 = crate::DirectMap::from_phys(cpu::read_cr3()).as_ptr::<u64>();
                    log!("  User backtrace:");
                    if let Some(p) = pid {
                        if !process::resolve_user_symbol(p, maybe_rip) {
                            log!("    {:#x}", maybe_rip);
                        }
                        user_backtrace(p, user_rbp, pml4, 20);
                    } else {
                        log!("    {:#x}", maybe_rip);
                    }
                }
            } else {
                log!("  Original fault was in kernel code");
                log!("  Kernel backtrace from original fault:");
                symbols::resolve_kernel_nonblocking(maybe_rip);
                let rbp_addr = saved_regs_base + 6 * 8;
                if let Some(orig_rbp) = safe_read_kernel(rbp_addr) {
                    kernel_backtrace_safe(orig_rbp, 20);
                }
            }
            break;
        }

        addr += 8;
    }

    cpu::halt();
}

// ============================================================
// Page fault handler (demand paging)
// ============================================================

/// Returns normally if the fault was resolved (page mapped in).
/// Diverges (never returns) if the fault is fatal.
pub(super) fn page_fault_handler(frame: &TrapFrame) {
    if percpu::swap_in_page_fault(true) {
        let cr2 = cpu::read_cr2();
        let ctx = ExceptionContext { frame, cr2 };
        fatal_exception(&ctx);
    }

    let fault_addr = cpu::read_cr2();

    raw_serial(b"PF cr2=");
    raw_serial_hex(b"", fault_addr);
    raw_serial_hex(b" rip=", frame.rip);
    raw_serial_hex(b" err=", frame.error_code);
    raw_serial_hex(b" cr3=", cpu::read_cr3());
    raw_serial(b"\n");

    // SMAP violation detection
    if frame.error_code & PF_PRESENT != 0 && frame.cs & RPL_MASK == 0
        && mm::is_kernel_addr(fault_addr)
    {
        log!("SMAP cr2={:#018x} rip={:#018x} err={:#018x} rflags={:#018x}",
            fault_addr, frame.rip, frame.error_code, frame.rflags);
        log!("  SMAP kernel backtrace:");
        symbols::resolve_kernel(frame.rip);
        kernel_backtrace(frame.rbp, 20);
    }

    // Only handle not-present faults — protection violations are always fatal
    if frame.error_code & PF_PRESENT == 0 {
        let is_user = frame.cs & RPL_MASK != 0;
        if is_user || percpu::current_tid().is_some() {
            if process::handle_page_fault(fault_addr, frame.error_code) {
                percpu::set_in_page_fault(false);
                return;
            }
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

    let ctx = ExceptionContext { frame, cr2: fault_addr };
    fatal_exception(&ctx);
}

// ============================================================
// Fatal exception handler — shared by #UD, #GP, #PF
// ============================================================

/// Fatal exception handler for #UD and #GP. Never returns.
pub(super) fn exception_handler(frame: &TrapFrame) -> ! {
    let cr2 = if frame.vector == 0x0E { cpu::read_cr2() } else { 0 };
    let ctx = ExceptionContext { frame, cr2 };
    fatal_exception(&ctx);
}

struct ExceptionContext<'a> {
    frame: &'a TrapFrame,
    cr2: u64,
}

impl ExceptionContext<'_> {
    fn vector(&self) -> Vector {
        Vector::from_raw(self.frame.vector)
    }

    fn is_user_mode(&self) -> bool {
        self.frame.cs & RPL_MASK != 0
    }

    fn is_user_fault(&self) -> bool {
        self.is_user_mode()
            || (self.vector() == Vector::PageFault
                && percpu::current_tid().is_some()
                && self.cr2 < 0x0000_8000_0000_0000)
    }
}

/// Core fatal exception logic. Prints diagnostics, then kills process or halts.
///
/// Step 1: Raw serial preamble (guaranteed output).
/// Step 2: Rich diagnostics via log!().
/// Step 3: Terminate.
fn fatal_exception(ctx: &ExceptionContext) -> ! {
    let is_user = ctx.is_user_fault();
    let recursive = percpu::swap_in_fatal(true);

    // === Step 1: Raw serial preamble ===
    let tid_raw = percpu::current_tid().map_or(u32::MAX, |t| t.raw());
    raw_serial_hex(b"\n!!! FAULT rip=", ctx.frame.rip);
    raw_serial_hex(b" cr2=", ctx.cr2);
    raw_serial_hex(b" err=", ctx.frame.error_code);
    raw_serial_hex(b" cr3=", cpu::read_cr3());
    raw_serial_hex(b" rsp=", ctx.frame.rsp);
    raw_serial_hex(b" tid=", tid_raw as u64);
    if recursive { raw_serial(b" RECURSIVE"); }
    raw_serial(b"\n");

    raw_serial(b" stack:");
    let rsp = ctx.frame.rsp;
    for i in 0..8u64 {
        let addr = rsp.wrapping_add(i * 8);
        if !mm::is_kernel_addr(addr) { break; }
        let val = unsafe { *(addr as *const u64) };
        raw_serial_hex(b" ", val);
    }
    raw_serial(b"\n");

    if recursive {
        if is_user {
            percpu::set_in_fatal(false);
            percpu::set_in_page_fault(false);
            syscall::kill_process(-1);
        }
        cpu::halt();
    }

    percpu::set_in_page_fault(false);

    // === Step 2: Rich diagnostics ===
    let tid = percpu::current_tid().unwrap_or(crate::process::Tid(0));
    let pml4 = if is_user { crate::DirectMap::from_phys(cpu::read_cr3()).as_ptr::<u64>() as *const u64 } else { core::ptr::null() };

    // Header
    if is_user {
        match ctx.vector() {
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
            _ => log!("FATAL tid={}: exception {:?}", tid, ctx.vector()),
        }
    } else {
        match ctx.vector() {
            Vector::PageFault => {
                let action = if ctx.frame.error_code & PF_INSTRUCTION_FETCH != 0 { "execute" }
                    else if ctx.frame.error_code & PF_WRITE != 0 { "write" }
                    else { "read" };
                let cause = if ctx.frame.error_code & PF_PRESENT != 0 { "protection violation" }
                    else { "unmapped address" };
                log!("KERNEL PANIC: {} {} at {:#x}", action, cause, ctx.cr2);
            }
            _ => {
                let name = match ctx.vector() {
                    Vector::InvalidOpcode => "invalid opcode",
                    Vector::GeneralProtection => "general protection fault",
                    Vector::DoubleFault => "double fault",
                    _ => "exception",
                };
                log!("KERNEL PANIC: {} (error_code={:#x})", name, ctx.frame.error_code);
            }
        }
    }

    // Crash location
    log!("  rip:");
    if is_user {
        if !process::resolve_user_symbol(tid, ctx.frame.rip) {
            log!("    {:#x}", ctx.frame.rip);
        }
    } else {
        symbols::resolve_kernel(ctx.frame.rip);
    }

    if ctx.vector() == Vector::PageFault {
        crate::mm::paging::debug_page_walk(ctx.cr2);
    }

    // Register dump
    log!("  Registers:");
    log!("    rax={:#018x}  rbx={:#018x}", ctx.frame.rax, ctx.frame.rbx);
    log!("    rcx={:#018x}  rdx={:#018x}", ctx.frame.rcx, ctx.frame.rdx);
    log!("    rsi={:#018x}  rdi={:#018x}", ctx.frame.rsi, ctx.frame.rdi);
    log!("    rbp={:#018x}  rsp={:#018x}", ctx.frame.rbp, ctx.frame.rsp);
    log!("     r8={:#018x}   r9={:#018x}", ctx.frame.r8, ctx.frame.r9);
    log!("    r10={:#018x}  r11={:#018x}", ctx.frame.r10, ctx.frame.r11);
    log!("    r12={:#018x}  r13={:#018x}", ctx.frame.r12, ctx.frame.r13);
    log!("    r14={:#018x}  r15={:#018x}", ctx.frame.r14, ctx.frame.r15);

    // Backtrace
    log!("  Backtrace:");
    if is_user {
        if !process::resolve_user_symbol(tid, ctx.frame.rip) {
            log!("    {:#x}", ctx.frame.rip);
        }
        if let Some(p) = percpu::current_tid() {
            user_backtrace(p, ctx.frame.rbp, pml4, 32);
        }
    } else {
        symbols::resolve_kernel(ctx.frame.rip);
        kernel_backtrace(ctx.frame.rbp, 32);
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
        let crash_addr = if ctx.vector() == Vector::PageFault { ctx.cr2 } else { 0 };
        process::dump_crash_diagnostics(crash_addr, ctx.frame.rip);
    }

    // === Step 3: Terminate ===
    if is_user {
        percpu::set_in_fatal(false);
        percpu::set_in_page_fault(false);
        syscall::kill_process(-1);
    }
    cpu::halt();
}

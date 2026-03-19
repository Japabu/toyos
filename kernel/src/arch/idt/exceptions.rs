use core::arch::naked_asm;

use crate::arch::{cpu, debug, paging, syscall, percpu};
use crate::{log, process};

use super::{Vector, SavedRegs, InterruptFrame, RPL_MASK, PF_PRESENT};

/// Write bytes to serial port 0x3F8 using raw port I/O.
/// No fmt, no allocation, no ptr::add — cannot cause recursive faults.
fn raw_serial(bytes: &[u8]) {
    for &b in bytes {
        unsafe { core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b, options(nomem, nostack)); }
    }
}

/// Write a u64 as hex to serial using raw port I/O.
fn raw_serial_hex(prefix: &[u8], value: u64) {
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
    let pml4 = if is_user { cpu::read_cr3().as_ptr::<u64>() as *const u64 } else { core::ptr::null() };
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
    if paging::is_kernel_addr(watched_addr) && watched_addr % 8 == 0 {
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
        "call {handler}",
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
    if addr % 8 != 0 || !paging::is_kernel_addr(addr) {
        return None;
    }
    Some(unsafe { core::ptr::read_volatile(addr as *const u64) })
}

extern "sysv64" fn double_fault_handler(regs: *const SavedRegs) -> ! {
    let regs = unsafe { &*regs };
    let frame = regs.interrupt_frame();
    let cr2 = cpu::read_cr2().raw();
    let cpu_id = percpu::cpu_id();
    let pid = percpu::current_tid();

    log!("DOUBLE FAULT on CPU {} (pid={:?})", cpu_id, pid);
    log!("  cr2={:#018x} (address that caused the fault chain)", cr2);
    log!("  rip={:#018x}  rsp={:#018x}  rbp={:#018x}", frame.rip, frame.rsp, regs.rbp);
    paging::debug_page_walk(cr2);

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
                    let pml4 = cpu::read_cr3().as_ptr::<u64>();
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
    use core::sync::atomic::{AtomicBool, Ordering};
    static IN_PAGE_FAULT: AtomicBool = AtomicBool::new(false);

    // Detect recursive page faults (e.g. from debug-mode ptr::add precondition checks
    // in the panic/log path). Break the recursion with raw serial output and halt.
    if IN_PAGE_FAULT.swap(true, Ordering::Relaxed) {
        let cr2 = cpu::read_cr2().raw();
        let frame = unsafe { &*regs }.interrupt_frame();
        raw_serial(b"\n!!! RECURSIVE KERNEL #PF");
        raw_serial_hex(b" rip=", frame.rip);
        raw_serial_hex(b" cr2=", cr2);
        raw_serial_hex(b" rsp=", frame.rsp);
        raw_serial(b"\n");
        cpu::halt();
    }

    let regs = unsafe { &*regs };
    let frame = regs.interrupt_frame();
    let fault_addr = cpu::read_cr2().raw();

    // SMAP violation detection: kernel-mode protection fault on a kernel direct-map address.
    // Enable stac immediately so diagnostics don't cascade into another SMAP fault.
    if frame.error_code & PF_PRESENT != 0 && frame.cs & RPL_MASK == 0
        && paging::is_kernel_addr(fault_addr)
    {
        log!("SMAP cr2={:#018x} rip={:#018x} err={:#018x} rflags={:#018x}",
            fault_addr, frame.rip, frame.error_code, frame.rflags);
        log!("  SMAP kernel backtrace:");
        crate::symbols::resolve_kernel(frame.rip);
        let mut rbp = regs.rbp;
        for _ in 0..20 {
            if rbp == 0 || rbp % 8 != 0 || !paging::is_kernel_addr(rbp) { break; }
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
        }
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
        let phys = paging::virt_to_phys(user_pml4, crate::UserAddr::new(addr))?;
        Some(unsafe { *phys.as_ptr::<u64>() })
    } else if paging::is_kernel_addr(addr) {
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
    let cr2 = if vector == Vector::PageFault { cpu::read_cr2().raw() } else { 0 };
    let ctx = ExceptionContext { vector, regs, frame, cr2 };
    fatal_exception(&ctx);
}

/// Core fatal exception logic. Shared by page_fault_handler (when unresolvable)
/// and exception_handler (for all other fatal exceptions).
///
/// Uses raw serial I/O exclusively — no fmt::Write, no log!(), no ptr::add().
/// This prevents recursive page faults from debug-mode precondition checks
/// that would exhaust the stack and triple fault.
fn fatal_exception(ctx: &ExceptionContext) -> ! {
    let is_user = ctx.is_user_fault();

    // --- Header ---
    if is_user {
        raw_serial(b"\n!!! FAULT ");
    } else {
        raw_serial(b"\n!!! KERNEL FAULT ");
    }
    match ctx.vector {
        Vector::PageFault => raw_serial(b"#PF"),
        Vector::InvalidOpcode => raw_serial(b"#UD"),
        Vector::GeneralProtection => raw_serial(b"#GP"),
        Vector::DoubleFault => raw_serial(b"#DF"),
        _ => raw_serial(b"???"),
    }

    // --- Registers ---
    raw_serial_hex(b" rip=", ctx.frame.rip);
    raw_serial_hex(b" cr2=", ctx.cr2);
    raw_serial_hex(b" err=", ctx.frame.error_code);
    raw_serial_hex(b"\n rsp=", ctx.frame.rsp);
    raw_serial_hex(b" cr3=", cpu::read_cr3().raw());
    raw_serial_hex(b" rax=", ctx.regs.rax);
    raw_serial_hex(b" rbx=", ctx.regs.rbx);
    raw_serial_hex(b"\n rcx=", ctx.regs.rcx);
    raw_serial_hex(b" rdx=", ctx.regs.rdx);
    raw_serial_hex(b" rsi=", ctx.regs.rsi);
    raw_serial_hex(b" rdi=", ctx.regs.rdi);
    raw_serial_hex(b"\n rbp=", ctx.regs.rbp);
    raw_serial_hex(b"  r8=", ctx.regs.r8);
    raw_serial_hex(b"  r9=", ctx.regs.r9);
    raw_serial_hex(b" r10=", ctx.regs.r10);
    raw_serial_hex(b"\n r11=", ctx.regs.r11);
    raw_serial_hex(b" r12=", ctx.regs.r12);
    raw_serial_hex(b" r13=", ctx.regs.r13);
    raw_serial_hex(b" r14=", ctx.regs.r14);
    raw_serial_hex(b"\n r15=", ctx.regs.r15);

    // --- Backtrace (frame pointer walk using wrapping_add, never ptr::add) ---
    raw_serial(b"\n backtrace:");
    raw_serial_hex(b"\n  ", ctx.frame.rip);
    let mut rbp = ctx.regs.rbp;
    for _ in 0..20 {
        if rbp == 0 || rbp % 8 != 0 { break; }
        // For kernel frames, read directly. For user frames in kernel context
        // (syscall path), the direct map covers all physical memory.
        if !paging::is_kernel_addr(rbp) && !is_user { break; }
        let ret_addr = unsafe { *((rbp as *const u64).wrapping_add(1)) };
        if ret_addr == 0 { break; }
        raw_serial_hex(b"\n  ", ret_addr);
        rbp = unsafe { *(rbp as *const u64) };
    }
    raw_serial(b"\n");

    // --- Terminate ---
    if is_user {
        // Kill only the faulting process — other processes continue.
        syscall::kill_process(-1);
    }
    cpu::halt();
}

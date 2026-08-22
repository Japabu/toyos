use crate::arch::{apic, cpu, syscall, percpu};
use crate::arch::percpu::CpuFaultState;
use crate::{alert, log, mm, process, scheduler, symbols};

use toyos_userbound::{blame, Blame, Faulted, Ring};

use super::{Vector, TrapFrame, PF_PRESENT, PF_WRITE, PF_INSTRUCTION_FETCH};

/// Walk RBP chain for kernel backtrace with symbol resolution.
pub(crate) fn kernel_backtrace(start_rbp: u64, max_frames: usize) {
    let mut rbp = start_rbp;
    for _ in 0..max_frames {
        if rbp == 0 || !rbp.is_multiple_of(8) || !mm::is_kernel_addr(rbp) { break; }
        // SAFETY: `rbp` is non-zero, 8-aligned and a kernel address, all just
        // checked, so both reads are inside the direct map — which covers every
        // byte of physical memory and is mapped for the life of the machine, so
        // neither can fault. What the words *mean* is not checked and cannot be:
        // this walks a frame chain on a stack that has already failed, and
        // `return_addr` is filtered on the next line rather than trusted.
        //
        // **Irreducible for the frame chain and not for the read.** `rbp` is a
        // register out of a `TrapFrame`, so there is no allocation to borrow and
        // no `KernelSlice` to carry — but the read itself should be a
        // `read_volatile` like `safe_read_kernel`'s, and the argument the two
        // differ on is the root-file sweep's open finding
        // (`issues/kernel/user-pages-still-read-through-a-plain-deref.md`).
        let saved_rbp = unsafe { *(rbp as *const u64) };
        // SAFETY: the same argument, for the return address one word up — `rbp`
        // is 8-aligned and a kernel address, and the direct map is contiguous.
        let return_addr = unsafe { *((rbp + 8) as *const u64) };
        if return_addr == 0 || !mm::is_kernel_addr(return_addr) { break; }
        symbols::resolve_kernel_return(return_addr);
        rbp = saved_rbp;
    }
}

/// Walk RBP chain for user backtrace through page tables.
///
/// The names come off the running task's own symbol table, so this takes no pid:
/// a user backtrace is always the backtrace of the process whose CPU is
/// producing the report.
fn user_backtrace(start_rbp: u64, pml4: *const u64, max_frames: usize) {
    let mut rbp = start_rbp;
    for _ in 0..max_frames {
        if rbp == 0 || !rbp.is_multiple_of(8) { break; }
        let Some(saved_rbp) = safe_read_u64(rbp, pml4) else { break };
        let Some(return_addr) = safe_read_u64(rbp + 8, pml4) else { break };
        if return_addr == 0 { break; }
        process::resolve_user_symbol_return(return_addr).log_bare(return_addr);
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
        symbols::resolve_kernel_return(return_addr);
        rbp = saved_rbp;
    }
}

// Safe memory reads — for exception handlers

/// Safe kernel memory read. Only reads kernel direct-map addresses.
fn safe_read_kernel(addr: u64) -> Option<u64> {
    if !addr.is_multiple_of(8) || !mm::is_kernel_addr(addr) {
        return None;
    }
    // SAFETY: `addr` is 8-aligned and a kernel address, checked immediately
    // above, so the read is inside the direct map and cannot fault. Irreducible
    // for `kernel_backtrace`'s reason: the address is a raw stack word, not a
    // borrow of anything. `read_volatile` because this runs on the crash path
    // and the values it reads are memory another CPU may still be writing.
    Some(unsafe { core::ptr::read_volatile(addr as *const u64) })
}

/// Safely read a u64 from memory. For user addresses, translates through page
/// tables to avoid triggering demand-paging faults inside exception handlers.
fn safe_read_u64(addr: u64, user_pml4: *const u64) -> Option<u64> {
    if !addr.is_multiple_of(8) || addr == 0 {
        return None;
    }
    if !user_pml4.is_null() {
        let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
        let pd_idx = ((addr >> 21) & 0x1FF) as usize;
        // SAFETY: the four reads below are one page walk, and each is guarded
        // by the present bit of the entry before it. `user_pml4` is a direct-map
        // pointer to the live PML4 the caller read out of `CR3`; every index is
        // masked to nine bits, so `add` stays inside that 512-entry table; and
        // each next-level pointer is a direct-map address of the physical frame
        // the previous entry named, which the direct map covers by construction.
        // The last read is `page_phys + offset` with `offset` masked to 2 MiB,
        // so it is inside the leaf the walk just resolved.
        //
        // **This is why the walk is here rather than through `mm::paging`**: the
        // caller is an exception handler on a faulted CPU, which may not take
        // the address space's lock and may not itself demand-page — the whole
        // reason a translation is done by hand instead of dereferencing `addr`.
        // The reads should still be volatile; see `kernel_backtrace`.
        let pml4e = unsafe { *user_pml4.add(pml4_idx) };
        if pml4e & 1 == 0 { return None; }
        let pdpt = crate::DirectMap::from_phys(pml4e & 0x000F_FFFF_FFFF_F000).as_ptr::<u64>();
        // SAFETY: the walk's argument, one level down.
        let pdpte = unsafe { *pdpt.add(pdpt_idx) };
        if pdpte & 1 == 0 { return None; }
        let pd = crate::DirectMap::from_phys(pdpte & 0x000F_FFFF_FFFF_F000).as_ptr::<u64>();
        // SAFETY: the walk's argument, one level down again.
        let pde = unsafe { *pd.add(pd_idx) };
        if pde & 1 == 0 { return None; }
        let page_phys = pde & 0x000F_FFFF_FFE0_0000;
        let offset = addr & (mm::PAGE_2M - 1);
        // SAFETY: the walk's argument — a direct-map read of a byte inside the
        // present 2 MiB leaf the three entries above resolved.
        Some(unsafe { *crate::DirectMap::from_phys(page_phys + offset).as_ptr::<u64>() })
    } else if mm::is_kernel_addr(addr) {
        // SAFETY: `addr` is 8-aligned (checked at the top) and a kernel address
        // (checked in this arm), so it is inside the direct map.
        Some(unsafe { *(addr as *const u64) })
    } else {
        None
    }
}

pub(crate) struct ExceptionContext<'a> {
    frame: &'a TrapFrame,
    cr2: u64,
}

impl ExceptionContext<'_> {
    fn vector(&self) -> Vector {
        Vector::from_raw(self.frame.vector)
    }

    /// The privilege level this frame arrived from.
    fn ring(&self) -> Ring {
        Ring::of_cs(self.frame.cs)
    }

    /// CR2 is meaningful on a #PF and stale on every other vector.
    fn faulted(&self) -> Faulted {
        if self.vector() == Vector::PageFault {
            Faulted::Address(self.cr2)
        } else {
            Faulted::Nothing
        }
    }

    /// Whose fault it was. See `toyos_userbound::fault`.
    fn blame(&self) -> Blame {
        blame(self.ring(), self.frame.rip, self.faulted(), percpu::current_tid().is_some())
    }
}

// DESIGN RULE: crash_report and everything it calls must be panic-free.
// No unwrap/expect/[], no allocation, no blocking locks. try_lock only.
// log!() is verified panic-free (let _ = write!(), serial::write is direct outb).
// Symbol resolution is lock-free (AtomicPtr, linear scan over static ELF data).
//
// "try_lock only" was not sufficient on its own, and the gap was not in this
// file. `Lock::try_lock` raises the preempt count on entry, and both its
// failure path and its guard's `Drop` lower it again — so on the pass that
// takes the count back to zero with `need_resched` set, `preempt::enable`
// dispatched `do_preempt` and the crash report reached the scheduler from
// inside a fault. `panic_console` had already refused `try_lock` for exactly
// this reason and said so in its own comment; the rest of the crash path kept
// using it, and two uses are still behind this one — the process table here,
// and `dump_crash_diagnostics`.
//
// **A symbol is no longer one of them.** `resolve_user_symbol` took the process
// table too, and a `try_lock` that must not wait is one that sometimes loses:
// what it lost was the faulting function's name, on a report that had already
// resolved the same address a line later. It reads the running task's own
// symbols now, with no lock in the path at all — `process`'s module header is
// the rule and `sched::driver::current_symbols` is the read.
//
// Fixed centrally rather than per call site: `preempt::enable` now declines
// the slow path while `PerCpu::fault_state` is non-zero. That is the honest
// place for it — a CPU inside a fault or panic report must not be rescheduled
// whatever it happens to call — and it covers uses this rule has not been
// applied to yet, which chasing call sites would not.

/// What a vector is called in a report.
///
/// Read by `crash_report_exception`, which has the frame in front of it, and by
/// `fatal_exception`, which hands it to `panic::record_fault` before the report
/// runs — so a `DOUBLE PANIC` names the fault it landed on top of in the same
/// words the report would have.
fn vector_name(vector: Vector) -> &'static str {
    match vector {
        Vector::DivideError => "divide error",
        Vector::Debug => "debug",
        Vector::Breakpoint => "breakpoint",
        Vector::Overflow => "overflow",
        Vector::BoundRange => "bound range exceeded",
        Vector::InvalidOpcode => "invalid opcode",
        Vector::DeviceNotAvailable => "device not available",
        Vector::DoubleFault => "double fault",
        Vector::InvalidTss => "invalid TSS",
        Vector::SegmentNotPresent => "segment not present",
        Vector::StackSegment => "stack fault",
        Vector::GeneralProtection => "general protection fault",
        Vector::PageFault => "page fault",
        Vector::X87FloatingPoint => "x87 floating-point exception",
        Vector::AlignmentCheck => "alignment check",
        Vector::MachineCheck => "machine check",
        Vector::SimdFloatingPoint => "SIMD floating-point exception",
        Vector::Virtualization => "virtualization exception",
        Vector::ControlProtection => "control protection",
        // Vectors with a `direct` gate never reach this report: their entries
        // do not go through `trap_dispatch`.
        _ => "exception",
    }
}

/// Source of a crash — either a hardware exception or a Rust panic.
pub(crate) enum CrashInfo<'a> {
    Exception(&'a ExceptionContext<'a>),
    Panic { message: &'a core::panic::PanicInfo<'a>, rbp: u64 },
}

/// Print full crash diagnostics. Used by both fatal_exception and the panic handler.
pub(crate) fn crash_report(info: &CrashInfo) {
    match info {
        CrashInfo::Exception(ctx) => crash_report_exception(ctx),
        CrashInfo::Panic { message, rbp } => crash_report_panic(message, *rbp),
    }
}

fn crash_report_exception(ctx: &ExceptionContext) {
    // **The verdict follows the blame and the report follows the ring**, and
    // they are two questions. A pointer that crossed the syscall boundary is
    // the process's fault and the process is what dies — but the frame that
    // faulted is Ring 0, so its `rip` is kernel text and its `rbp` walks a
    // kernel stack. One `is_user` used to answer both, which is why that case
    // resolved a kernel address through the process's symbol table and printed
    // a user backtrace off a kernel frame pointer.
    let theirs = ctx.blame() != Blame::Kernel;
    let ring3 = ctx.ring().is_user();
    let tid = percpu::current_tid().unwrap_or(crate::process::Tid(0));
    let pid = percpu::current_pid();
    let pml4 = if ring3 { crate::DirectMap::from_phys(crate::mm::paging::Cr3::current().phys()).as_ptr::<u64>() } else { core::ptr::null() };

    let (pf_action, pf_cause) = if ctx.vector() == Vector::PageFault {
        let action = if ctx.frame.error_code & PF_INSTRUCTION_FETCH != 0 { "execute" }
            else if ctx.frame.error_code & PF_WRITE != 0 { "write" }
            else { "read" };
        let cause = if ctx.frame.error_code & PF_PRESENT != 0 { "protection violation" }
            else { "unmapped address" };
        (action, cause)
    } else {
        ("", "")
    };

    let name = vector_name(ctx.vector());

    if theirs {
        match ctx.vector() {
            Vector::PageFault => log!("SEGFAULT tid={}: {} {} at {:#x}", tid, pf_action, pf_cause, ctx.cr2),
            Vector::InvalidOpcode => log!("SIGILL tid={}: illegal instruction", tid),
            Vector::DivideError | Vector::X87FloatingPoint | Vector::SimdFloatingPoint => {
                log!("SIGFPE tid={}: {}", tid, name)
            }
            Vector::GeneralProtection | Vector::StackSegment | Vector::AlignmentCheck => {
                log!("SIGBUS tid={}: {} (error_code={:#x})", tid, name, ctx.frame.error_code)
            }
            _ => log!("FATAL tid={}: {}", tid, name),
        }
    } else {
        match ctx.vector() {
            Vector::PageFault => log!("KERNEL PANIC: {} {} at {:#x}", pf_action, pf_cause, ctx.cr2),
            _ => log!("KERNEL PANIC: {} (error_code={:#x})", name, ctx.frame.error_code),
        }
    }

    log!("  rip:");
    if ring3 {
        if pid.is_some() {
            process::resolve_user_symbol(ctx.frame.rip).log_bare(ctx.frame.rip);
        } else {
            log!("    {:#x}", ctx.frame.rip);
        }
    } else {
        symbols::resolve_kernel(ctx.frame.rip);
    }

    if ctx.vector() == Vector::PageFault {
        crate::mm::paging::debug_page_walk(ctx.cr2);
    }

    log!("  Registers:");
    log!("    rax={:#018x}  rbx={:#018x}", ctx.frame.rax, ctx.frame.rbx);
    log!("    rcx={:#018x}  rdx={:#018x}", ctx.frame.rcx, ctx.frame.rdx);
    log!("    rsi={:#018x}  rdi={:#018x}", ctx.frame.rsi, ctx.frame.rdi);
    log!("    rbp={:#018x}  rsp={:#018x}", ctx.frame.rbp, ctx.frame.rsp);
    log!("     r8={:#018x}   r9={:#018x}", ctx.frame.r8, ctx.frame.r9);
    log!("    r10={:#018x}  r11={:#018x}", ctx.frame.r10, ctx.frame.r11);
    log!("    r12={:#018x}  r13={:#018x}", ctx.frame.r12, ctx.frame.r13);
    log!("    r14={:#018x}  r15={:#018x}", ctx.frame.r14, ctx.frame.r15);
    // A #GP error code is a selector or it is nothing, and a selector says
    // nothing without the segments the faulting context was running with.
    log!("    cs={:#06x}  ss={:#06x}  rflags={:#018x}",
        ctx.frame.cs, ctx.frame.ss, ctx.frame.rflags);

    // **Ahead of both backtraces, because a crash report can die before it
    // finishes.** A 2026-08-20 storm capture of the `BTreeMap` class ended
    // `FAULT rip=… cr2=0x0 … RECURSIVE` one line into the user backtrace, and
    // everything the report had left to say went with it. This is the part that
    // decides between the two readings of that class, so it goes where a later
    // fault cannot take it.
    //
    // Only where the kernel is the one that failed: a Ring 3 fault says nothing
    // about which CPU is on which kernel stack, and these lines under every user
    // segfault would bury the report that is about the process.
    if !theirs {
        crate::hw::report_contexts(ctx.frame.rsp, None);
    }

    log!("  Backtrace:");
    if ring3 {
        if pid.is_some() {
            user_backtrace(ctx.frame.rbp, pml4, 32);
        }
    } else {
        kernel_backtrace(ctx.frame.rbp, 32);

        // The `Syscall:` line below is the faulting thread's *last* syscall and
        // says nothing about where the fault is: `syscall_rip` is never cleared
        // (`issues/panic-path/syscall-rip-never-cleared.md`), so it can name a return address
        // many syscalls old. Reading it as the fault site cost the AMD `#GP`
        // investigation its first day.
        let user_rip = percpu::syscall_rip();
        if user_rip != 0 && pid.is_some() {
            log!("  Syscall: num={} user_rip={:#x} user_rsp={:#x}",
                percpu::syscall_num(), user_rip, percpu::user_rsp());
            log!("  User backtrace:");
            process::resolve_user_symbol(user_rip).log_bare(user_rip);
            let pml4 = crate::DirectMap::from_phys(crate::mm::paging::Cr3::current().phys()).as_ptr::<u64>();
            user_backtrace(percpu::syscall_rbp(), pml4, 20);
        }
    }

    if safe_read_u64(ctx.frame.rsp, pml4).is_some() {
        log!("  Stack (from RSP):");
        for i in 0..8u64 {
            let addr = ctx.frame.rsp + i * 8;
            let Some(val) = safe_read_u64(addr, pml4) else { break };
            log!("    [{:#x}] = {:#018x}", addr, val);
        }
    }

    if theirs {
        let crash_addr = if ctx.vector() == Vector::PageFault { ctx.cr2 } else { 0 };
        process::dump_crash_diagnostics(crash_addr, ctx.frame.rip);
    }
}

fn crash_report_panic(info: &core::panic::PanicInfo, rbp: u64) {
    // Before the first word of the report, which is the state
    // `panic::record_panic` exists to survive: what the machine says now comes
    // from the copy the handler took, or it is `DOUBLE PANIC` and nothing else.
    if crate::actuator::panic_in_report() {
        panic!("panic-in-report: the crash report panicked before it said anything");
    }
    alert!("PANIC: {}", info);

    log!("  Backtrace:");
    kernel_backtrace(rbp, 20);

    // **A panic is where this class of defect actually surfaces**, which is why
    // it is here and not only on the fault path: the two `BTreeMap` deaths and
    // the two `cpu N has no CpuSched` deaths on record are all Rust panics with
    // no register dump at all, and every one of them turns on whether a sibling
    // was standing on this stack. The address of a local is the stack pointer
    // the containment test wants — this frame is on the crashing stack, which is
    // the whole of what it asks.
    let here = 0u64;
    crate::hw::report_contexts(core::ptr::addr_of!(here) as u64, None);

    // Process/thread context (try_lock only)
    if let Some(pid) = percpu::current_pid() {
        let tid = percpu::current_tid();
        log!("  Running: pid={} tid={:?}", pid, tid);
        if let Some(guard) = process::PROCESS_TABLE.try_lock() {
            if let Some(table) = guard.as_ref() {
                if let Some(proc) = table.get(pid) {
                    log!("  Process: {} pid={} state={}", proc.name_str(), proc.pid(), if proc.tearing_down() { "TearingDown" } else { "Live" });
                }
            }
        }

        let user_rip = percpu::syscall_rip();
        if user_rip != 0 {
            log!("  Syscall: num={} user_rip={:#x} user_rsp={:#x}",
                percpu::syscall_num(), user_rip, percpu::user_rsp());
            log!("  User backtrace:");
            process::resolve_user_symbol(user_rip).log_bare(user_rip);
            let pml4 = crate::DirectMap::from_phys(crate::mm::paging::Cr3::current().phys()).as_ptr::<u64>();
            user_backtrace(percpu::syscall_rbp(), pml4, 20);
        }
    }
}

/// Terminate after a fatal fault.
///
/// One argument, and it is [`Blame`]: this used to take `is_user` and
/// `is_ring3` as separate `bool`s, whose fourth combination — a user fault from
/// a frame that was not Ring 3 and was not in a syscall either — meant nothing
/// and was writable all the same. The three arms below are the three states
/// there are.
pub(crate) fn recover_or_halt(blame: Blame) -> ! {
    match blame {
        // True user-mode fault — no kernel locks held, safe to use normal exit.
        Blame::Process => {
            percpu::set_fault_state(CpuFaultState::Normal);
            crate::panic::forget();
            syscall::kill_process(-1);
        }
        // Kernel fault on the thread's behalf — may hold locks, use try_lock path.
        Blame::ProcessThroughKernel => try_recover_from_panic(),
        Blame::Kernel => apic::halt_all_cpus(),
    }
}

/// Recover from a panic in syscall context. Hands the faulted thread to the
/// idle loop through the poison set, then rejoins the scheduler via lock-free
/// schedule_no_return.
///
/// Nothing here touches the process table, and that is the point. The faulted
/// thread may hold any kernel lock, including the table's, so blocking on it
/// can deadlock and a `try_lock` can fail. Cleanup has exactly one home: the
/// poison set is both the "do not re-schedule" mark and the cleanup request,
/// and `schedule_no_return` jumps into `cpu_idle_loop`, which reaps it —
/// zombify plus the waiter's wake — before it picks another task.
pub(crate) fn try_recover_from_panic() -> ! {
    if let Some(tid) = percpu::current_tid() {
        let pid = percpu::current_pid().unwrap_or(crate::process::Pid(u32::MAX));
        scheduler::poison_tid(scheduler::TaskId(pid, tid));
    }
    percpu::set_fault_state(CpuFaultState::Normal);
    // The crash this CPU was in is over, so its captured evidence dies with it
    // — the same reason `panic_console::discard_capture` is called beside this
    // on the panic path. Left standing, the next `DOUBLE PANIC` on this CPU
    // would name a panic the machine survived an hour ago as the crash it had
    // just landed on top of.
    crate::panic::forget();
    scheduler::schedule_no_return();
}

// Exception handlers — called from trap_dispatch in mod.rs

/// Double fault handler — runs on IST1. Always from kernel. Never returns.
pub(super) fn double_fault_handler(frame: &TrapFrame) -> ! {
    let cr2 = cpu::read_cr2();
    let cpu_id = percpu::cpu_id();
    let tid = percpu::current_tid();
    let pid = percpu::current_pid();

    log!("DOUBLE FAULT on CPU {} (pid={:?} tid={:?})", cpu_id, pid, tid);
    log!("  cr2={:#018x} (address that caused the fault chain)", cr2);
    log!("  rip={:#018x}  rsp={:#018x}  rbp={:#018x}", frame.rip, frame.rsp, frame.rbp);
    crate::mm::paging::debug_page_walk(cr2);

    log!("  Kernel backtrace:");
    symbols::resolve_kernel(frame.rip);
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

        let valid_cs =
            maybe_cs == u64::from(percpu::KERNEL_CS) || maybe_cs == u64::from(percpu::USER_CS);
        let valid_rflags = maybe_rflags & 2 != 0 && maybe_rflags & !0x3F_FFFF == 0;
        let valid_rip = maybe_rip > 0x1000;

        if valid_cs && valid_rflags && valid_rip {
            let is_user = maybe_cs == u64::from(percpu::USER_CS);
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
                    log!("  User context (pid={:?} tid={:?}):", pid, tid);
                    log!("    rip={:#018x}  rsp={:#018x}  rbp={:#018x}", maybe_rip, maybe_rsp, user_rbp);

                    let pml4 = crate::DirectMap::from_phys(crate::mm::paging::Cr3::current().phys()).as_ptr::<u64>();
                    log!("  User backtrace:");
                    if pid.is_some() {
                        process::resolve_user_symbol(maybe_rip).log_bare(maybe_rip);
                        user_backtrace(user_rbp, pml4, 20);
                    } else {
                        log!("    {:#x}", maybe_rip);
                    }
                }
            } else {
                log!("  Original fault was in kernel code");
                log!("  Kernel backtrace from original fault:");
                symbols::resolve_kernel(maybe_rip);
                let rbp_addr = saved_regs_base + 6 * 8;
                if let Some(orig_rbp) = safe_read_kernel(rbp_addr) {
                    kernel_backtrace_safe(orig_rbp, 20);
                }
            }
            break;
        }

        addr += 8;
    }

    apic::halt_all_cpus();
}

/// #MC — an abort, and the one exception a Ring 3 frame does not make the
/// process's fault. There is no instruction to return to and the state that
/// reported it is not trustworthy, so this halts whichever ring it came from
/// rather than killing a process and carrying on over a broken machine.
///
/// Deliverable and untested: firmware leaves CR4.MCE set and this kernel never
/// clears it, so a machine check arrives here rather than shutting the
/// processor down. Nothing in the suite can stage one.
pub(super) fn machine_check_handler(frame: &TrapFrame) -> ! {
    log!("MACHINE CHECK on CPU {}", percpu::cpu_id());
    let ctx = ExceptionContext { frame, cr2: 0 };
    crash_report(&CrashInfo::Exception(&ctx));
    apic::halt_all_cpus();
}

/// Returns normally if the fault was resolved (page mapped in).
/// Diverges (never returns) if the fault is fatal.
pub(super) fn page_fault_handler(frame: &TrapFrame) {
    if percpu::swap_fault_state(percpu::CpuFaultState::PageFault) != percpu::CpuFaultState::Normal {
        let cr2 = cpu::read_cr2();
        let ctx = ExceptionContext { frame, cr2 };
        fatal_exception(&ctx);
    }

    let fault_addr = cpu::read_cr2();

    if frame.error_code & PF_PRESENT != 0 && !Ring::of_cs(frame.cs).is_user()
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
        let is_user = Ring::of_cs(frame.cs).is_user();
        if is_user || percpu::current_tid().is_some() {
            if process::handle_page_fault(fault_addr, frame.error_code) {
                percpu::set_fault_state(percpu::CpuFaultState::Normal);
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

// Fatal exception handler — shared by #UD, #GP, #PF

/// Fatal exception handler for #UD and #GP. Never returns.
pub(super) fn exception_handler(frame: &TrapFrame) -> ! {
    let cr2 = if frame.vector == 0x0E { cpu::read_cr2() } else { 0 };
    let ctx = ExceptionContext { frame, cr2 };
    fatal_exception(&ctx);
}

/// Core fatal exception logic. Prints diagnostics, then kills process or halts all CPUs.
fn fatal_exception(ctx: &ExceptionContext) -> ! {
    let blame = ctx.blame();
    let prev = percpu::swap_fault_state(CpuFaultState::Fatal);
    let recursive = prev == CpuFaultState::Fatal || prev == CpuFaultState::Panic;

    // Before this fault has said one word about itself, because a panic taken
    // anywhere below — inside `emit`, inside a symbol walk, inside the page
    // walk — reaches the panic handler with this CPU already `Fatal` and gets
    // the `DOUBLE PANIC` arm, which can then only report what was captured
    // here. `panic.rs` owns the argument; the ordering is the whole of it.
    crate::panic::record_fault(
        vector_name(ctx.vector()),
        ctx.frame.rip,
        ctx.cr2,
        ctx.frame.error_code,
    );
    if crate::actuator::panic_in_report() {
        panic!("panic-in-report: the crash report panicked before it said anything");
    }

    let tid_raw = percpu::current_tid().map_or(u32::MAX, |t| t.raw());
    if recursive {
        alert!("FAULT rip={:#018x} cr2={:#018x} err={:#018x} cr3={:#018x} rsp={:#018x} tid={} RECURSIVE",
            ctx.frame.rip, ctx.cr2, ctx.frame.error_code, cpu::read_cr3(), ctx.frame.rsp, tid_raw);
    } else {
        alert!("FAULT rip={:#018x} cr2={:#018x} err={:#018x} cr3={:#018x} rsp={:#018x} tid={}",
            ctx.frame.rip, ctx.cr2, ctx.frame.error_code, cpu::read_cr3(), ctx.frame.rsp, tid_raw);
    }

    // A second fault while reporting the first: no second report, and the
    // normal exit path even for the `ProcessThroughKernel` case — this CPU is
    // not going to survive `try_recover_from_panic`'s rejoin either way, and
    // ending the process is the only thing left that can keep the machine.
    if recursive {
        if blame != Blame::Kernel {
            percpu::set_fault_state(CpuFaultState::Normal);
            crate::panic::forget();
            syscall::kill_process(-1);
        }
        apic::halt_all_cpus();
    }

    crash_report(&CrashInfo::Exception(ctx));
    recover_or_halt(blame);
}

//! Kernel threads: a task with no address space of its own, and the one place
//! that says what a panic inside one means.
//!
//! `specs/log-architecture-spec.md` §4.3 is the design. L3 builds it for one
//! thread, `klogd`; the completion branch's C6 spawns `usbd` and `iod` on it
//! and adds their two rows to [`ROWS`].
//!
//! **A kernel thread is not a special kind of task.** It is an ordinary task
//! whose `NewTask::address_space` is `None` — `driver::spawn` then names the
//! kernel's own `cr3`, which is what every CPU is already in between two user
//! threads — reached through a trampoline that never issues an `iretq`
//! (`loader::start::kernel_start`). It is preemptible, it is stealable, it
//! shows up in `ps` and in Ctrl+Alt+D, and it logs like anything else.
//!
//! It gets a process-table entry rather than a bare task, and that is what
//! makes it nameable: `share_for` is keyed by `Pid`, `sched::dump`'s census
//! walks the table, and `crash_report_panic` prints the process's *name* — so
//! without an entry a panicking kernel thread would report a pid nothing in the
//! machine could resolve.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::process::{
    ElfInfo, Endowments, PageFaultTrace, ProcessAccounting, ProcessData, ProcessEntry, ThreadData,
    ThreadEntry, PROCESS_TABLE, THREAD_NAME_LEN,
};
use crate::scheduler::{self, TaskId};
use crate::symbols::SymbolTable;
use crate::sync::Lock;

use super::payload::ThreadSched;

/// How many kernel threads the machine may have.
///
/// Three, which is every one either open design names: `klogd` here, and
/// `usbd` and `iod` from the completion branch's C6. A fourth is a design
/// decision and gets to notice that it is one.
const MAX_KERNEL_TASKS: usize = 3;

/// No task. `TaskId::pack` puts a `Pid` in the high word and a `Tid` in the
/// low one, and neither id map ever issues `u32::MAX`, so this collides with
/// nothing an entry can hold.
const NO_TASK: u64 = u64::MAX;

/// One kernel thread, and whether a panic inside it may be recovered from.
///
/// **The column exists because the ordinary predicate is not merely wrong for
/// a kernel thread, it is nondeterministic.** `main.rs`'s panic handler
/// recovers when `percpu::syscall_rip() != 0 && percpu::current_tid().is_some()`
/// — and `syscall_rip` is *never cleared*
/// (`specs/issues/panic-path/syscall-rip-never-cleared.md`, and
/// `arch/idt/exceptions.rs` says so in its own comment). A kernel thread has a
/// tid, so the second clause holds; the first reads whatever user thread last
/// ran on *this* CPU left behind. The same panic on the same build therefore
/// recovers or halts depending on which CPU the thread happened to be
/// scheduled on. The row is what makes the answer a property of the thread.
struct Row {
    task: AtomicU64,
    recoverable: AtomicU64,
}

/// Every kernel thread, registered at spawn and never removed: these do not
/// exit.
static ROWS: [Row; MAX_KERNEL_TASKS] =
    [const { Row { task: AtomicU64::new(NO_TASK), recoverable: AtomicU64::new(0) } };
        MAX_KERNEL_TASKS];

/// What a kernel thread's panic does.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OnPanic {
    /// The thread is killed and the machine carries on, which is what a
    /// kernel thread whose absence is survivable and *visible* may ask for.
    Recover,
    /// The machine halts with a report.
    ///
    /// `klogd`'s answer, and the reason is its own: it is the machine's only
    /// console drainer on a live machine, so a killed `klogd` is a machine that
    /// goes quiet with nothing left able to say so. That is the exact failure
    /// the panel exists to make impossible, and it is why the recoverable
    /// branch may not be reached by accident.
    Halt,
}

/// The row of the task this CPU is running, if it is a kernel thread.
///
/// **Two per-CPU words and at most [`MAX_KERNEL_TASKS`] relaxed loads**, and
/// the cheapness is a requirement rather than a nicety: one caller is the panic
/// handler, which may hold any lock and may not fault, and the other is
/// `scheduler::blocking_baseline`, which runs on every blocking call in the
/// machine **with preemption still on**. Asking the `CpuSched` instead — the
/// structural question, "was this task given an address space" — is what the
/// first draft did, and it is unsound from a preemptible context: `with_cpu`
/// hands a pass `&mut CpuSched`, so a timer landing inside the read aliases it
/// and the running task's record may be moving underneath. The identity words
/// cannot move under their own thread.
fn current_row() -> Option<&'static Row> {
    let (Some(pid), Some(tid)) = (
        crate::arch::percpu::current_pid(),
        crate::arch::percpu::current_tid(),
    ) else {
        return None;
    };
    let packed = TaskId(pid, tid).pack();
    ROWS.iter().find(|row| row.task.load(Ordering::Relaxed) == packed)
}

/// Is the task this CPU is running a kernel thread?
///
/// A pid a row holds is never reused: these threads do not exit.
pub fn current_is_kernel_thread() -> bool {
    current_row().is_some()
}

/// Whether a panic on the task this CPU is running is recoverable, or `None`
/// when the running task is not a kernel thread and the ordinary predicate
/// decides.
pub fn panic_recovers_here() -> Option<bool> {
    Some(current_row()?.recoverable.load(Ordering::Relaxed) != 0)
}

/// Start a kernel thread running `body(arg)` on its own kernel stack.
///
/// Returns the scheduler faces of the new task; `klogd`'s wake reaches it
/// through the `shared` half without going near the process table.
///
/// Panics on failure. Every caller is kernel init: a machine that cannot
/// allocate a 16 KiB stack at boot has nothing to fall back to, and a kernel
/// thread that silently did not start is the failure this whole subsystem
/// exists to make impossible.
pub fn spawn(name: &str, body: extern "C" fn(u64) -> !, arg: u64, on_panic: OnPanic) -> ThreadSched {
    let (stack, entry_rsp) = crate::loader::alloc_kernel_stack(
        crate::loader::kernel_start,
        body as usize as u64,
        0,
        arg,
    )
    .unwrap_or_else(|| panic!("kthread: no kernel stack for {name}"));

    let mut short = [0u8; THREAD_NAME_LEN];
    let len = name.len().min(THREAD_NAME_LEN - 1);
    short[..len].copy_from_slice(&name.as_bytes()[..len]);

    // The whole insert-then-place sequence under one hold of the table lock,
    // exactly as `loader::spawn` does it and for the same reason: once the pid
    // is visible its main thread is already in the scheduler.
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("kthread: spawned before process::init");
    let pid = table.insert_with(|pid| {
        ProcessEntry::new(
            pid,
            short,
            Arc::new(Lock::new(kernel_process_data(name))),
            Arc::new(Lock::new(SymbolTable::empty())),
            ThreadEntry::new(Arc::new(Lock::new(kernel_thread_data()))),
        )
    });
    let tid = table.get(pid).expect("kthread: the entry just inserted is gone").main_tid();
    let sched = scheduler::enqueue_new(TaskId(pid, tid), stack, entry_rsp, None, 0);
    table
        .get_mut(pid)
        .and_then(|p| p.threads_mut().get_mut(tid))
        .expect("kthread: the thread just inserted is gone")
        .set_sched(sched.clone());
    drop(guard);

    register(TaskId(pid, tid), on_panic, name);
    crate::log!(
        "kthread: {name} pid={} tid={} runs in the kernel address space; a panic in it {}",
        pid,
        tid,
        match on_panic {
            OnPanic::Halt => "halts the machine",
            OnPanic::Recover => "kills the thread",
        }
    );
    sched
}

fn register(id: TaskId, on_panic: OnPanic, name: &str) {
    let packed = id.pack();
    for row in &ROWS {
        if row
            .task
            .compare_exchange(NO_TASK, packed, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            row.recoverable
                .store(u64::from(on_panic == OnPanic::Recover), Ordering::Relaxed);
            return;
        }
    }
    panic!("kthread: {name} is the {}th kernel thread and there is room for {MAX_KERNEL_TASKS}", MAX_KERNEL_TASKS + 1);
}

/// A process record for a thread that has no user half at all.
///
/// Every field is the empty value rather than a plausible one: there is no ELF,
/// no TLS, no stack in user memory, no handle and no endowment, and a kernel
/// thread that ever reached one of them would be reaching for something that
/// was never there.
fn kernel_process_data(name: &str) -> ProcessData {
    ProcessData {
        handles: crate::object::HandleTable::new(),
        cwd: String::from("/"),
        env: Vec::new(),
        elf: ElfInfo::none(),
        mmap_regions: Vec::new(),
        pipe_maps: Vec::new(),
        demand_pages: Vec::new(),
        fault_trace: PageFaultTrace::new(),
        peak_memory: 0,
        alloc_count: 0,
        free_count: 0,
        exe_path: String::from(name),
        spawn_ns: crate::clock::nanos_since_boot(),
        accounting: ProcessAccounting::default(),
        endowments: Endowments::empty(),
    }
}

fn kernel_thread_data() -> ThreadData {
    ThreadData {
        tls_pages: None,
        stack_pages: None,
        user_stack_base: crate::mm::UserAddr::new(0),
        user_stack_size: 0,
        syscall_counts: [0; toyos_abi::syscall::SYSCALL_PROFILE_BINS],
        syscall_total: 0,
        syscall_total_ns: 0,
    }
}

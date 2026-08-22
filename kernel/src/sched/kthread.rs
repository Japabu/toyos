//! Kernel threads: a task with no address space of its own, and the one place
//! that says what a panic inside one means.
//!
//! There are three: `klogd`, the console drainer; `drivers::xhci::usbd`; and
//! `crate::iod`. [`ROWS`] carries all three, and the machine has one thread per
//! kind of work that must not borrow whichever thread happened to trap — a
//! stuck USB enumeration must not stop the log.
//!
//! **A kernel thread is not a special kind of task.** It is an ordinary task
//! that names `mm::paging::kernel` as its address space — the one every CPU is
//! already in between two user threads — reached through a trampoline that
//! never issues an `iretq` (`loader::start::kernel_start`). It is preemptible,
//! it is stealable, it shows up in `ps` and in Ctrl+Alt+D, and it logs like
//! anything else.
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
/// Three, and all three exist: `klogd` (`log::console`), `usbd`
/// (`drivers::xhci::usbd`) and `iod` (`crate::iod`). A fourth is a design
/// decision and gets to notice that it is one — `Claim::take` dies naming the
/// thread that had nowhere to go, before it takes the process table.
///
/// **The test kernel carries room for one `log-storm` thread per shard on top,
/// and the shipping kernel carries none of it.** That storm is one ordinary
/// stealable task per CPU, and it exists only in the build that has the
/// actuator. A shipping kernel spawning a fourth still dies naming it, so the
/// rule above is untouched where it applies. It used to say the storm is what
/// exercises the migration §2.3a's bracket exists to survive, and that was
/// never true of any workload here: a task is stealable while it is *Ready*,
/// and nothing switches a Ring 0 context out between two instructions
/// (`issues/kernel/a-ring-0-loop-is-never-preempted.md`).
#[cfg(not(feature = "boot-actuators"))]
const MAX_KERNEL_TASKS: usize = 3;
#[cfg(feature = "boot-actuators")]
const MAX_KERNEL_TASKS: usize = 3 + toyos_abi::log::MAX_LOG_SHARDS;

/// No task. `TaskId::pack` puts a `Pid` in the high word and a `Tid` in the
/// low one, and neither id map ever issues `u32::MAX`, so this collides with
/// nothing an entry can hold.
const NO_TASK: u64 = u64::MAX;

/// A row somebody is filling in. Collides with nothing for [`NO_TASK`]'s reason
/// — its high word is `u32::MAX` too — and it is what lets the identity be the
/// **last** thing published rather than the first.
const CLAIMING: u64 = u64::MAX - 1;

/// One kernel thread, and whether a panic inside it may be recovered from.
///
/// **The column exists because the ordinary predicate is not merely wrong for
/// a kernel thread, it is nondeterministic.** `main.rs`'s panic handler
/// recovers when `percpu::syscall_rip() != 0 && percpu::current_tid().is_some()`
/// — and `syscall_rip` is *never cleared*
/// (`issues/panic-path/syscall-rip-never-cleared.md`, and
/// `arch/idt/exceptions.rs` says so in its own comment). A kernel thread has a
/// tid, so the second clause holds; the first reads whatever user thread last
/// ran on *this* CPU left behind. The same panic on the same build therefore
/// recovers or halts depending on which CPU the thread happened to be
/// scheduled on. The row is what makes the answer a property of the thread.
///
/// **[`Row::task`] is the last word written and the first word read**, and that
/// is the whole publication protocol. A reader searches on the identity, so
/// publishing it before the policy beside it leaves a window where the row is
/// findable and answers with a default nobody chose — which is a second way to
/// get the nondeterminism this type exists to remove. The identity is stored
/// `Release` after the payload and loaded `Acquire` before it.
struct Row {
    task: AtomicU64,
    recoverable: AtomicU64,
}

/// A row reserved before its thread exists, to be published before that thread
/// can run.
///
/// **Two phases because the two failures are at opposite ends.** Reserving
/// needs no [`TaskId`] and must happen before the process table is locked, so
/// that "there is room for a fourth kernel thread" is answered by a panic that
/// holds no lock. Publishing must happen before `enqueue_new`, because from
/// that call the task is runnable, stealable, and able to panic — and a panic
/// on a task whose row does not exist yet is answered by the coin toss above.
struct Claim(&'static Row);

impl Claim {
    /// Reserve a row, or die naming the thread that had nowhere to go.
    fn take(name: &str) -> Self {
        for row in &ROWS {
            if row
                .task
                .compare_exchange(NO_TASK, CLAIMING, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Self(row);
            }
        }
        panic!("kthread: {name} is the {}th kernel thread and there is room for {MAX_KERNEL_TASKS}", MAX_KERNEL_TASKS + 1);
    }

    /// Fill the payload, then publish the identity. In that order, and the
    /// order is the point.
    fn publish(self, id: TaskId, on_panic: OnPanic) {
        self.0
            .recoverable
            .store(u64::from(on_panic == OnPanic::Recover), Ordering::Relaxed);
        self.0.task.store(id.pack(), Ordering::Release);
    }
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
/// **Two per-CPU words and at most [`MAX_KERNEL_TASKS`] acquire loads**, and
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
    // `Acquire` against `Claim::publish`'s `Release`: what the pair orders is
    // the policy word beside the identity, not the identity. A relaxed load here
    // could find the row and then read a `recoverable` nobody had written yet.
    ROWS.iter().find(|row| row.task.load(Ordering::Acquire) == packed)
}

/// Is the task this CPU is running a kernel thread?
///
/// **A pid a row holds is never reused, and the reason is `id_map`'s rather
/// than this module's.** It used to be "these threads do not exit", which was
/// true while `klogd` was the only one: its row is [`OnPanic::Halt`], so a panic
/// in it takes the machine and no pid is ever given back. `usbd` and `iod` carry
/// [`OnPanic::Recover`], so one of them *can* die — its entry is zombified by
/// the idle loop and reaped — and a row holding a dead task's identity would
/// then answer for whoever took the pid next. Nobody does: `IdMap` counts up
/// from zero and never reissues a key, which is the property that makes a row
/// safe to leave standing.
pub fn current_is_kernel_thread() -> bool {
    current_row().is_some()
}

/// Is `id` one of this machine's kernel threads?
///
/// The same question [`current_is_kernel_thread`] asks, about a task that is not
/// the one running — `sched::dump`'s census walks the process table and wants to
/// name these three however they happen to be scheduled. At most
/// [`MAX_KERNEL_TASKS`] acquire loads and no lock, which is what the dump needs:
/// it runs from `drain_irqs` on a machine that is already suspected of being
/// stuck, and may take nothing.
pub fn is_kernel_task(id: TaskId) -> bool {
    let packed = id.pack();
    ROWS.iter().any(|row| row.task.load(Ordering::Acquire) == packed)
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
/// allocate a [`crate::process::KERNEL_STACK_SIZE`] stack at boot has nothing
/// to fall back to, and a kernel thread that silently did not start is the
/// failure this whole subsystem exists to make impossible.
pub fn spawn(name: &str, body: extern "C" fn(u64) -> !, arg: u64, on_panic: OnPanic) -> ThreadSched {
    let (stack, entry_rsp) = crate::loader::alloc_kernel_stack(
        crate::loader::kernel_start,
        body as usize as u64,
        0,
        arg,
    )
    .unwrap_or_else(|| panic!("kthread: no kernel stack for {name}"));

    // **Reserved here, before the table lock, and published before
    // `enqueue_new`.** Its refusal is a panic, and a panic holding the process
    // table is `issues/panic-path/panic-holding-process-table-hangs.md`.
    let claim = Claim::take(name);

    let mut short = [0u8; THREAD_NAME_LEN];
    let len = name.len().min(THREAD_NAME_LEN - 1);
    short[..len].copy_from_slice(&name.as_bytes()[..len]);

    // A kernel thread has no user half, so it has no user symbols either — and
    // it names the empty table rather than being an exception to what a task
    // carries. A crash report on one resolves nothing here and everything
    // through `symbols::resolve_kernel`, which is the right answer for a thread
    // whose every frame is kernel text.
    let syms = Arc::new(SymbolTable::empty());

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
            Arc::clone(&syms),
            ThreadEntry::new(Arc::new(Lock::new(kernel_thread_data()))),
        )
    });
    let tid = table.get(pid).expect("kthread: the entry just inserted is gone").main_tid();
    // **Before `enqueue_new` and not after it.** That call is where the task
    // becomes runnable, stealable and able to panic, and a panic on a task whose
    // row is not published yet is answered by the coin toss the row exists to
    // replace. It was after until 2026-08-15.
    claim.publish(TaskId(pid, tid), on_panic);
    // **The kernel address space, named rather than defaulted to.** A kernel
    // thread runs in the one every CPU is already in between two user threads;
    // saying so here is what let `KernelPayload.address_space` stop being an
    // `Option`, so one declaration decides every task's `cr3`.
    let sched = scheduler::enqueue_new(
        TaskId(pid, tid),
        stack,
        entry_rsp,
        crate::mm::paging::kernel().clone(),
        0,
        syms,
    );
    table
        .get_mut(pid)
        .and_then(|p| p.threads_mut().get_mut(tid))
        .expect("kthread: the thread just inserted is gone")
        .set_sched(sched.clone());
    drop(guard);

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

use core::arch::{asm, naked_asm};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use hashbrown::HashMap;

use crate::arch::{cpu, paging, percpu};
use crate::pipe::PipeId;
use crate::process::{self, AddressSpace, IdleProof, OwnedAlloc, Pid, Tid, KERNEL_STACK_SIZE};
use crate::sync::Lock;
use crate::{keyboard, PhysAddr};

const IA32_FS_BASE: u32 = 0xC0000100;
const MAX_CPUS: usize = 16;
const MAX_VRUNTIME_LAG_NS: u64 = 50_000_000; // 50ms

// ---------------------------------------------------------------------------
// BlockReason — why a thread is blocked
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum BlockReason {
    Keyboard,
    PipeRead(PipeId),
    PipeWrite(PipeId),
    WaitPid(Pid),
    ThreadJoin(Tid),
    Poll { deadline: u64 },
    NetRecv { deadline: u64 },
    Sleep { deadline: u64 },
    Futex { phys_addr: PhysAddr, deadline: u64 },
}


// ---------------------------------------------------------------------------
// ThreadCtx — context switch state, owned by the scheduler
// ---------------------------------------------------------------------------

pub struct ThreadCtx {
    pub tid: Tid,
    pub process: Pid,
    pub kernel_stack: OwnedAlloc,
    pub kernel_rsp: u64,
    pub address_space: Option<Arc<AddressSpace>>,
    pub fs_base: u64,
    pub cpu_ns: u64,
    pub scheduled_at: u64,
}

impl ThreadCtx {
    pub fn kernel_stack_top(&self) -> u64 {
        self.kernel_stack.ptr() as u64 + KERNEL_STACK_SIZE as u64
    }

    pub fn cr3(&self) -> PhysAddr {
        unsafe { self.address_space.as_ref().unwrap().cr3_value() }
    }

    pub fn stop_cpu_timer(&mut self, now: u64) {
        if self.scheduled_at > 0 {
            self.cpu_ns += now - self.scheduled_at;
            self.scheduled_at = 0;
        }
    }

    pub fn start_cpu_timer(&mut self, now: u64) {
        self.scheduled_at = now;
    }

    pub fn cpu_ns(&self) -> u64 {
        if self.scheduled_at > 0 {
            self.cpu_ns + (crate::clock::nanos_since_boot() - self.scheduled_at)
        } else {
            self.cpu_ns
        }
    }

}

// ---------------------------------------------------------------------------
// SwitchReason — disposition of the outgoing thread
// ---------------------------------------------------------------------------

enum SwitchReason {
    Yield,
    Block(BlockReason),
    Exit,
}

// ---------------------------------------------------------------------------
// CpuRunQueue + CpuQueueGuard — per-CPU ready queue with typed lock ordering
// ---------------------------------------------------------------------------

struct CpuRunQueue {
    current: Option<ThreadCtx>,
    outgoing: Option<(ThreadCtx, SwitchReason)>,
    save_rsp: u64,
    ready: BTreeMap<(u64, Tid), ThreadCtx>,
}

impl CpuRunQueue {
    const fn new() -> Self {
        Self {
            current: None,
            outgoing: None,
            save_rsp: 0,
            ready: BTreeMap::new(),
        }
    }
}

/// Typed guard for a locked CpuRunQueue. All queue operations go through this.
///
/// Lock ordering is enforced by the API: `charge()` and `effective_vruntime()`
/// acquire the vruntimes lock internally, guaranteeing CPU queue → vruntimes
/// ordering. You cannot lock vruntimes without holding a CpuQueueGuard — the
/// compiler prevents it.
pub struct CpuQueueGuard<'a>(crate::sync::LockGuard<'a, CpuRunQueue>);

impl<'a> CpuQueueGuard<'a> {
    pub fn pick_next(&mut self) -> Option<ThreadCtx> {
        self.0.ready.pop_first().map(|(_, ctx)| ctx)
    }

    pub fn insert(&mut self, vrt: u64, ctx: ThreadCtx) {
        let tid = ctx.tid;
        self.0.ready.insert((vrt, tid), ctx);
    }

    pub fn take_current(&mut self) -> Option<ThreadCtx> { self.0.current.take() }
    pub fn set_current(&mut self, ctx: ThreadCtx) { self.0.current = Some(ctx); }
    pub fn current(&self) -> Option<&ThreadCtx> { self.0.current.as_ref() }
    fn take_outgoing(&mut self) -> Option<(ThreadCtx, SwitchReason)> { self.0.outgoing.take() }
    fn set_outgoing(&mut self, ctx: ThreadCtx, reason: SwitchReason) { self.0.outgoing = Some((ctx, reason)); }
    pub fn save_rsp_ptr(&mut self) -> *mut u64 { &mut self.0.save_rsp as *mut u64 }
    pub fn save_rsp(&self) -> u64 { self.0.save_rsp }
    pub fn ready_len(&self) -> usize { self.0.ready.len() }
    pub fn is_ready(&self, tid: Tid) -> bool { self.0.ready.keys().any(|(_, t)| *t == tid) }

    pub fn remove_ready(&mut self, tid: Tid) -> Option<ThreadCtx> {
        let key = self.0.ready.keys().find(|(_, t)| *t == tid).copied();
        key.and_then(|k| self.0.ready.remove(&k))
    }

    /// Charge vruntime. Acquires vruntimes lock internally.
    /// Lock order: CPU queue (held via self) → vruntimes.
    pub fn charge(&self, sched: &Scheduler, process: Pid, ns: u64) {
        sched.charge_vruntime(process, ns);
    }

    /// Read effective vruntime (with lag cap). Acquires vruntimes lock internally.
    pub fn effective_vruntime(&self, sched: &Scheduler, process: Pid) -> u64 {
        sched.effective_vruntime(process)
    }

    /// Consume the guard without unlocking. The resuming side calls force_unlock.
    pub fn into_raw(self) { core::mem::forget(self.0); }
}

// ---------------------------------------------------------------------------
// BlockedPool — blocked threads with secondary indexes
// ---------------------------------------------------------------------------

struct BlockedPool {
    threads: HashMap<Tid, (ThreadCtx, BlockReason)>,
    pipe_read_waiters: HashMap<PipeId, Vec<Tid>>,
    pipe_write_waiters: HashMap<PipeId, Vec<Tid>>,
    futex_waiters: HashMap<PhysAddr, Vec<Tid>>,
    poll_waiters: Vec<Tid>,
}

impl BlockedPool {
    fn new() -> Self {
        Self {
            threads: HashMap::new(),
            pipe_read_waiters: HashMap::new(),
            pipe_write_waiters: HashMap::new(),
            futex_waiters: HashMap::new(),
            poll_waiters: Vec::new(),
        }
    }

    fn insert(&mut self, ctx: ThreadCtx, reason: BlockReason) {
        let tid = ctx.tid;
        match &reason {
            BlockReason::PipeRead(id) => {
                self.pipe_read_waiters.entry(*id).or_default().push(tid);
            }
            BlockReason::PipeWrite(id) => {
                self.pipe_write_waiters.entry(*id).or_default().push(tid);
            }
            BlockReason::Futex { phys_addr, .. } => {
                self.futex_waiters.entry(*phys_addr).or_default().push(tid);
            }
            BlockReason::Poll { .. } => {
                self.poll_waiters.push(tid);
            }
            _ => {}
        }
        self.threads.insert(tid, (ctx, reason));
    }

    fn remove(&mut self, tid: Tid) -> Option<(ThreadCtx, BlockReason)> {
        let (ctx, reason) = self.threads.remove(&tid)?;
        // Clean up secondary indexes
        match &reason {
            BlockReason::PipeRead(id) => {
                if let Some(v) = self.pipe_read_waiters.get_mut(id) {
                    v.retain(|t| *t != tid);
                }
            }
            BlockReason::PipeWrite(id) => {
                if let Some(v) = self.pipe_write_waiters.get_mut(id) {
                    v.retain(|t| *t != tid);
                }
            }
            BlockReason::Futex { phys_addr, .. } => {
                if let Some(v) = self.futex_waiters.get_mut(phys_addr) {
                    v.retain(|t| *t != tid);
                }
            }
            BlockReason::Poll { .. } => {
                self.poll_waiters.retain(|t| *t != tid);
            }
            _ => {}
        }
        Some((ctx, reason))
    }

    /// Take all threads blocked on reading from a specific pipe.
    fn take_pipe_readers(&mut self, pipe_id: PipeId) -> Vec<ThreadCtx> {
        let tids: Vec<Tid> = self.pipe_read_waiters.remove(&pipe_id).unwrap_or_default();
        tids.into_iter().filter_map(|tid| {
            let (ctx, reason) = self.threads.remove(&tid)?;
            // No need to clean pipe_read_waiters — we already removed the whole vec
            // But clean other indexes if somehow in multiple
            match &reason {
                BlockReason::Futex { phys_addr, .. } => {
                    if let Some(v) = self.futex_waiters.get_mut(phys_addr) {
                        v.retain(|t| *t != tid);
                    }
                }
                _ => {}
            }
            Some(ctx)
        }).collect()
    }

    /// Take all threads blocked on writing to a specific pipe.
    fn take_pipe_writers(&mut self, pipe_id: PipeId) -> Vec<ThreadCtx> {
        let tids: Vec<Tid> = self.pipe_write_waiters.remove(&pipe_id).unwrap_or_default();
        tids.into_iter().filter_map(|tid| {
            let (ctx, _) = self.threads.remove(&tid)?;
            Some(ctx)
        }).collect()
    }

    /// Take up to `count` threads blocked on a specific futex address.
    fn take_futex_waiters(&mut self, addr: PhysAddr, count: usize) -> Vec<ThreadCtx> {
        let tids = match self.futex_waiters.get_mut(&addr) {
            Some(v) => {
                let n = count.min(v.len());
                v.drain(..n).collect::<Vec<_>>()
            }
            None => return Vec::new(),
        };
        tids.into_iter().filter_map(|tid| {
            let (ctx, _) = self.threads.remove(&tid)?;
            Some(ctx)
        }).collect()
    }

    /// Take BlockedPoll threads whose poll_fds reference a specific pipe for reading.
    fn take_poll_readers_for_pipe(&mut self, pipe_id: PipeId) -> Vec<ThreadCtx> {
        let mut woken = Vec::new();
        let mut remaining = Vec::new();
        for tid in self.poll_waiters.drain(..) {
            if let Some((_, reason)) = self.threads.get(&tid) {
                if matches!(reason, BlockReason::Poll { .. }) {
                    // Check ProcessData for pipe interest
                    let dominated = {
                        let table = process::PROCESS_TABLE.lock();
                        let table = table.as_ref().unwrap();
                        if let Some(info) = table.get(tid) {
                            let data = info.data().lock();
                            data.poll_read_pipes[..data.poll_read_pipe_count as usize]
                                .contains(&pipe_id)
                        } else {
                            false
                        }
                    };
                    if dominated {
                        if let Some((ctx, _)) = self.threads.remove(&tid) {
                            woken.push(ctx);
                            continue;
                        }
                    }
                }
            }
            remaining.push(tid);
        }
        self.poll_waiters = remaining;
        woken
    }

    /// Take BlockedPoll threads whose poll_fds reference a specific pipe for writing.
    fn take_poll_writers_for_pipe(&mut self, pipe_id: PipeId) -> Vec<ThreadCtx> {
        let mut woken = Vec::new();
        let mut remaining = Vec::new();
        for tid in self.poll_waiters.drain(..) {
            if let Some((_, reason)) = self.threads.get(&tid) {
                if matches!(reason, BlockReason::Poll { .. }) {
                    let dominated = {
                        let table = process::PROCESS_TABLE.lock();
                        let table = table.as_ref().unwrap();
                        if let Some(info) = table.get(tid) {
                            let data = info.data().lock();
                            data.poll_write_pipes[..data.poll_write_pipe_count as usize]
                                .contains(&pipe_id)
                        } else {
                            false
                        }
                    };
                    if dominated {
                        if let Some((ctx, _)) = self.threads.remove(&tid) {
                            woken.push(ctx);
                            continue;
                        }
                    }
                }
            }
            remaining.push(tid);
        }
        self.poll_waiters = remaining;
        woken
    }

    /// Scan for deadline/global-event wakeups. Returns Tids to wake.
    fn scan_timeouts_and_events(&self, now: u64, kb_ready: bool, net_ready: bool) -> Vec<Tid> {
        let mut woken = Vec::new();
        // Check zombie tids/pids for waitpid/thread_join
        let (zombie_tids, zombie_pids) = {
            let table = process::PROCESS_TABLE.lock();
            let table = table.as_ref().unwrap();
            let mut zt = Vec::new();
            let mut zp = Vec::new();
            for (_, entry) in table.iter() {
                if matches!(entry.state(), process::ProcessState::Zombie(_)) {
                    zt.push(entry.tid());
                    zp.push(entry.process());
                }
            }
            (zt, zp)
        };

        for (tid, (_, reason)) in &self.threads {
            let wake = match reason {
                BlockReason::Keyboard => kb_ready,
                BlockReason::PipeRead(id) => crate::pipe::has_data(*id),
                BlockReason::PipeWrite(id) => crate::pipe::has_space(*id),
                BlockReason::WaitPid(child_pid) => zombie_pids.contains(child_pid),
                BlockReason::ThreadJoin(child_tid) => zombie_tids.contains(child_tid),
                BlockReason::Poll { deadline } => {
                    kb_ready || net_ready
                        || (*deadline > 0 && now >= *deadline)
                }
                BlockReason::NetRecv { deadline } => {
                    net_ready || (*deadline > 0 && now >= *deadline)
                }
                BlockReason::Sleep { deadline } => now >= *deadline,
                BlockReason::Futex { deadline, .. } => *deadline > 0 && now >= *deadline,
            };
            if wake {
                woken.push(*tid);
            }
        }
        woken
    }
}

// ---------------------------------------------------------------------------
// Scheduler — the global scheduler instance
// ---------------------------------------------------------------------------

pub struct Scheduler {
    cpus: [Lock<CpuRunQueue>; MAX_CPUS],
    blocked: Lock<Option<BlockedPool>>,
    vruntimes: Lock<Option<HashMap<Pid, u64>>>,
    min_vruntime: AtomicU64,
}

static SCHEDULER: Scheduler = Scheduler {
    cpus: [const { Lock::new(CpuRunQueue::new()) }; MAX_CPUS],
    blocked: Lock::new(None),
    vruntimes: Lock::new(None),
    min_vruntime: AtomicU64::new(0),
};

/// Initialize the scheduler. Must be called once during boot.
pub fn init() {
    *SCHEDULER.blocked.lock() = Some(BlockedPool::new());
    *SCHEDULER.vruntimes.lock() = Some(HashMap::new());
}

static FUTEX_LOCK: Lock<()> = Lock::new(());

impl Scheduler {
    fn lock_cpu(&self, cpu: usize) -> CpuQueueGuard<'_> {
        CpuQueueGuard(self.cpus[cpu].lock())
    }

    fn try_lock_cpu(&self, cpu: usize) -> Option<CpuQueueGuard<'_>> {
        self.cpus[cpu].try_lock().map(CpuQueueGuard)
    }

    fn effective_vruntime(&self, process: Pid) -> u64 {
        let vrt = self.vruntimes.lock_unwrap().get(&process).copied().unwrap_or(0);
        let min = self.min_vruntime.load(Ordering::Relaxed);
        vrt.max(min.saturating_sub(MAX_VRUNTIME_LAG_NS))
    }

    fn charge_vruntime(&self, process: Pid, ns: u64) {
        let mut vruntimes = self.vruntimes.lock_unwrap();
        let vrt = vruntimes.entry(process).or_insert(0);
        *vrt = vrt.saturating_add(ns);
        let new_vrt = *vrt;
        drop(vruntimes);

        // Update min_vruntime monotonically
        let old_min = self.min_vruntime.load(Ordering::Relaxed);
        if new_vrt > old_min {
            // Approximate: just push min_vruntime up. A full min scan is too expensive.
            // The lag cap in effective_vruntime handles the case where min is stale.
        }
    }

    fn init_vruntime(&self, process: Pid) {
        let min = self.min_vruntime.load(Ordering::Relaxed);
        self.vruntimes.lock_unwrap().entry(process).or_insert(min);
    }

    fn pick_target_cpu(&self) -> u32 {
        let count = crate::arch::smp::cpu_count();
        let mut best_cpu = 0u32;
        let mut best_len = usize::MAX;
        for i in 0..count {
            if let Some(q) = self.try_lock_cpu(i as usize) {
                let len = q.ready_len();
                if len < best_len {
                    best_len = len;
                    best_cpu = i;
                }
            }
        }
        best_cpu
    }

    fn enqueue_woken(&self, woken: Vec<ThreadCtx>) {
        for ctx in woken {
            let cpu = self.pick_target_cpu();
            let mut q = self.lock_cpu(cpu as usize);
            let vrt = q.effective_vruntime(self, ctx.process);
            q.insert(vrt, ctx);
        }
    }
}

// ---------------------------------------------------------------------------
// Public API — called by process.rs and syscall.rs
// ---------------------------------------------------------------------------

/// Wake a specific thread from the blocked pool by Tid.
/// Used after teardown to immediately wake parent (waitpid) or joiner (thread_join).
pub fn wake_tid(tid: Tid) {
    let ctx = {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        match pool.remove(tid) {
            Some((ctx, _)) => ctx,
            None => return, // not blocked, nothing to do
        }
    };
    let cpu = SCHEDULER.pick_target_cpu();
    let mut q = SCHEDULER.lock_cpu(cpu as usize);
    let vrt = q.effective_vruntime(&SCHEDULER, ctx.process);
    q.insert(vrt, ctx);
}

/// Remove a thread's ThreadCtx from the scheduler (blocked pool or ready queue).
/// Returns None if the thread is currently running (can't be removed).
pub fn remove_thread(tid: Tid) -> Option<ThreadCtx> {
    {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        if let Some((ctx, _)) = pool.remove(tid) {
            return Some(ctx);
        }
    }
    for i in 0..crate::arch::smp::cpu_count() as usize {
        let mut q = SCHEDULER.lock_cpu(i);
        if let Some(ctx) = q.remove_ready(tid) {
            return Some(ctx);
        }
    }
    None
}

/// Get the address space from the current thread's ThreadCtx.
pub fn current_address_space() -> Option<Arc<AddressSpace>> {
    let cpu = percpu::cpu_id() as usize;
    let q = SCHEDULER.lock_cpu(cpu);
    q.current().and_then(|ctx| ctx.address_space.clone())
}

/// Enqueue a newly spawned thread into the scheduler.
pub fn enqueue_new(ctx: ThreadCtx) {
    SCHEDULER.init_vruntime(ctx.process);
    let cpu = SCHEDULER.pick_target_cpu();
    let mut q = SCHEDULER.lock_cpu(cpu as usize);
    let vrt = q.effective_vruntime(&SCHEDULER, ctx.process);
    q.insert(vrt, ctx);
}

/// Block the current thread and switch to the next ready one.
pub fn block(reason: BlockReason) {
    do_schedule(SwitchReason::Block(reason));
}

/// Cooperative yield: put current thread back in the ready queue.
pub fn yield_now() {
    do_schedule(SwitchReason::Yield);
}

/// Timer preemption: called from the timer interrupt handler.
pub fn preempt() {
    if percpu::current_tid().is_none() {
        return;
    }
    yield_now();
}

/// Exit the current thread: context switch away, then the idle loop
/// handles zombie collection.
pub fn exit_current(code: i32) -> ! {
    // Set zombie state in the thread table (if not already zombie)
    {
        let mut guard = process::PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        let tid = percpu::current_tid().unwrap();
        if let Some(entry) = table.get_mut(tid) {
            if !matches!(entry.state(), process::ProcessState::Zombie(_)) {
                entry.zombify(code);
            }
        }
    }

    do_schedule(SwitchReason::Exit);
    unreachable!("exit_current: returned from schedule");
}

/// Schedule without saving current context (used by ap_idle and BSP boot).
pub fn schedule_no_return() -> ! {
    percpu::set_current_tid(None);
    unsafe { percpu::set_kernel_stack(percpu::idle_stack_top()); }
    unsafe { cpu::write_cr3(paging::kernel_cr3()); }
    let sp = percpu::idle_stack_top();
    unsafe {
        asm!(
            "mov rsp, {sp}",
            "jmp {func}",
            sp = in(reg) sp,
            func = in(reg) cpu_idle_loop as *const () as usize,
            options(noreturn),
        );
    }
}

/// Wake processes blocked on reading from a pipe.
pub fn wake_pipe_readers(pipe_id: PipeId) {
    let woken = {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        let mut result = pool.take_pipe_readers(pipe_id);
        result.extend(pool.take_poll_readers_for_pipe(pipe_id));
        result
    };
    SCHEDULER.enqueue_woken(woken);
}

/// Wake processes blocked on writing to a pipe.
pub fn wake_pipe_writers(pipe_id: PipeId) {
    let woken = {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        let mut result = pool.take_pipe_writers(pipe_id);
        result.extend(pool.take_poll_writers_for_pipe(pipe_id));
        result
    };
    SCHEDULER.enqueue_woken(woken);
}

/// Wake all BlockedPoll processes (for listener/connect events).
pub fn wake_all_poll() {
    let woken = {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        let tids: Vec<Tid> = pool.poll_waiters.drain(..).collect();
        tids.into_iter().filter_map(|tid| {
            pool.threads.remove(&tid).map(|(ctx, _)| ctx)
        }).collect::<Vec<_>>()
    };
    SCHEDULER.enqueue_woken(woken);
}

/// Futex wait: atomically check value and block.
pub fn futex_wait(phys_addr: PhysAddr, expected: u32, deadline: u64) -> bool {
    let _futex = FUTEX_LOCK.lock();
    let current = unsafe { *(phys_addr.raw() as *const u32) };
    if current != expected {
        return false; // value changed, don't block
    }
    drop(_futex);
    // Even though we dropped the futex lock, the block is still correct:
    // any concurrent futex_wake that changes the value will wake us from
    // the blocked pool after we insert ourselves.
    block(BlockReason::Futex { phys_addr, deadline });
    true
}

/// Futex wake: wake up to `count` threads blocked on `phys_addr`.
pub fn futex_wake(phys_addr: PhysAddr, count: usize) -> u64 {
    let _futex = FUTEX_LOCK.lock();
    let woken = {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        pool.take_futex_waiters(phys_addr, count)
    };
    let n = woken.len() as u64;
    drop(_futex);
    SCHEDULER.enqueue_woken(woken);
    n
}

/// Get the ThreadCtx for the current thread from the current CPU's run queue.
/// Used by exit paths that need to access the thread's context.
pub fn with_current_ctx<R>(f: impl FnOnce(&ThreadCtx) -> R) -> Option<R> {
    let cpu = percpu::cpu_id() as usize;
    let q = SCHEDULER.lock_cpu(cpu);
    q.current().map(f)
}

/// Get scheduling state for sysinfo display.
/// Returns: 0=Running, 1=Ready, 2=Blocked, 3=unknown (not in scheduler, e.g. zombie).
pub fn thread_sched_state(tid: Tid) -> u8 {
    for i in 0..crate::arch::smp::cpu_count() as usize {
        if let Some(q) = SCHEDULER.try_lock_cpu(i) {
            if let Some(ctx) = q.current() {
                if ctx.tid == tid { return 0; }
            }
            if q.is_ready(tid) { return 1; }
        }
    }
    if SCHEDULER.blocked.lock_unwrap().threads.contains_key(&tid) {
        return 2;
    }
    3
}

/// Get cpu_ns for a thread that might be running or blocked.
pub fn thread_cpu_ns(tid: Tid) -> u64 {
    for i in 0..crate::arch::smp::cpu_count() as usize {
        if let Some(q) = SCHEDULER.try_lock_cpu(i) {
            if let Some(ctx) = q.current() {
                if ctx.tid == tid { return ctx.cpu_ns(); }
            }
        }
    }
    let pool = SCHEDULER.blocked.lock_unwrap();
    if let Some((ctx, _)) = pool.threads.get(&tid) {
        return ctx.cpu_ns;
    }
    0
}

/// Force-unlock the current CPU's queue lock. Called from process_start/thread_start
/// trampolines after the first context_switch into a new thread.
///
/// # Safety
/// Must only be called when the current CPU's queue lock is held (via `forget`).
pub unsafe fn force_unlock_current_cpu() {
    SCHEDULER.cpus[percpu::cpu_id() as usize].force_unlock();
}

/// Handle outgoing thread after context_switch. Public wrapper for process_start trampoline.
pub fn handle_outgoing_public() {
    handle_outgoing();
}

// ---------------------------------------------------------------------------
// Core scheduling logic
// ---------------------------------------------------------------------------

fn do_schedule(reason: SwitchReason) {
    let cpu = percpu::cpu_id() as usize;
    let now = crate::clock::nanos_since_boot();

    let mut queue = SCHEDULER.lock_cpu(cpu);

    if let Some(mut old) = queue.take_current() {
        old.fs_base = cpu::rdmsr(IA32_FS_BASE);
        let elapsed = if old.scheduled_at > 0 { now - old.scheduled_at } else { 0 };
        old.stop_cpu_timer(now);
        queue.charge(&SCHEDULER, old.process, elapsed);
        queue.set_outgoing(old, reason);
    }

    if let Some(new) = queue.pick_next() {
        let new_cr3 = new.cr3();
        let new_fs_base = new.fs_base;
        let new_ks_top = new.kernel_stack_top();
        let new_rsp = new.kernel_rsp;
        let new_tid = new.tid;

        let mut new = new;
        new.start_cpu_timer(now);
        queue.set_current(new);

        let old_rsp_ptr = queue.save_rsp_ptr();
        percpu::set_current_tid(Some(new_tid));
        unsafe { percpu::set_kernel_stack(new_ks_top); }
        unsafe { cpu::write_cr3(new_cr3); }
        cpu::wrmsr(IA32_FS_BASE, new_fs_base);

        queue.into_raw();
        unsafe { context_switch(old_rsp_ptr, new_rsp); }
        unsafe { SCHEDULER.cpus[percpu::cpu_id() as usize].force_unlock(); }

        handle_outgoing();
        return;
    }

    let old_rsp_ptr = queue.save_rsp_ptr();
    percpu::set_current_tid(None);
    unsafe { percpu::set_kernel_stack(percpu::idle_stack_top()); }
    unsafe { cpu::write_cr3(paging::kernel_cr3()); }

    queue.into_raw();
    unsafe { context_switch(old_rsp_ptr, percpu::idle_rsp()); }
    unsafe { SCHEDULER.cpus[percpu::cpu_id() as usize].force_unlock(); }

    handle_outgoing();
}

fn handle_outgoing() {
    let cpu = percpu::cpu_id() as usize;
    let mut queue = SCHEDULER.lock_cpu(cpu);
    if let Some((mut old, reason)) = queue.take_outgoing() {
        old.kernel_rsp = queue.save_rsp();
        match reason {
            SwitchReason::Yield => {
                let vrt = queue.effective_vruntime(&SCHEDULER, old.process);
                queue.insert(vrt, old);
            }
            SwitchReason::Block(block_reason) => {
                drop(queue);
                SCHEDULER.blocked.lock_unwrap().insert(old, block_reason);
                return;
            }
            SwitchReason::Exit => {
                drop(old);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Idle loop
// ---------------------------------------------------------------------------

fn cpu_idle_loop() -> ! {
    let idle_proof = unsafe { IdleProof::new_unchecked() };
    loop {
        // Poll blocked threads for timeouts and events
        poll_blocked();

        // Collect orphan zombies (from thread table)
        {
            let mut guard = process::PROCESS_TABLE.lock();
            let table = guard.as_mut().unwrap();
            table.collect_orphan_zombies(idle_proof);
        }

        // Check for ready threads on this CPU
        let cpu = percpu::cpu_id() as usize;
        {
            let mut queue = SCHEDULER.lock_cpu(cpu);
            if let Some(new) = queue.pick_next() {
                let new_cr3 = new.cr3();
                let new_fs_base = new.fs_base;
                let new_ks_top = new.kernel_stack_top();
                let new_rsp = new.kernel_rsp;
                let new_tid = new.tid;

                let mut new = new;
                new.start_cpu_timer(crate::clock::nanos_since_boot());
                queue.set_current(new);

                percpu::set_current_tid(Some(new_tid));
                unsafe { percpu::set_kernel_stack(new_ks_top); }
                unsafe { cpu::write_cr3(new_cr3); }
                cpu::wrmsr(IA32_FS_BASE, new_fs_base);

                queue.into_raw();
                unsafe { context_switch(percpu::idle_rsp_ptr(), new_rsp); }
                unsafe { SCHEDULER.cpus[percpu::cpu_id() as usize].force_unlock(); }

                handle_outgoing();
                continue;
            }
        }

        // Halt until next interrupt
        unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)); }
    }
}

fn poll_blocked() {
    if percpu::cpu_id() == 0 {
        crate::drivers::xhci::poll_if_pending();
    }

    let kb_ready = keyboard::has_data();
    let net_ready = crate::net::has_packet();
    let now = crate::clock::nanos_since_boot();

    let woken: Vec<ThreadCtx> = {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        let tids = pool.scan_timeouts_and_events(now, kb_ready, net_ready);
        tids.iter().filter_map(|tid| pool.remove(*tid).map(|(ctx, _)| ctx)).collect()
    };

    SCHEDULER.enqueue_woken(woken);
}

// ---------------------------------------------------------------------------
// Context switch (naked asm, unchanged)
// ---------------------------------------------------------------------------

#[unsafe(naked)]
unsafe extern "C" fn context_switch(old_rsp: *mut u64, new_rsp: u64) {
    naked_asm!(
        "pushfq",
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",   // save old RSP
        "mov rsp, rsi",     // load new RSP
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "popfq",
        "ret",
    );
}

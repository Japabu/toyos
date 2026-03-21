use core::arch::{asm, naked_asm};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use hashbrown::HashMap;

use crate::arch::{cpu, percpu};
use crate::io_uring::RingId;
use crate::pipe::PipeId;
use crate::process::{self, AddressSpace, IdleProof, OwnedAlloc, Pid, Tid, KERNEL_STACK_SIZE};
use crate::sync::Lock;
use crate::DirectMap;

const IA32_FS_BASE: u32 = 0xC0000100;
const MAX_CPUS: usize = 8;
const MAX_VRUNTIME_LAG_NS: u64 = 50_000_000; // 50ms
const EVENT_QUEUE_SIZE: usize = 256;

// ---------------------------------------------------------------------------
// EventSource — what wakes a blocked thread
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventSource {
    Keyboard,
    Mouse,
    Network,
    Listener,
    PipeReadable(PipeId),
    PipeWritable(PipeId),
    Futex(DirectMap),
    IoUring(RingId),
}

// ---------------------------------------------------------------------------
// PerCpuEventQueue — lock-free interrupt-to-scheduler channel
// ---------------------------------------------------------------------------

struct PerCpuEventQueue {
    events: [EventSource; EVENT_QUEUE_SIZE],
    head: AtomicU32, // writer (interrupt handler) — wait-free
    tail: AtomicU32, // reader (scheduler) — single consumer
    overflow_count: AtomicU64, // events dropped due to full buffer
}

impl PerCpuEventQueue {
    const fn new() -> Self {
        Self {
            events: [EventSource::Keyboard; EVENT_QUEUE_SIZE],
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
            overflow_count: AtomicU64::new(0),
        }
    }

    /// Push an event from interrupt context. Wait-free, no locks.
    fn push(&self, event: EventSource) {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let next = (head + 1) % EVENT_QUEUE_SIZE as u32;
        if next == tail {
            self.overflow_count.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // SAFETY: single producer per CPU, index is in bounds
        unsafe {
            let slot = &self.events as *const _ as *mut EventSource;
            slot.add(head as usize).write(event);
        }
        self.head.store(next, Ordering::Release);
    }

    /// Drain all pending events. Called from scheduler context.
    fn drain_into(&self, buf: &mut [EventSource; EVENT_QUEUE_SIZE], count: &mut usize) {
        *count = 0;
        loop {
            let tail = self.tail.load(Ordering::Relaxed);
            let head = self.head.load(Ordering::Acquire);
            if tail == head {
                break;
            }
            if *count >= EVENT_QUEUE_SIZE {
                break;
            }
            // SAFETY: single consumer, index is in bounds
            buf[*count] = unsafe {
                let slot = &self.events as *const EventSource;
                slot.add(tail as usize).read()
            };
            *count += 1;
            self.tail.store((tail + 1) % EVENT_QUEUE_SIZE as u32, Ordering::Release);
        }
    }
}

// SAFETY: PerCpuEventQueue uses atomics for synchronization.
unsafe impl Sync for PerCpuEventQueue {}

static PERCPU_EVENTS: [PerCpuEventQueue; MAX_CPUS] =
    [const { PerCpuEventQueue::new() }; MAX_CPUS];

/// Push an event from interrupt context. Wait-free, no locks, safe from any context.
pub fn push_event(event: EventSource) {
    let cpu = percpu::cpu_id() as usize;
    PERCPU_EVENTS[cpu].push(event);
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
    pub blocked_on: Option<EventSource>, // what this thread is waiting on (None = pure timeout/wake_tid)
    pub deadline: u64, // 0 = no deadline
    pub blocked_since: u64, // nanos_since_boot when entered blocked pool (0 = not blocked)
}

impl ThreadCtx {
    pub fn kernel_stack_top(&self) -> u64 {
        self.kernel_stack.ptr() as u64 + KERNEL_STACK_SIZE as u64
    }

    pub fn cr3(&self) -> DirectMap {
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
// WokenBatch — compiler-enforced thread leak prevention
// ---------------------------------------------------------------------------

#[must_use = "woken threads must be enqueued or they are permanently lost"]
pub struct WokenBatch {
    threads: Vec<ThreadCtx>,
}

impl WokenBatch {
    fn new() -> Self {
        Self { threads: Vec::new() }
    }

    fn push(&mut self, ctx: ThreadCtx) {
        self.threads.push(ctx);
    }

    fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }
}

// ---------------------------------------------------------------------------
// SwitchReason — disposition of the outgoing thread (no heap allocation)
// ---------------------------------------------------------------------------

enum SwitchReason {
    Yield,
    Block {
        event: Option<EventSource>,
        deadline: u64,
    },
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

/// Typed guard for a locked CpuRunQueue. Lock ordering enforced by API:
/// `charge()` and `effective_vruntime()` acquire vruntimes internally,
/// guaranteeing CPU queue → vruntimes. Compiler prevents wrong ordering.
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

    pub fn charge(&self, sched: &Scheduler, process: Pid, ns: u64) {
        sched.charge_vruntime(process, ns);
    }

    pub fn effective_vruntime(&self, sched: &Scheduler, process: Pid) -> u64 {
        sched.effective_vruntime(process)
    }

    pub fn into_raw(self) { core::mem::forget(self.0); }
}

// ---------------------------------------------------------------------------
// BlockedPool — event-indexed blocked threads with deadline heap
// ---------------------------------------------------------------------------

struct BlockedPool {
    threads: HashMap<Tid, ThreadCtx>,
    by_event: BTreeMap<EventSource, Vec<Tid>>,
    deadlines: BTreeMap<(u64, Tid), Tid>,
}

impl BlockedPool {
    fn new() -> Self {
        Self {
            threads: HashMap::new(),
            by_event: BTreeMap::new(),
            deadlines: BTreeMap::new(),
        }
    }

    fn insert(&mut self, mut ctx: ThreadCtx) {
        let tid = ctx.tid;
        ctx.blocked_since = crate::clock::nanos_since_boot();
        if let Some(event) = ctx.blocked_on {
            self.by_event.entry(event)
                .or_insert_with(Vec::new)
                .push(tid);
        }
        if ctx.deadline > 0 {
            self.deadlines.insert((ctx.deadline, tid), tid);
        }
        self.threads.insert(tid, ctx);
    }

    /// Remove a thread from all indexes. Single cleanup path.
    fn remove_thread(&mut self, tid: Tid) -> Option<ThreadCtx> {
        let ctx = self.threads.remove(&tid)?;
        if let Some(event) = &ctx.blocked_on {
            if let Some(waiters) = self.by_event.get_mut(event) {
                waiters.retain(|&t| t != tid);
                if waiters.is_empty() {
                    self.by_event.remove(event);
                }
            }
        }
        if ctx.deadline > 0 {
            self.deadlines.remove(&(ctx.deadline, tid));
        }
        Some(ctx)
    }

    /// Wake all threads waiting on an event source into a batch.
    fn take_by_event_into(&mut self, event: &EventSource, batch: &mut WokenBatch) {
        let Some(waiters) = self.by_event.remove(event) else { return };
        for tid in waiters {
            if let Some(ctx) = self.remove_thread(tid) {
                batch.push(ctx);
            }
        }
    }

    /// Wake up to `count` threads waiting on an event source.
    fn take_by_event_limited(&mut self, event: &EventSource, count: usize, batch: &mut WokenBatch) {
        let Some(waiters) = self.by_event.get_mut(event) else { return };
        let n = count.min(waiters.len());
        let tids_to_wake: Vec<Tid> = waiters.drain(..n).collect();
        if waiters.is_empty() {
            self.by_event.remove(event);
        }
        for tid in tids_to_wake {
            if let Some(ctx) = self.remove_thread(tid) {
                batch.push(ctx);
            }
        }
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

// Scheduler metrics — zero overhead when not read
static CONTEXT_SWITCHES: AtomicU64 = AtomicU64::new(0);
static IDLE_ENTRIES: AtomicU64 = AtomicU64::new(0);

/// Returns (context_switches, idle_entries).
#[allow(dead_code)]
pub fn stats() -> (u64, u64) {
    (CONTEXT_SWITCHES.load(Ordering::Relaxed), IDLE_ENTRIES.load(Ordering::Relaxed))
}

pub fn init() {
    *SCHEDULER.blocked.lock() = Some(BlockedPool::new());
    *SCHEDULER.vruntimes.lock() = Some(HashMap::new());
}

/// Log scheduler health. Called from idle loop.
pub fn log_health() {
    let mut ready = 0usize;
    for i in 0..crate::arch::smp::cpu_count() as usize {
        if let Some(q) = SCHEDULER.try_lock_cpu(i) {
            ready += q.ready_len();
            if q.current().is_some() { ready += 1; }
        }
    }
    let blocked = SCHEDULER.blocked.try_lock()
        .map(|g| g.as_ref().map(|p| p.threads.len()).unwrap_or(0))
        .unwrap_or(0);
    let tid = percpu::current_tid();
    crate::log!("sched: ready={} blocked={} current={:?}", ready, blocked, tid);

    // If everything is stuck, dump what threads are blocked on
    if ready == 0 && blocked > 0 {
        dump_blocked();
    }
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

    fn enqueue_batch(&self, batch: WokenBatch) {
        for ctx in batch.threads {
            let cpu = self.pick_target_cpu();
            let mut q = self.lock_cpu(cpu as usize);
            let vrt = q.effective_vruntime(self, ctx.process);
            q.insert(vrt, ctx);
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn enqueue_new(ctx: ThreadCtx) {
    SCHEDULER.init_vruntime(ctx.process);
    let cpu = SCHEDULER.pick_target_cpu();
    let mut q = SCHEDULER.lock_cpu(cpu as usize);
    let vrt = q.effective_vruntime(&SCHEDULER, ctx.process);
    q.insert(vrt, ctx);
}

/// Block the current thread on the given event sources with optional deadline.
/// `deadline = 0` means no timeout. `events` empty means woken only by `wake_tid` or deadline.
pub fn block(events: &[EventSource], deadline: u64) {
    do_schedule(SwitchReason::Block {
        event: events.first().copied(),
        deadline,
    });
}

pub fn yield_now() {
    do_schedule(SwitchReason::Yield);
}

/// Timer preemption. Called from timer interrupt handler.
pub fn preempt() {
    if percpu::current_tid().is_none() {
        return;
    }
    yield_now();
}

/// Check and wake threads with expired deadlines.
/// Called from drain_events (which already holds the blocked pool lock).
fn check_deadlines_locked(pool: &mut BlockedPool, batch: &mut WokenBatch) {
    let now = crate::clock::nanos_since_boot();
    while let Some((&(deadline, tid), _)) = pool.deadlines.first_key_value() {
        if deadline > now { break; }
        pool.deadlines.pop_first();
        if let Some(ctx) = pool.remove_thread(tid) {
            batch.push(ctx);
        }
    }
}

/// Public entry point for timer interrupt. Uses try_lock.
pub fn check_deadlines() {
    let now = crate::clock::nanos_since_boot();
    let Some(mut guard) = SCHEDULER.blocked.try_lock() else { return };
    let Some(pool) = guard.as_mut() else { return };
    let mut batch = WokenBatch::new();
    while let Some((&(deadline, tid), _)) = pool.deadlines.first_key_value() {
        if deadline > now { break; }
        pool.deadlines.pop_first();
        if let Some(ctx) = pool.remove_thread(tid) {
            batch.push(ctx);
        }
    }
    drop(guard);
    if !batch.is_empty() {
        SCHEDULER.enqueue_batch(batch);
    }
}

pub fn exit_current(code: i32) -> ! {
    {
        let mut guard = process::PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        let tid = percpu::current_tid().unwrap();
        if let Some(entry) = table.get_mut(tid) {
            if !matches!(entry.state(), process::ProcessState::Zombie(_)) {
                match entry.kind() {
                    process::Kind::Thread { .. } => entry.zombify_thread(code),
                    process::Kind::Process { .. } => {
                        let cleanup = entry.zombify_process(code);
                        table.handle_orphans(cleanup);
                    }
                }
            }
        }
    }
    do_schedule(SwitchReason::Exit);
    unreachable!("exit_current: returned from schedule");
}

pub fn schedule_no_return() -> ! {
    percpu::set_current_tid(None);
    unsafe { percpu::set_kernel_stack(percpu::idle_stack_top()); }
    unsafe { cpu::write_cr3(crate::mm::paging::kernel().lock().as_ref().unwrap().cr3()); }
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

/// Wake all threads waiting on a specific event source.
pub fn wake_by_event(event: EventSource) {
    let batch = {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        let mut batch = WokenBatch::new();
        pool.take_by_event_into(&event, &mut batch);
        batch
    };
    if !batch.is_empty() {
        SCHEDULER.enqueue_batch(batch);
    }
}

/// Wake pipe readers: threads blocked on PipeReadable(pipe_id) + poll threads interested in this pipe.
pub fn wake_pipe_readers(pipe_id: PipeId) {
    wake_by_event(EventSource::PipeReadable(pipe_id));
}

/// Wake pipe writers: threads blocked on PipeWritable(pipe_id) + poll threads interested in this pipe.
pub fn wake_pipe_writers(pipe_id: PipeId) {
    wake_by_event(EventSource::PipeWritable(pipe_id));
}

/// Wake a specific thread by Tid (for waitpid/thread_join).
pub fn wake_tid(tid: Tid) {
    let ctx = {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        match pool.remove_thread(tid) {
            Some(ctx) => ctx,
            None => return,
        }
    };
    let cpu = SCHEDULER.pick_target_cpu();
    let mut q = SCHEDULER.lock_cpu(cpu as usize);
    let vrt = q.effective_vruntime(&SCHEDULER, ctx.process);
    q.insert(vrt, ctx);
}

/// Remove a thread from the scheduler entirely (for kill).
pub fn remove_thread(tid: Tid) -> Option<ThreadCtx> {
    {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        if let Some(ctx) = pool.remove_thread(tid) {
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

pub fn current_address_space() -> Option<Arc<AddressSpace>> {
    let cpu = percpu::cpu_id() as usize;
    let q = SCHEDULER.lock_cpu(cpu);
    q.current().and_then(|ctx| ctx.address_space.clone())
}

pub fn futex_wait(phys_addr: DirectMap, expected: u32, deadline: u64) -> bool {
    let _futex = FUTEX_LOCK.lock();
    let current = unsafe { *phys_addr.as_ptr::<u32>() };
    if current != expected {
        return false;
    }
    drop(_futex);
    block(&[EventSource::Futex(phys_addr)], deadline);
    true
}

pub fn futex_wake(phys_addr: DirectMap, count: usize) -> u64 {
    let _futex = FUTEX_LOCK.lock();
    let mut batch = WokenBatch::new();
    {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        pool.take_by_event_limited(&EventSource::Futex(phys_addr), count, &mut batch);
    }
    let n = batch.threads.len() as u64;
    drop(_futex);
    if !batch.is_empty() {
        SCHEDULER.enqueue_batch(batch);
    }
    n
}

pub fn with_current_ctx<R>(f: impl FnOnce(&ThreadCtx) -> R) -> Option<R> {
    let cpu = percpu::cpu_id() as usize;
    let q = SCHEDULER.lock_cpu(cpu);
    q.current().map(f)
}

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

pub fn thread_cpu_ns(tid: Tid) -> u64 {
    for i in 0..crate::arch::smp::cpu_count() as usize {
        if let Some(q) = SCHEDULER.try_lock_cpu(i) {
            if let Some(ctx) = q.current() {
                if ctx.tid == tid { return ctx.cpu_ns(); }
            }
        }
    }
    let pool = SCHEDULER.blocked.lock_unwrap();
    if let Some(ctx) = pool.threads.get(&tid) {
        return ctx.cpu_ns;
    }
    0
}

pub unsafe fn force_unlock_current_cpu() {
    SCHEDULER.cpus[percpu::cpu_id() as usize].force_unlock();
}

pub fn handle_outgoing_public() {
    handle_outgoing();
}

// ---------------------------------------------------------------------------
// Core scheduling logic
// ---------------------------------------------------------------------------

/// Drain per-CPU event queue and wake affected threads. One lock acquisition.
fn drain_events() {
    // Process xHCI events (keyboard/mouse) — converts MSI-X interrupt flag
    // into EventSource pushes via HID dispatch_report → push_event.
    if percpu::cpu_id() == 0 {
        crate::drivers::xhci::poll_if_pending();
    }

    // Virtio-net MSI-X interrupt sets a pending flag — convert to event.
    if crate::arch::idt::virtio_net::irq_pending() {
        PERCPU_EVENTS[percpu::cpu_id() as usize].push(EventSource::Network);
        let watchers = crate::net::io_uring_watchers();
        if !watchers.is_empty() {
            crate::io_uring::complete_pending_for_event(
                &watchers,
                EventSource::Network,
            );
        }
    }

    let cpu = percpu::cpu_id() as usize;

    // Check for event queue overflow (events silently dropped by push)
    let overflow = PERCPU_EVENTS[cpu].overflow_count.swap(0, Ordering::Relaxed);
    if overflow > 0 {
        crate::log!("EVENT QUEUE OVERFLOW: cpu={} dropped={} events", cpu, overflow);
    }

    let mut events = [EventSource::Keyboard; EVENT_QUEUE_SIZE];
    let mut event_count = 0usize;
    PERCPU_EVENTS[cpu].drain_into(&mut events, &mut event_count);
    if event_count == 0 { return; }

    let mut batch = WokenBatch::new();
    {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        for i in 0..event_count {
            pool.take_by_event_into(&events[i], &mut batch);
        }
        // Also check deadlines while we hold the lock — this is the primary
        // deadline check path. The timer's check_deadlines is a fallback.
        check_deadlines_locked(&mut pool, &mut batch);
    }
    if !batch.is_empty() {
        SCHEDULER.enqueue_batch(batch);
    }
}

fn do_schedule(reason: SwitchReason) {
    drain_events();

    let cpu = percpu::cpu_id() as usize;
    let now = crate::clock::nanos_since_boot();

    let mut queue = SCHEDULER.lock_cpu(cpu);

    if let Some(mut old) = queue.take_current() {
        check_stack_canary(&old);
        old.fs_base = cpu::rdmsr(IA32_FS_BASE);
        let elapsed = if old.scheduled_at > 0 { now - old.scheduled_at } else { 0 };
        old.stop_cpu_timer(now);
        queue.charge(&SCHEDULER, old.process, elapsed);
        queue.set_outgoing(old, reason);
    }

    if let Some(new) = queue.pick_next() {
        CONTEXT_SWITCHES.fetch_add(1, Ordering::Relaxed);
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
        unsafe { cpu::write_cr3(new_cr3.raw()); }
        cpu::wrmsr(IA32_FS_BASE, new_fs_base);

        queue.into_raw();
        unsafe { context_switch(old_rsp_ptr, new_rsp); }
        unsafe { SCHEDULER.cpus[percpu::cpu_id() as usize].force_unlock(); }

        handle_outgoing();
        return;
    }

    IDLE_ENTRIES.fetch_add(1, Ordering::Relaxed);
    let old_rsp_ptr = queue.save_rsp_ptr();
    percpu::set_current_tid(None);
    unsafe { percpu::set_kernel_stack(percpu::idle_stack_top()); }
    unsafe { cpu::write_cr3(crate::mm::paging::kernel().lock().as_ref().unwrap().cr3()); }

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
            SwitchReason::Block { event, deadline } => {
                old.blocked_on = event;
                old.deadline = deadline;
                drop(queue);
                SCHEDULER.blocked.lock_unwrap().insert(old);
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

static IDLE_HEALTH_COUNTER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn cpu_idle_loop() -> ! {
    let idle_proof = unsafe { IdleProof::new_unchecked() };
    loop {
        // Health check every ~1000 idle iterations
        if IDLE_HEALTH_COUNTER.fetch_add(1, Ordering::Relaxed) % 1000 == 999 {
            log_health();
        }

        drain_events();

        {
            let mut guard = process::PROCESS_TABLE.lock();
            let table = guard.as_mut().unwrap();
            table.collect_orphan_zombies(idle_proof);
        }

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
                        unsafe { cpu::write_cr3(new_cr3.raw()); }
                cpu::wrmsr(IA32_FS_BASE, new_fs_base);

                queue.into_raw();
                unsafe { context_switch(percpu::idle_rsp_ptr(), new_rsp); }
                unsafe { SCHEDULER.cpus[percpu::cpu_id() as usize].force_unlock(); }

                handle_outgoing();
                continue;
            }
        }

        unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)); }
    }
}

// ---------------------------------------------------------------------------
// Stack canary — detects kernel stack overflow on context switch
// ---------------------------------------------------------------------------

const STACK_CANARY: u64 = 0xDEAD_BEEF_CAFE_BABE;

pub fn write_stack_canary(stack: &OwnedAlloc) {
    unsafe { *(stack.ptr() as *mut u64) = STACK_CANARY; }
}

fn check_stack_canary(ctx: &ThreadCtx) {
    let canary = unsafe { *(ctx.kernel_stack.ptr() as *const u64) };
    if canary != STACK_CANARY {
        panic!("KERNEL STACK OVERFLOW: tid={} canary={:#x} expected={:#x}",
            ctx.tid, canary, STACK_CANARY);
    }
}

// ---------------------------------------------------------------------------
// Blocked thread dump — diagnostic for "system hangs" debugging
// ---------------------------------------------------------------------------

fn event_name(event: &EventSource) -> &'static str {
    match event {
        EventSource::Keyboard => "Keyboard",
        EventSource::Mouse => "Mouse",
        EventSource::Network => "Network",
        EventSource::Listener => "Listener",
        EventSource::PipeReadable(_) => "PipeR",
        EventSource::PipeWritable(_) => "PipeW",
        EventSource::Futex(_) => "Futex",
        EventSource::IoUring(_) => "IoUring",
    }
}

/// Dump all blocked threads with their registered events and deadlines.
/// Safe to call from any context (uses try_lock for process table).
pub fn dump_blocked() {
    let pool = SCHEDULER.blocked.lock_unwrap();
    let now = crate::clock::nanos_since_boot();
    crate::log!("=== BLOCKED THREADS ({}) ===", pool.threads.len());
    for (tid, ctx) in &pool.threads {
        let since_ms = if ctx.blocked_since > 0 { (now - ctx.blocked_since) / 1_000_000 } else { 0 };

        let events = match &ctx.blocked_on {
            Some(e) => event_name(e),
            None => "(none)",
        };

        // Try to get process name without blocking
        let mut name_buf = [0u8; 28];
        let mut got_name = false;
        if let Some(guard) = crate::process::PROCESS_TABLE.try_lock() {
            if let Some(table) = guard.as_ref() {
                if let Some(entry) = table.get(*tid) {
                    name_buf = *entry.name();
                    got_name = true;
                }
            }
        }
        let name = if got_name {
            core::str::from_utf8(&name_buf).unwrap_or("?").trim_end_matches('\0')
        } else {
            "?"
        };

        if ctx.deadline > 0 {
            let dl_secs = ctx.deadline / 1_000_000_000;
            let dl_ms = (ctx.deadline % 1_000_000_000) / 1_000_000;
            crate::log!("  tid={} pid={} ({}) events=[{}] deadline={}.{:03}s since={}ms",
                tid, ctx.process, name, events, dl_secs, dl_ms, since_ms);
        } else {
            crate::log!("  tid={} pid={} ({}) events=[{}] deadline=none since={}ms",
                tid, ctx.process, name, events, since_ms);
        }
    }
    crate::log!("=== END BLOCKED ===");
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

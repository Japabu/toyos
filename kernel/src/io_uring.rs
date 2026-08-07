//! Kernel io_uring implementation — shared-memory submission/completion rings.
//!
//! Two syscalls: `io_uring_setup` (create ring) and `io_uring_enter` (submit + wait).
//! The SQ/CQ/SQE arrays live in a single 2MB shared page accessible to both
//! kernel (via direct map) and userspace (via page table mapping).
//!
//! One-shot POLL_ADD: each fires once, then the pending poll is consumed.
//! Userspace must re-submit POLL_ADD to re-arm.
//!
//! Lock ordering: the wake path copies watcher lists under source locks (PIPES,
//! LISTENERS, device locks), releases them, then acquires IO_URINGS.
//! The recheck path in process_poll_add holds IO_URINGS while calling source
//! readiness checks (which acquire source locks internally). This is safe
//! because no path holds source locks while acquiring IO_URINGS.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use toyos_sched::task::WaitClass;

use crate::fd;
use crate::id_map::{IdKey, IdMap};
use crate::listener::ListenerId;
use crate::pipe::{self, PipeId};
use crate::process::{self, Pid};
use crate::scheduler;
use crate::shared_memory::{self, SharedToken};
use crate::sched::payload::KWaitQueue;
use crate::sched::waitqs::{new_queue, wake_all};
use crate::sync::Lock;
use crate::DirectMap;

use toyos_abi::io_uring::{
    IoUringCqe, IoUringParams, IoUringRingHeader, IoUringSqe,
    SQ_RING_OFF, CQ_RING_OFF, SQES_OFF,
};
use toyos_abi::syscall::SyscallError;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct RingId(usize);

impl RingId {
    pub fn raw(self) -> usize { self.0 }
}

impl core::ops::Add for RingId {
    type Output = Self;
    fn add(self, rhs: Self) -> Self { RingId(self.0 + rhs.0) }
}

impl IdKey for RingId {
    const ZERO: Self = RingId(0);
    const ONE: Self = RingId(1);
}

/// An owned reference to a ring. Creation and `Clone` bump the ring's
/// reference count, `Drop` tears it down at zero — the `PipeReader` shape.
/// Held by `Descriptor::IoUring`, so a `dup`ped ring fd stays usable after the
/// original is closed instead of naming a destroyed instance.
pub struct RingRef(RingId);

impl RingRef {
    pub fn id(&self) -> RingId { self.0 }
}

impl Clone for RingRef {
    fn clone(&self) -> Self {
        let mut guard = IO_URINGS.lock();
        let map = guard.as_mut().expect("io_uring not initialized");
        // A live `RingRef` whose instance is gone is a refcount bug: the
        // instance is removed only when the last one drops.
        map.get_mut(self.0).expect("RingRef outlived its ring").refs += 1;
        Self(self.0)
    }
}

impl Drop for RingRef {
    fn drop(&mut self) {
        let instance = {
            let mut guard = IO_URINGS.lock();
            let map = guard.as_mut().expect("io_uring not initialized");
            let instance = map.get_mut(self.0).expect("RingRef outlived its ring");
            instance.refs -= 1;
            if instance.refs > 0 {
                return;
            }
            map.remove(self.0)
        };
        if let Some(mut instance) = instance {
            // The polls' `WatcherGuard`s clean the per-source watcher lists.
            instance.pending_polls.clear();
            // Unmaps from every process and frees the backing pages.
            let _ = shared_memory::destroy(instance.shm_token, instance.owner_pid);
        }
    }
}

// IoUringOp — type-safe op code, converted from raw u8 at boundary

#[derive(Clone, Copy)]
pub enum IoUringOp {
    Nop,
    PollAdd,
    PollRemove,
    Accept,
    Close,
}

impl IoUringOp {
    fn from_raw(raw: u8) -> Result<Self, SyscallError> {
        match raw {
            0 => Ok(Self::Nop),
            1 => Ok(Self::PollAdd),
            2 => Ok(Self::PollRemove),
            3 => Ok(Self::Accept),
            4 => Ok(Self::Close),
            _ => Err(SyscallError::InvalidArgument),
        }
    }
}

// PollFlags — type-safe poll interest flags

#[derive(Clone, Copy)]
pub struct PollFlags(u32);

impl PollFlags {
    pub const IN: Self = Self(1);
    pub const OUT: Self = Self(4);

    pub fn from_raw(raw: u32) -> Self { Self(raw) }
    pub fn readable(self) -> bool { self.0 & 1 != 0 }
    pub fn writable(self) -> bool { self.0 & 4 != 0 }
    pub fn raw(self) -> u32 { self.0 }
}

/// What a `POLL_ADD` is registered on: io_uring's key for "which rings care
/// about this object". It names the same objects the wait queues hang off, but
/// it is not a scheduler concept — the scheduler knows only tasks, tickets and
/// causes (scheduler-core-spec §8.1).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Keyboard,
    Mouse,
    Network,
    Listener(ListenerId),
    PipeReadable(PipeId),
    PipeWritable(PipeId),
    Audio,
}

// WatcherGuard — RAII cleanup of per-fd watcher lists

struct WatcherGuard {
    ring_id: RingId,
    sources: [Option<Source>; 2],
}

impl WatcherGuard {
    fn new(ring_id: RingId) -> Self {
        Self { ring_id, sources: [None; 2] }
    }

    fn add_source(&mut self, source: Source) {
        if self.sources[0].is_none() {
            self.sources[0] = Some(source);
        } else {
            self.sources[1] = Some(source);
        }
    }
}

impl Drop for WatcherGuard {
    fn drop(&mut self) {
        for source in self.sources.iter().flatten() {
            source.remove_watcher(self.ring_id);
        }
    }
}

// PendingPoll — a POLL_ADD that hasn't fired yet

struct PendingPoll {
    user_data: u64,
    fd_num: u32,
    flags: PollFlags,
    read_source: Option<Source>,
    write_source: Option<Source>,
    _watcher: WatcherGuard,
}

/// Hard cap on pending polls per ring. With dedup this should never be reached
/// (bounded by number of open fds), but guards against future bugs.
const MAX_PENDING_POLLS: usize = 1024;

struct IoUringInstance {
    shm_phys: DirectMap,
    shm_token: SharedToken,
    /// Live `RingRef`s. Never zero while this entry is in the map.
    refs: u32,
    sq_size: u32,
    cq_size: u32,
    pending_polls: Vec<PendingPoll>,
    /// Threads waiting on this ring's completion queue (spec §8.6).
    waiters: Arc<KWaitQueue>,
    /// The authoritative CQ tail. The copy in the shared header is a
    /// publication for userspace, which only ever reads it — the kernel must
    /// not read its own tail back out of a page the process can write. Only
    /// touched under the `IO_URINGS` lock.
    cq_tail: core::cell::Cell<u32>,
    owner_pid: Pid,
}

impl IoUringInstance {
    fn sq_header(&self) -> &IoUringRingHeader {
        unsafe { &*(self.shm_phys.as_mut_ptr::<u8>().add(SQ_RING_OFF as usize) as *const IoUringRingHeader) }
    }

    fn cq_header(&self) -> &IoUringRingHeader {
        unsafe { &*(self.shm_phys.as_mut_ptr::<u8>().add(CQ_RING_OFF as usize) as *const IoUringRingHeader) }
    }

    fn sqe_at(&self, index: u32) -> &IoUringSqe {
        let ptr = self.shm_phys.as_mut_ptr::<u8>();
        unsafe { &*(ptr.add(SQES_OFF as usize + index as usize * core::mem::size_of::<IoUringSqe>()) as *const IoUringSqe) }
    }

    fn cqe_at_mut(&self, index: u32) -> &mut IoUringCqe {
        let ptr = self.shm_phys.as_mut_ptr::<u8>();
        unsafe { &mut *(ptr.add(CQ_RING_OFF as usize + 16 + index as usize * core::mem::size_of::<IoUringCqe>()) as *mut IoUringCqe) }
    }

    /// Post a CQE, or record a drop if the ring reports itself full.
    ///
    /// `head` lives in the page the process maps and writes, so "full" is
    /// either genuine — impossible with 2x sizing and an honest head — or a
    /// lie. Either way it is the process's own ring and its own problem, and
    /// not a kill: `complete_pending_for_event` calls this on the *waker's*
    /// thread, which belongs to a different process.
    fn post_cqe(&self, user_data: u64, result: i32, flags: u32) {
        let cq = self.cq_header();
        let tail = self.cq_tail.get();
        if tail.wrapping_sub(cq.head.load(Ordering::Acquire)) >= self.cq_size {
            cq.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let idx = tail & (self.cq_size - 1);
        let cqe = self.cqe_at_mut(idx);
        cqe.user_data = user_data;
        cqe.result = result;
        cqe.flags = flags;
        self.cq_tail.set(tail.wrapping_add(1));
        cq.tail.store(tail.wrapping_add(1), Ordering::Release);
    }

    /// Count available CQEs (unread by userspace). Measured against the
    /// kernel's own tail; a process that rewrites `head` can only ever
    /// mislead itself about how many completions are waiting for it.
    fn cq_count(&self) -> u32 {
        let head = self.cq_header().head.load(Ordering::Acquire);
        self.cq_tail.get().wrapping_sub(head)
    }

    /// Completions this ring has thrown away. Cumulative, never cleared.
    fn dropped(&self) -> u32 {
        self.cq_header().dropped.load(Ordering::Relaxed)
    }
}

static IO_URINGS: Lock<Option<IdMap<RingId, IoUringInstance>>> = Lock::new(None);

pub fn init() {
    *IO_URINGS.lock() = Some(IdMap::new());
}

/// Largest submission ring a process may ask for. Bounds every quantity in
/// `submit_sqes` that a process can influence.
const MAX_SQ_DEPTH: u32 = 256;

/// Create an io_uring instance. Returns (ring reference, shared_memory_token).
pub fn create(depth: u32) -> Result<(RingRef, SharedToken), SyscallError> {
    if depth == 0 || depth > MAX_SQ_DEPTH || !depth.is_power_of_two() {
        return Err(SyscallError::InvalidArgument);
    }

    let sq_size = depth;
    let cq_size = depth * 2;

    let pid = process::current_process();
    let addr_space = process::current_address_space();
    let shm_token = shared_memory::alloc(crate::mm::PAGE_2M, pid, &addr_space)
        .map_err(|_| SyscallError::ResourceExhausted)?;

    let shm_vaddr = shared_memory::map(shm_token, pid, &addr_space)
        .map_err(|_| SyscallError::Unknown)?;
    let shm_phys = addr_space.lock().translate(crate::UserAddr::new(shm_vaddr))
        .ok_or(SyscallError::Unknown)?;

    let base = shm_phys.as_mut_ptr::<u8>();

    // Zero the entire page first (alloc_zeroed does this, but be explicit)
    // Write params at offset 0
    let params = unsafe { &mut *(base as *mut IoUringParams) };
    params.sq_off = SQ_RING_OFF;
    params.cq_off = CQ_RING_OFF;
    params.sqes_off = SQES_OFF;
    params.sq_ring_size = sq_size;
    params.cq_ring_size = cq_size;
    params.features = 0;
    params._pad = 0;

    let sq_header = unsafe { &mut *(base.add(SQ_RING_OFF as usize) as *mut IoUringRingHeader) };
    sq_header.head = core::sync::atomic::AtomicU32::new(0);
    sq_header.tail = core::sync::atomic::AtomicU32::new(0);
    sq_header.ring_size = sq_size;
    sq_header.dropped = core::sync::atomic::AtomicU32::new(0);

    let cq_header = unsafe { &mut *(base.add(CQ_RING_OFF as usize) as *mut IoUringRingHeader) };
    cq_header.head = core::sync::atomic::AtomicU32::new(0);
    cq_header.tail = core::sync::atomic::AtomicU32::new(0);
    cq_header.ring_size = cq_size;
    cq_header.dropped = core::sync::atomic::AtomicU32::new(0);

    let ring_id = {
        let mut guard = IO_URINGS.lock();
        let map = guard.as_mut().expect("io_uring not initialized");
        map.insert(IoUringInstance {
            shm_phys,
            shm_token,
            refs: 1,
            sq_size,
            cq_size,
            pending_polls: Vec::new(),
            waiters: new_queue(WaitClass::Io),
            cq_tail: core::cell::Cell::new(0),
            owner_pid: pid,
        })
    };

    Ok((RingRef(ring_id), shm_token))
}

// Enter — submit SQEs and/or wait for CQEs

fn cq_count(ring_id: RingId) -> Result<u32, SyscallError> {
    with_instance(ring_id, |inst| inst.cq_count())
}

/// What `enter` sees of a ring before deciding to park: how many completions
/// are readable, and whether any have been thrown away.
fn cq_state(ring_id: RingId) -> Result<(u32, u32), SyscallError> {
    with_instance(ring_id, |inst| (inst.cq_count(), inst.dropped()))
}

/// Process SQEs and wait for completions. Called from the syscall handler.
/// Returns the number of CQEs available after processing.
pub fn enter(
    ring_id: RingId,
    to_submit: u32,
    min_complete: u32,
    timeout_nanos: u64,
) -> Result<u32, SyscallError> {
    let deadline = if timeout_nanos == 0 {
        1 // sentinel for non-blocking
    } else if timeout_nanos == u64::MAX {
        0 // block forever
    } else {
        crate::clock::nanos_since_boot().saturating_add(timeout_nanos)
    };

    if to_submit > 0 {
        submit_sqes(ring_id, to_submit)?;
    }

    // Wait phase. The queue is cloned out of the table so the ticket and the
    // registration can borrow it across the park without holding the table.
    let queue = waiters_of(ring_id)?;
    loop {
        let (count, dropped) = cq_state(ring_id)?;

        if count >= min_complete || min_complete == 0 {
            return Ok(count);
        }

        if deadline == 1 {
            return Ok(count);
        }

        // A ring that has thrown a completion away must not be slept on: the
        // one this thread waits for may be the one discarded. Returning short
        // puts the counter in front of `Poller::wait`'s assertion, which is
        // otherwise read only after the call that blocks.
        if dropped > 0 {
            return Ok(count);
        }

        if deadline > 0 && crate::clock::nanos_since_boot() >= deadline {
            return Ok(count);
        }

        // The re-check is this ring's own condition, not mere readiness: a
        // waiter for `min_complete` CQEs that cancelled on the first one would
        // spin instead of parking.
        //
        // The ticket must be consumed on every path out of here, `?` included.
        // A sibling thread closing this ring's fd removes it from IO_URINGS in
        // exactly this window, and a ticket dropped still armed is a panic.
        let ticket = scheduler::prepare_wait(&queue);
        let recheck = match cq_count(ring_id) {
            Ok(n) => n,
            Err(e) => { ticket.cancel(); return Err(e); }
        };
        if recheck >= min_complete {
            ticket.cancel();
            continue;
        }
        scheduler::block_on(ticket, if deadline == 1 { 0 } else { deadline });
    }
}

/// This ring's completion waiter set, cloned out of the table.
fn waiters_of(ring_id: RingId) -> Result<Arc<KWaitQueue>, SyscallError> {
    with_instance(ring_id, |inst| inst.waiters.clone())
}

/// Read and process SQEs from the submission ring.
///
/// Both inputs are untrusted: `count` is a syscall argument, and the `head`/
/// `tail` the ring depth is measured against live in the 2 MiB page the
/// process maps and writes itself. Neither is clamped — a request the ring
/// could never honestly hold is refused, because clamping would silently
/// turn a lie into a smaller lie.
fn submit_sqes(ring_id: RingId, count: u32) -> Result<(), SyscallError> {
    if count > with_instance(ring_id, |inst| inst.sq_size)? {
        return Err(SyscallError::InvalidArgument);
    }
    for _ in 0..count {
        let Some(sqe) = claim_sqe(ring_id)? else { break };
        process_sqe(ring_id, &sqe);
    }
    Ok(())
}

/// Take the SQE at the ring head, advancing it. `None` when the ring is empty.
///
/// One SQE at a time under the lock rather than a batch copied into a `Vec`
/// whose capacity userland picks; processing needs the lock released between
/// entries either way.
fn claim_sqe(ring_id: RingId) -> Result<Option<IoUringSqe>, SyscallError> {
    with_instance(ring_id, |instance| {
        let sq = instance.sq_header();
        let head = sq.head.load(Ordering::Acquire);
        let tail = sq.tail.load(Ordering::Acquire);
        let available = tail.wrapping_sub(head);
        if available == 0 {
            return Ok(None);
        }
        if available > instance.sq_size {
            return Err(SyscallError::InvalidArgument);
        }
        let sqe = *instance.sqe_at(head & (instance.sq_size - 1));
        sq.head.store(head.wrapping_add(1), Ordering::Release);
        Ok(Some(sqe))
    })?
}

fn with_instance<R>(ring_id: RingId, f: impl FnOnce(&IoUringInstance) -> R) -> Result<R, SyscallError> {
    let guard = IO_URINGS.lock();
    let map = guard.as_ref().expect("io_uring not initialized");
    Ok(f(map.get(ring_id).ok_or(SyscallError::NotFound)?))
}

/// Process a single SQE.
fn process_sqe(ring_id: RingId, sqe: &IoUringSqe) {
    let op = match IoUringOp::from_raw(sqe.op) {
        Ok(op) => op,
        Err(_) => {
            post_cqe_locked(ring_id, sqe.user_data, -(SyscallError::InvalidArgument as i32), 0);
            return;
        }
    };

    match op {
        IoUringOp::Nop => {
            post_cqe_locked(ring_id, sqe.user_data, 0, 0);
        }
        IoUringOp::PollAdd => {
            process_poll_add(ring_id, sqe);
        }
        IoUringOp::PollRemove => {
            process_poll_remove(ring_id, sqe.user_data);
        }
        IoUringOp::Accept => {
            process_accept(ring_id, sqe);
        }
        IoUringOp::Close => {
            process_close(ring_id, sqe);
        }
    }
}

fn process_poll_add(ring_id: RingId, sqe: &IoUringSqe) {
    let fd_num = sqe.fd as u32;
    let flags = PollFlags::from_raw(sqe.op_flags);
    let user_data = sqe.user_data;

    // Check readiness first (use fd_owner_data — fds belong to the process, not the thread)
    let (ready, read_source, write_source) = process::with_fd_owner_data(|data| {
        let readable = flags.readable() && fd::has_data(&data.fds, fd_num);
        let writable = flags.writable() && fd::has_space(&data.fds, fd_num);
        let rsrc = if flags.readable() {
            data.fds.get(fd_num).and_then(|d| d.read_source())
        } else { None };
        let wsrc = if flags.writable() {
            data.fds.get(fd_num).and_then(|d| d.write_source())
        } else { None };
        (readable || writable, rsrc, wsrc)
    });

    if ready {
        // Already ready — post CQE immediately (one-shot: consumed)
        let mut result_flags = 0u32;
        if flags.readable() { result_flags |= PollFlags::IN.raw(); }
        if flags.writable() { result_flags |= PollFlags::OUT.raw(); }
        post_cqe_locked(ring_id, user_data, result_flags as i32, 0);
        return;
    }

    // Not ready — insert pending poll.
    // Drop any existing PendingPoll for this fd FIRST, so its WatcherGuard
    // cleanup runs before we register the new watchers. Otherwise:
    //   1. add_watcher(new) → no-op (old watcher still registered)
    //   2. drop(old) → removes the watcher
    //   3. result: zero watchers despite an active PendingPoll
    let mut woken: Option<Arc<KWaitQueue>> = None;
    let mut guard = IO_URINGS.lock();
    let map = guard.as_mut().expect("io_uring not initialized");
    if let Some(instance) = map.get_mut(ring_id) {
        // Remove existing PendingPoll for this fd (drops old WatcherGuard)
        if let Some(pos) = instance.pending_polls.iter().position(|pp| pp.fd_num == fd_num) {
            instance.pending_polls.swap_remove(pos);
        }

        let mut watcher = WatcherGuard::new(ring_id);
        if let Some(src) = read_source {
            src.add_watcher(ring_id);
            watcher.add_source(src);
        }
        if let Some(src) = write_source {
            src.add_watcher(ring_id);
            watcher.add_source(src);
        }

        let new_pp = PendingPoll {
            user_data,
            fd_num,
            flags,
            read_source,
            write_source,
            _watcher: watcher,
        };

        if instance.pending_polls.len() < MAX_PENDING_POLLS {
            instance.pending_polls.push(new_pp);
        } else {
            instance.post_cqe(user_data, -(SyscallError::ResourceExhausted as i32), 0);
            let queue = instance.waiters.clone();
            drop(guard);
            wake_all(&queue);
            return;
        }

        // Recheck: close TOCTOU window between readiness check and PendingPoll
        // insertion. A concurrent wake (complete_pending_for_event) either already
        // ran and found no PendingPoll (recheck catches the data it left behind),
        // or is blocked on IO_URINGS and will find the PendingPoll after we release.
        let became_ready = read_source.is_some_and(Source::is_ready)
            || write_source.is_some_and(Source::is_ready);
        if became_ready {
            if let Some(pos) = instance.pending_polls.iter().position(|pp| pp.fd_num == fd_num) {
                let pp = instance.pending_polls.swap_remove(pos);
                let mut result_flags = 0u32;
                if pp.flags.readable() { result_flags |= PollFlags::IN.raw(); }
                if pp.flags.writable() { result_flags |= PollFlags::OUT.raw(); }
                instance.post_cqe(pp.user_data, result_flags as i32, 0);
                woken = Some(instance.waiters.clone());
            }
        }
    }
    drop(guard);
    if let Some(queue) = woken {
        wake_all(&queue);
    }
}

fn process_poll_remove(ring_id: RingId, target_user_data: u64) {
    let mut guard = IO_URINGS.lock();
    let map = guard.as_mut().expect("io_uring not initialized");
    if let Some(instance) = map.get_mut(ring_id) {
        if let Some(pos) = instance.pending_polls.iter().position(|p| p.user_data == target_user_data) {
            instance.pending_polls.swap_remove(pos);
            instance.post_cqe(target_user_data, 0, 0);
        } else {
            instance.post_cqe(target_user_data, -(SyscallError::NotFound as i32), 0);
        }
    }
}

fn process_accept(ring_id: RingId, sqe: &IoUringSqe) {
    let fd_num = sqe.fd as u32;
    let user_data = sqe.user_data;

    let listener_id = process::with_fd_owner_data(|data| {
        match data.fds.get(fd_num) {
            Some(fd::Descriptor::Listener(l)) => Some(l.id()),
            _ => None,
        }
    });

    let Some(listener_id) = listener_id else {
        post_cqe_locked(ring_id, user_data, -(SyscallError::InvalidArgument as i32), 0);
        return;
    };

    match crate::listener::pop_connection(listener_id) {
        Some(conn) => {
            let new_fd = process::with_fd_owner_data(|data| {
                data.fds.insert(fd::Descriptor::Socket {
                    rx: conn.rx,
                    tx: conn.tx,
                    peer: conn.client_pid,
                })
            });
            match new_fd {
                Ok(fd_num) => post_cqe_locked(ring_id, user_data, fd_num as i32, 0),
                Err(e) => post_cqe_locked(ring_id, user_data, -(e as i32), 0),
            }
        }
        None => {
            post_cqe_locked(ring_id, user_data, -(SyscallError::WouldBlock as i32), 0);
        }
    }
}

fn process_close(ring_id: RingId, sqe: &IoUringSqe) {
    let fd_num = sqe.fd as u32;
    let user_data = sqe.user_data;

    let result = process::with_fd_owner_data(|data| {
        fd::close(&mut data.fds, fd_num, &mut data.pipe_maps)
    });

    post_cqe_locked(ring_id, user_data, result as i32, 0);
}

/// Post a CQE and wake this ring's waiters.
///
/// The wake is not optional although every caller is the submitting thread: a
/// ring is a process-wide object, and a sibling thread parked in `enter` on it
/// never sees a completion nobody announced.
fn post_cqe_locked(ring_id: RingId, user_data: u64, result: i32, flags: u32) {
    let guard = IO_URINGS.lock();
    let map = guard.as_ref().expect("io_uring not initialized");
    let woken = map.get(ring_id).map(|instance| {
        instance.post_cqe(user_data, result, flags);
        instance.waiters.clone()
    });
    drop(guard);
    if let Some(queue) = woken {
        wake_all(&queue);
    }
}

// Wake path — called when a source becomes ready

/// Complete pending polls registered on `event`.
/// Called from wake paths AFTER releasing source locks (PIPES, device locks).
pub fn complete_pending_for_event(watchers: &[RingId], event: Source) {
    complete_pending_for_source(watchers, |pp| {
        pp.read_source == Some(event) || pp.write_source == Some(event)
    });
}

fn complete_pending_for_source(watchers: &[RingId], matches: impl Fn(&PendingPoll) -> bool) {
    if watchers.is_empty() { return; }

    // Collect the queues, wake after the table lock is gone: a wake posts
    // mailbox messages and may send a kick IPI, and neither needs IO_URINGS.
    let mut to_wake: Vec<Arc<KWaitQueue>> = Vec::new();
    let mut guard = IO_URINGS.lock();
    let map = guard.as_mut().expect("io_uring not initialized");

    for &ring_id in watchers {
        let Some(instance) = map.get_mut(ring_id) else { continue };

        let mut i = 0;
        while i < instance.pending_polls.len() {
            if matches(&instance.pending_polls[i]) {
                let pp = instance.pending_polls.swap_remove(i);
                let mut result_flags = 0u32;
                if pp.flags.readable() { result_flags |= PollFlags::IN.raw(); }
                if pp.flags.writable() { result_flags |= PollFlags::OUT.raw(); }
                instance.post_cqe(pp.user_data, result_flags as i32, 0);
            } else {
                i += 1;
            }
        }

        to_wake.push(instance.waiters.clone());
    }
    drop(guard);
    for queue in to_wake {
        wake_all(&queue);
    }
}

/// Cancel every pending poll on a source that is going away, in every ring
/// that was watching it. Called by the fd close path.
///
/// **Selected by source and never by fd number.** The rings this reaches
/// belong to *other* processes — that is the whole point of walking the
/// source's watcher list — and an fd number means nothing outside the process
/// that owns it. Matching on it cancelled a poll the closing process had never
/// heard of: a client exiting with its connection on fd 3 posted `-NotFound`
/// for whatever the server had on *its* fd 3, and a server whose listener sat
/// there then read ready with nothing queued and blocked in `accept` forever.
/// Found in the layout wizard's gate, where the wizard's fd 3 was the gate's
/// listener; the compositor is exposed to exactly the same shape.
///
/// **Every cancellation is woken.** The ring belongs to a thread parked in
/// `enter` on it — that is what a pending `POLL_ADD` means — and nothing else
/// can end that park: the poll is gone, so the source's own close-path wake
/// finds no watcher for it, and a `u64::MAX` wait never returns.
pub fn remove_fd(sources: &[Option<Source>]) {
    let mut affected: Vec<RingId> = Vec::new();
    for source in sources.iter().flatten() {
        for &id in source.watchers().iter() {
            if !affected.contains(&id) {
                affected.push(id);
            }
        }
    }

    if affected.is_empty() { return; }

    let watches_a_closing_source = |pp: &PendingPoll| {
        sources
            .iter()
            .flatten()
            .any(|&s| pp.read_source == Some(s) || pp.write_source == Some(s))
    };

    let mut to_wake: Vec<Arc<KWaitQueue>> = Vec::new();
    let mut guard = IO_URINGS.lock();
    let map = guard.as_mut().expect("io_uring not initialized");
    for ring_id in affected {
        if let Some(instance) = map.get_mut(ring_id) {
            // WatcherGuard drops with the poll → cleans the watcher lists.
            let mut i = 0;
            let mut cancelled = false;
            while i < instance.pending_polls.len() {
                if watches_a_closing_source(&instance.pending_polls[i]) {
                    let pp = instance.pending_polls.swap_remove(i);
                    // Post error CQE so userspace knows the poll was cancelled
                    instance.post_cqe(pp.user_data, -(SyscallError::NotFound as i32), 0);
                    cancelled = true;
                } else {
                    i += 1;
                }
            }
            if cancelled {
                to_wake.push(instance.waiters.clone());
            }
        }
    }
    drop(guard);
    for queue in to_wake {
        wake_all(&queue);
    }
}

/// Destroy an io_uring instance. Called when the ring fd is closed.

// Watcher list operations — dispatch to the source object

impl Source {
    /// Is the object ready right now? Called under the IO_URINGS lock during
    /// the TOCTOU recheck in `process_poll_add`.
    fn is_ready(self) -> bool {
        match self {
            Self::PipeReadable(id) => pipe::has_data(id),
            Self::PipeWritable(id) => pipe::has_space(id),
            Self::Listener(id) => crate::listener::has_pending_by_id(id),
            Self::Keyboard => crate::keyboard::has_data(),
            Self::Mouse => crate::mouse::has_data(),
            Self::Network => crate::net::has_packet(),
            Self::Audio => crate::audio::has_pending(),
        }
    }

    fn add_watcher(self, ring_id: RingId) {
        match self {
            Self::PipeReadable(pipe_id) | Self::PipeWritable(pipe_id) => {
                pipe::add_io_uring_watcher(pipe_id, ring_id);
            }
            Self::Keyboard => crate::keyboard::add_io_uring_watcher(ring_id),
            Self::Mouse => crate::mouse::add_io_uring_watcher(ring_id),
            Self::Network => crate::net::add_io_uring_watcher(ring_id),
            Self::Audio => crate::audio::add_io_uring_watcher(ring_id),
            Self::Listener(id) => crate::listener::add_io_uring_watcher(id, ring_id),
        }
    }

    fn remove_watcher(self, ring_id: RingId) {
        match self {
            Self::PipeReadable(pipe_id) | Self::PipeWritable(pipe_id) => {
                pipe::remove_io_uring_watcher(pipe_id, ring_id);
            }
            Self::Keyboard => crate::keyboard::remove_io_uring_watcher(ring_id),
            Self::Mouse => crate::mouse::remove_io_uring_watcher(ring_id),
            Self::Network => crate::net::remove_io_uring_watcher(ring_id),
            Self::Audio => crate::audio::remove_io_uring_watcher(ring_id),
            Self::Listener(id) => crate::listener::remove_io_uring_watcher(id, ring_id),
        }
    }

    fn watchers(self) -> Vec<RingId> {
        match self {
            Self::PipeReadable(pipe_id) | Self::PipeWritable(pipe_id) => {
                pipe::io_uring_watchers(pipe_id)
            }
            Self::Keyboard => crate::keyboard::io_uring_watchers(),
            Self::Mouse => crate::mouse::io_uring_watchers(),
            Self::Network => crate::net::io_uring_watchers(),
            Self::Audio => crate::audio::io_uring_watchers(),
            Self::Listener(id) => crate::listener::io_uring_watchers(id),
        }
    }
}

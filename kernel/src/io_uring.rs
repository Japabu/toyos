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

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use crate::fd;
use crate::id_map::{IdKey, IdMap};
use crate::pipe;
use crate::process::{self, Pid};
use crate::scheduler::{self, EventSource};
use crate::shared_memory::{self, SharedToken};
use crate::sync::Lock;
use crate::DirectMap;

use toyos_abi::io_uring::{
    IoUringCqe, IoUringParams, IoUringRingHeader, IoUringSqe,
    SQ_RING_OFF, CQ_RING_OFF, SQES_OFF,
};
use toyos_abi::syscall::SyscallError;

// ---------------------------------------------------------------------------
// RingId
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct RingId(usize);

impl RingId {
    #[allow(dead_code)]
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

// ---------------------------------------------------------------------------
// IoUringOp — type-safe op code, converted from raw u8 at boundary
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// PollFlags — type-safe poll interest flags
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// WatcherGuard — RAII cleanup of per-fd watcher lists
// ---------------------------------------------------------------------------

struct WatcherGuard {
    ring_id: RingId,
    sources: [Option<EventSource>; 2],
}

impl WatcherGuard {
    fn new(ring_id: RingId) -> Self {
        Self { ring_id, sources: [None; 2] }
    }

    fn add_source(&mut self, source: EventSource) {
        if self.sources[0].is_none() {
            self.sources[0] = Some(source);
        } else {
            self.sources[1] = Some(source);
        }
    }
}

impl Drop for WatcherGuard {
    fn drop(&mut self) {
        for source in &self.sources {
            if let Some(source) = source {
                remove_watcher_from_source(source, self.ring_id);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PendingPoll — a POLL_ADD that hasn't fired yet
// ---------------------------------------------------------------------------

struct PendingPoll {
    user_data: u64,
    fd_num: u32,
    flags: PollFlags,
    read_source: Option<EventSource>,
    write_source: Option<EventSource>,
    _watcher: WatcherGuard,
}

// ---------------------------------------------------------------------------
// IoUringInstance
// ---------------------------------------------------------------------------

/// Hard cap on pending polls per ring. With dedup this should never be reached
/// (bounded by number of open fds), but guards against future bugs.
const MAX_PENDING_POLLS: usize = 1024;

struct IoUringInstance {
    shm_phys: DirectMap,
    shm_token: SharedToken,
    sq_size: u32,
    cq_size: u32,
    pending_polls: Vec<PendingPoll>,
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

    /// Post a CQE. Asserts CQ is not full (structurally impossible with 2× sizing).
    fn post_cqe(&self, user_data: u64, result: i32, flags: u32) {
        let cq = self.cq_header();
        let head = cq.head.load(Ordering::Acquire);
        let tail = cq.tail.load(Ordering::Acquire);
        assert!(
            tail.wrapping_sub(head) < self.cq_size,
            "io_uring CQ overflow: head={head} tail={tail} size={}", self.cq_size
        );
        let idx = tail & (self.cq_size - 1);
        let cqe = self.cqe_at_mut(idx);
        cqe.user_data = user_data;
        cqe.result = result;
        cqe.flags = flags;
        cq.tail.store(tail.wrapping_add(1), Ordering::Release);
    }

    /// Count available CQEs (unread by userspace).
    fn cq_count(&self) -> u32 {
        let cq = self.cq_header();
        let head = cq.head.load(Ordering::Acquire);
        let tail = cq.tail.load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }
}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static IO_URINGS: Lock<Option<IdMap<RingId, IoUringInstance>>> = Lock::new(None);

pub fn init() {
    *IO_URINGS.lock() = Some(IdMap::new());
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

/// Create an io_uring instance. Returns (ring_id, shared_memory_token).
pub fn create(depth: u32) -> Result<(RingId, SharedToken), SyscallError> {
    // Validate: power of 2, max 256
    if depth == 0 || depth > 256 || !depth.is_power_of_two() {
        return Err(SyscallError::InvalidArgument);
    }

    let sq_size = depth;
    let cq_size = depth * 2;

    let pid = process::current_process();
    let addr_space = process::current_address_space();
    let shm_token = shared_memory::alloc(crate::mm::PAGE_2M, pid, &addr_space);

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

    // Initialize SQ ring header
    let sq_header = unsafe { &mut *(base.add(SQ_RING_OFF as usize) as *mut IoUringRingHeader) };
    sq_header.head = core::sync::atomic::AtomicU32::new(0);
    sq_header.tail = core::sync::atomic::AtomicU32::new(0);
    sq_header.ring_size = sq_size;
    sq_header._pad = 0;

    // Initialize CQ ring header
    let cq_header = unsafe { &mut *(base.add(CQ_RING_OFF as usize) as *mut IoUringRingHeader) };
    cq_header.head = core::sync::atomic::AtomicU32::new(0);
    cq_header.tail = core::sync::atomic::AtomicU32::new(0);
    cq_header.ring_size = cq_size;
    cq_header._pad = 0;

    let ring_id = {
        let mut guard = IO_URINGS.lock();
        let map = guard.as_mut().expect("io_uring not initialized");
        map.insert(IoUringInstance {
            shm_phys,
            shm_token,
            sq_size,
            cq_size,
            pending_polls: Vec::new(),
            owner_pid: pid,
        })
    };

    Ok((ring_id, shm_token))
}

// ---------------------------------------------------------------------------
// Enter — submit SQEs and/or wait for CQEs
// ---------------------------------------------------------------------------

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

    // Submit phase
    if to_submit > 0 {
        submit_sqes(ring_id, to_submit)?;
    }

    // Wait phase
    loop {
        let count = {
            let guard = IO_URINGS.lock();
            let map = guard.as_ref().expect("io_uring not initialized");
            let instance = map.get(ring_id).ok_or(SyscallError::NotFound)?;
            instance.cq_count()
        };

        if count >= min_complete || min_complete == 0 {
            return Ok(count);
        }

        // Non-blocking check
        if deadline == 1 {
            return Ok(count);
        }

        // Timeout check
        if deadline > 0 && crate::clock::nanos_since_boot() >= deadline {
            return Ok(count);
        }

        scheduler::block(Some(EventSource::IoUring(ring_id)), deadline);
    }
}

/// Read and process SQEs from the submission ring.
fn submit_sqes(ring_id: RingId, count: u32) -> Result<(), SyscallError> {
    let guard = IO_URINGS.lock();
    let map = guard.as_ref().expect("io_uring not initialized");
    let instance = map.get(ring_id).ok_or(SyscallError::NotFound)?;

    let sq = instance.sq_header();
    let head = sq.head.load(Ordering::Acquire);
    let tail = sq.tail.load(Ordering::Acquire);
    let available = tail.wrapping_sub(head);
    let to_process = count.min(available);

    // Copy SQEs out so we can release the lock
    let mut sqes = Vec::with_capacity(to_process as usize);
    for i in 0..to_process {
        let idx = (head.wrapping_add(i)) & (instance.sq_size - 1);
        sqes.push(*instance.sqe_at(idx));
    }

    // Advance SQ head
    sq.head.store(head.wrapping_add(to_process), Ordering::Release);
    drop(guard);

    // Process each SQE
    for sqe in &sqes {
        process_sqe(ring_id, sqe);
    }

    Ok(())
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
            data.fds.get(fd_num).and_then(|d| d.read_event_source())
        } else { None };
        let wsrc = if flags.writable() {
            data.fds.get(fd_num).and_then(|d| d.write_event_source())
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
    let mut guard = IO_URINGS.lock();
    let map = guard.as_mut().expect("io_uring not initialized");
    if let Some(instance) = map.get_mut(ring_id) {
        // Remove existing PendingPoll for this fd (drops old WatcherGuard)
        if let Some(pos) = instance.pending_polls.iter().position(|pp| pp.fd_num == fd_num) {
            instance.pending_polls.swap_remove(pos);
        }

        let mut watcher = WatcherGuard::new(ring_id);
        if let Some(src) = read_source {
            add_watcher_to_source(&src, ring_id);
            watcher.add_source(src);
        }
        if let Some(src) = write_source {
            add_watcher_to_source(&src, ring_id);
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
            return;
        }

        // Recheck: close TOCTOU window between readiness check and PendingPoll
        // insertion. A concurrent wake (complete_pending_for_event) either already
        // ran and found no PendingPoll (recheck catches the data it left behind),
        // or is blocked on IO_URINGS and will find the PendingPoll after we release.
        let became_ready = read_source.as_ref().map_or(false, source_ready)
            || write_source.as_ref().map_or(false, source_ready);
        if became_ready {
            if let Some(pos) = instance.pending_polls.iter().position(|pp| pp.fd_num == fd_num) {
                let pp = instance.pending_polls.swap_remove(pos);
                let mut result_flags = 0u32;
                if pp.flags.readable() { result_flags |= PollFlags::IN.raw(); }
                if pp.flags.writable() { result_flags |= PollFlags::OUT.raw(); }
                instance.post_cqe(pp.user_data, result_flags as i32, 0);
                scheduler::push_event(EventSource::IoUring(ring_id));
            }
        }
    }
}

fn process_poll_remove(ring_id: RingId, target_user_data: u64) {
    let mut guard = IO_URINGS.lock();
    let map = guard.as_mut().expect("io_uring not initialized");
    if let Some(instance) = map.get_mut(ring_id) {
        if let Some(pos) = instance.pending_polls.iter().position(|p| p.user_data == target_user_data) {
            instance.pending_polls.swap_remove(pos);
            // Post CQE for the POLL_REMOVE itself (success)
            instance.post_cqe(target_user_data, 0, 0);
        } else {
            // Not found — post error CQE
            instance.post_cqe(target_user_data, -(SyscallError::NotFound as i32), 0);
        }
    }
}

fn process_accept(ring_id: RingId, sqe: &IoUringSqe) {
    let fd_num = sqe.fd as u32;
    let user_data = sqe.user_data;

    // Get the listener name from the fd
    let listener_name = process::with_fd_owner_data(|data| {
        match data.fds.get(fd_num) {
            Some(fd::Descriptor::Listener(name)) => Some(name.clone()),
            _ => None,
        }
    });

    let Some(name) = listener_name else {
        post_cqe_locked(ring_id, user_data, -(SyscallError::InvalidArgument as i32), 0);
        return;
    };

    match crate::listener::pop_connection(&name) {
        Some(conn) => {
            // Create socket fd from the pending connection
            let new_fd = process::with_fd_owner_data(|data| {
                data.fds.insert(fd::Descriptor::Socket {
                    rx: conn.rx,
                    tx: conn.tx,
                })
            });
            post_cqe_locked(ring_id, user_data, new_fd as i32, 0);
        }
        None => {
            post_cqe_locked(ring_id, user_data, -(SyscallError::WouldBlock as i32), 0);
        }
    }
}

fn process_close(ring_id: RingId, sqe: &IoUringSqe) {
    let fd_num = sqe.fd as u32;
    let user_data = sqe.user_data;
    let pid = process::current_process();

    let result = process::with_fd_owner_data(|data| {
        fd::close(&mut data.fds, &mut *crate::vfs::lock(), fd_num, pid)
    });

    post_cqe_locked(ring_id, user_data, result as i32, 0);
}

/// Post a CQE, acquiring the IO_URINGS lock.
fn post_cqe_locked(ring_id: RingId, user_data: u64, result: i32, flags: u32) {
    let guard = IO_URINGS.lock();
    let map = guard.as_ref().expect("io_uring not initialized");
    if let Some(instance) = map.get(ring_id) {
        instance.post_cqe(user_data, result, flags);
    }
}

// ---------------------------------------------------------------------------
// Wake path — called when a source becomes ready
// ---------------------------------------------------------------------------

/// Complete pending polls that match a given event source.
/// Called from wake paths AFTER releasing source locks (PIPES, device locks).
pub fn complete_pending_for_event(watchers: &[RingId], event: EventSource) {
    complete_pending_for_source(watchers, |pp| {
        pp.read_source == Some(event) || pp.write_source == Some(event)
    });
}

fn complete_pending_for_source(watchers: &[RingId], matches: impl Fn(&PendingPoll) -> bool) {
    if watchers.is_empty() { return; }

    let mut guard = IO_URINGS.lock();
    let map = guard.as_mut().expect("io_uring not initialized");

    for &ring_id in watchers {
        let Some(instance) = map.get_mut(ring_id) else { continue };

        // Find and remove matching pending polls
        let mut i = 0;
        while i < instance.pending_polls.len() {
            if matches(&instance.pending_polls[i]) {
                let pp = instance.pending_polls.swap_remove(i);
                let mut result_flags = 0u32;
                if pp.flags.readable() { result_flags |= PollFlags::IN.raw(); }
                if pp.flags.writable() { result_flags |= PollFlags::OUT.raw(); }
                instance.post_cqe(pp.user_data, result_flags as i32, 0);
                // Don't increment i — swap_remove moved the last element here
            } else {
                i += 1;
            }
        }

        // Push wake event for the thread blocked on this ring
        scheduler::push_event(EventSource::IoUring(ring_id));
    }
}

// ---------------------------------------------------------------------------
// FD close integration
// ---------------------------------------------------------------------------

/// Remove all pending polls for a given fd from all affected rings.
/// Called by the fd close path. Uses source watcher lists to find affected rings.
pub fn remove_fd(fd_num: u32, sources: &[Option<EventSource>]) {
    let mut affected: Vec<RingId> = Vec::new();
    for source in sources.iter().flatten() {
        for &id in watchers_for_source(source).iter() {
            if !affected.contains(&id) {
                affected.push(id);
            }
        }
    }

    if affected.is_empty() { return; }

    let mut guard = IO_URINGS.lock();
    let map = guard.as_mut().expect("io_uring not initialized");
    for ring_id in affected {
        if let Some(instance) = map.get_mut(ring_id) {
            // Remove all pending polls for this fd (WatcherGuard drops → cleans watcher lists)
            let mut i = 0;
            while i < instance.pending_polls.len() {
                if instance.pending_polls[i].fd_num == fd_num {
                    let pp = instance.pending_polls.swap_remove(i);
                    // Post error CQE so userspace knows the poll was cancelled
                    instance.post_cqe(pp.user_data, -(SyscallError::NotFound as i32), 0);
                } else {
                    i += 1;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Destroy
// ---------------------------------------------------------------------------

/// Destroy an io_uring instance. Called when the ring fd is closed.
pub fn destroy(ring_id: RingId) {
    let instance = {
        let mut guard = IO_URINGS.lock();
        let map = guard.as_mut().expect("io_uring not initialized");
        map.remove(ring_id)
    };

    if let Some(mut instance) = instance {
        // Drop all pending polls (WatcherGuards clean up watcher lists)
        instance.pending_polls.clear();
        // Destroy shared memory region — unmaps from all processes, frees backing pages
        let _ = shared_memory::destroy(instance.shm_token, instance.owner_pid);
    }
}

// ---------------------------------------------------------------------------
// Watcher list operations — dispatch to the source object
// ---------------------------------------------------------------------------

/// Check if an event source is currently ready. Called under IO_URINGS lock
/// during the TOCTOU recheck in process_poll_add.
fn source_ready(source: &EventSource) -> bool {
    match source {
        EventSource::PipeReadable(id) => pipe::has_data(*id),
        EventSource::PipeWritable(id) => pipe::has_space(*id),
        EventSource::Listener(id) => crate::listener::has_pending_by_id(*id),
        EventSource::Keyboard => crate::keyboard::has_data(),
        EventSource::Mouse => crate::mouse::has_data(),
        EventSource::Network => crate::net::has_packet(),
        EventSource::Futex(_) | EventSource::IoUring(_) => false,
    }
}

fn add_watcher_to_source(source: &EventSource, ring_id: RingId) {
    match source {
        EventSource::PipeReadable(pipe_id) | EventSource::PipeWritable(pipe_id) => {
            pipe::add_io_uring_watcher(*pipe_id, ring_id);
        }
        EventSource::Keyboard => crate::keyboard::add_io_uring_watcher(ring_id),
        EventSource::Mouse => crate::mouse::add_io_uring_watcher(ring_id),
        EventSource::Network => crate::net::add_io_uring_watcher(ring_id),
        EventSource::Listener(id) => crate::listener::add_io_uring_watcher(*id, ring_id),
        EventSource::Futex(_) | EventSource::IoUring(_) => {}
    }
}

fn remove_watcher_from_source(source: &EventSource, ring_id: RingId) {
    match source {
        EventSource::PipeReadable(pipe_id) | EventSource::PipeWritable(pipe_id) => {
            pipe::remove_io_uring_watcher(*pipe_id, ring_id);
        }
        EventSource::Keyboard => crate::keyboard::remove_io_uring_watcher(ring_id),
        EventSource::Mouse => crate::mouse::remove_io_uring_watcher(ring_id),
        EventSource::Network => crate::net::remove_io_uring_watcher(ring_id),
        EventSource::Listener(id) => crate::listener::remove_io_uring_watcher(*id, ring_id),
        EventSource::Futex(_) | EventSource::IoUring(_) => {}
    }
}

pub fn watchers_for_source(source: &EventSource) -> Vec<RingId> {
    match source {
        EventSource::PipeReadable(pipe_id) | EventSource::PipeWritable(pipe_id) => {
            pipe::io_uring_watchers(*pipe_id)
        }
        EventSource::Keyboard => crate::keyboard::io_uring_watchers(),
        EventSource::Mouse => crate::mouse::io_uring_watchers(),
        EventSource::Network => crate::net::io_uring_watchers(),
        EventSource::Listener(id) => crate::listener::io_uring_watchers(*id),
        EventSource::Futex(_) | EventSource::IoUring(_) => Vec::new(),
    }
}

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

use crate::object::shm::SharedMemObject;
use crate::object::{ops, KObjectRef};
use crate::id_map::{IdKey, IdMap};
use crate::pipe::{self, PipeId};
use crate::process::{self, Pid};
use crate::scheduler;
use crate::sync::Lock;
use crate::completion::{self, Watch};
use crate::time::{Deadline, Duration};
use crate::DirectMap;

use toyos_abi::io_uring::{
    IoUringCqe, IoUringParams, IoUringRingHeader, IoUringSqe,
    SQ_RING_OFF, CQ_RING_OFF, SQES_OFF,
};
use toyos_abi::handle::{RawHandle, Rights};
use toyos_abi::syscall::SyscallError;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct RingId(usize);

impl RingId {
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
/// Held by an `IoUringObject`, so a `dup`ped ring handle stays usable after the
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
            for poll in instance.pending_polls.drain(..) {
                for source in [poll.read_source, poll.write_source].into_iter().flatten() {
                    source.remove_watcher(self.0);
                }
            }
            // Unmap, flush, and only then let go of the pages: `Unmapped`'s
            // drop is the flush and the `Arc` below is what frees.
            drop(instance.shm.unmap_from(instance.owner_pid));
        }
    }
}

// IoUringOp — type-safe op code, converted from raw u8 at boundary

#[derive(Clone, Copy)]
pub enum IoUringOp {
    Nop,
    PollAdd,
    Accept,
}

impl IoUringOp {
    fn from_raw(raw: u8) -> Result<Self, SyscallError> {
        // 2 is retired (`toyos_abi::io_uring`, formerly IORING_OP_POLL_REMOVE)
        // and falls to the refusal like every other number nothing declares.
        match raw {
            0 => Ok(Self::Nop),
            1 => Ok(Self::PollAdd),
            3 => Ok(Self::Accept),
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
///
/// **A port is named by the object and never by a number.** There is no
/// registry to look an acceptor up in any more, so the watch *holds* what it
/// watches — which is also what stops a poll outliving the port it names. It
/// holds the *shared* half rather than either end, because the poll a server
/// registers on its `Acceptor` is completed by a client connecting through a
/// `Connector`, and that is the one thing the two have in common.
#[derive(Clone)]
pub enum Source {
    Keyboard,
    Mouse,
    Network,
    Port(Arc<crate::object::port::PortShared>),
    PipeReadable(PipeId),
    PipeWritable(PipeId),
    VirtioSound,
    Hda,
    /// The machine's kernel log, named by a `SysCap` that carries
    /// `Rights::LOG`.
    ///
    /// **Edge-triggered, and it is the one source that has to be.** Readiness
    /// here means "records have moved", never "there is something for you": the
    /// kernel holds no reader's cursor, so it cannot answer the second at all.
    /// A reader closes the window itself by reading once more after submitting
    /// the poll — the same arm-then-rescan `klogd` does on the kernel's side —
    /// which is why [`Source::is_ready`] answers `false` here and every
    /// completion comes from `log::user::post_readiness`.
    Log,
}

impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Keyboard, Self::Keyboard)
            | (Self::Mouse, Self::Mouse)
            | (Self::Network, Self::Network)
            | (Self::VirtioSound, Self::VirtioSound)
            | (Self::Log, Self::Log)
            | (Self::Hda, Self::Hda) => true,
            (Self::Port(a), Self::Port(b)) => Arc::ptr_eq(a, b),
            (Self::PipeReadable(a), Self::PipeReadable(b)) => a == b,
            (Self::PipeWritable(a), Self::PipeWritable(b)) => a == b,
            _ => false,
        }
    }
}

// PendingPoll — a POLL_ADD that hasn't fired yet

struct PendingPoll {
    user_data: u64,
    /// The handle the poll was submitted against, and the dedup key. A handle
    /// is a slot in *this* process's table, so it is only ever compared with
    /// another poll on the same ring — which is the whole of what dedup needs.
    handle: RawHandle,
    flags: PollFlags,
    read_source: Option<Source>,
    write_source: Option<Source>,
}

impl PendingPoll {
    fn watches(&self, source: &Source) -> bool {
        self.read_source.as_ref() == Some(source) || self.write_source.as_ref() == Some(source)
    }
}

/// Take the poll at `index` out, unregistering this ring from any source no
/// other poll of the same ring still names.
///
/// **A source's watcher list is a set of rings, not a count**, so removing the
/// registration unconditionally — which is what an RAII guard beside each poll
/// did — disarms a sibling poll of the same ring on the same object. Two
/// handles to one object in one ring is the reachable shape (a `dup`ped
/// acceptor polled through both), and nothing about the failure is visible: the
/// poll stays in the list and no wake ever reaches it again. The guard could
/// not have got this right, because whether a registration is still owed is a
/// property of the ring and not of the poll.
fn take_poll(instance: &mut IoUringInstance, index: usize) -> PendingPoll {
    let poll = instance.pending_polls.swap_remove(index);
    for source in [&poll.read_source, &poll.write_source].into_iter().flatten() {
        if !instance.pending_polls.iter().any(|p| p.watches(source)) {
            source.remove_watcher(instance.id);
        }
    }
    poll
}

/// Hard cap on pending polls per ring. With dedup this should never be reached
/// (bounded by number of open fds), but guards against future bugs.
const MAX_PENDING_POLLS: usize = 1024;

struct IoUringInstance {
    id: RingId,
    shm_phys: DirectMap,
    /// The ring's own pages. **A ring is not something two processes share**,
    /// so its page has no lifetime of its own and no second name: it goes with
    /// the last handle to the ring.
    shm: alloc::sync::Arc<SharedMemObject>,
    /// Live `RingRef`s. Never zero while this entry is in the map.
    refs: u32,
    sq_size: u32,
    cq_size: u32,
    pending_polls: Vec<PendingPoll>,
    /// Threads armed on this ring's completion queue (spec §8.6), cloned out
    /// of the table because `enter` holds it across its park.
    ///
    /// **It was half of a `Wakeable` pair, and the other half was dead.** An
    /// `Arc<KWaitQueue>` stood beside it — minted at setup, cloned at five wake
    /// sites and walked on every CQE — with nothing registered on it since
    /// `enter` started parking through `completion::wait_until` on the calling
    /// thread's own queue. The pair's own doc said it existed "so a site cannot
    /// take one and forget the other", which is a real hazard
    /// (`issues/kernel/io-uring-source-half-a-wake-pair.md` records losing
    /// it twice) and was not that type's to prevent: §5.6's answer is that there
    /// is **no pair**, and a type minted to enforce one is the pair surviving
    /// under a new name. Both the alias and the `wakeable()` accessor are gone
    /// with it, because a synonym for one field earns nothing.
    watch: Arc<Watch>,
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

/// Create an io_uring instance and map its rings into the caller. Answers the
/// reference and the address the rings are at.
pub fn create(depth: u32) -> Result<(RingRef, u64), SyscallError> {
    if depth == 0 || depth > MAX_SQ_DEPTH || !depth.is_power_of_two() {
        return Err(SyscallError::InvalidArgument);
    }

    let sq_size = depth;
    let cq_size = depth * 2;

    let pid = process::current_process();
    let addr_space = process::current_address_space();
    let shm = SharedMemObject::create(crate::mm::PAGE_2M)?;
    let shm_vaddr = shm.map_into(pid, &addr_space)?;
    let shm_phys = shm.phys();

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
        map.insert_with(|id| IoUringInstance {
            id,
            shm_phys,
            shm,
            refs: 1,
            sq_size,
            cq_size,
            pending_polls: Vec::new(),
            watch: Arc::new(Watch::new()),
            cq_tail: core::cell::Cell::new(0),
            owner_pid: pid,
        })
    };

    Ok((RingRef(ring_id), shm_vaddr))
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
    // **Three readings of one word, and this is where they stop.** The relative
    // `timeout_nanos` still arrives from userland with `0` meaning non-blocking
    // and `u64::MAX` meaning forever — that is the ABI until C11 — but inside
    // the kernel each becomes a named `Deadline`: `passed()` is evaluate-once,
    // `never()` arms no timer, and anything else is an instant. What this
    // replaces mapped relative `0` onto absolute `1` and `1` back onto `0` —
    // the motivating example for why the absolute form may not be a bare
    // `u64`.
    let non_blocking = timeout_nanos == 0;
    let deadline = if non_blocking {
        Deadline::passed()
    } else if timeout_nanos == u64::MAX {
        Deadline::never()
    } else {
        Deadline::at(crate::clock::now() + Duration::from_nanos(timeout_nanos))
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

        if non_blocking {
            return Ok(count);
        }

        // A ring that has thrown a completion away must not be slept on: the
        // one this thread waits for may be the one discarded. Returning short
        // puts the counter in front of `Poller::wait`'s assertion, which is
        // otherwise read only after the call that blocks.
        if dropped > 0 {
            return Ok(count);
        }

        if deadline.reached(crate::clock::now()) {
            return Ok(count);
        }

        // The re-check is this ring's own condition, not mere readiness: a
        // waiter for `min_complete` CQEs that cancelled on the first one would
        // spin instead of parking. It runs *after* the arm, inside
        // `completion::wait_until`, which is what closes the window a sibling
        // thread closing this ring's fd opens.
        let parkable = scheduler::Parkable::of_current();
        if completion::wait_until(
            &parkable,
            completion::Subject::of(&queue),
            completion::Token::new(ring_id.0 as u64),
            WaitClass::Io,
            deadline,
            || cq_count(ring_id).map_or(true, |n| n >= min_complete),
        )
        .is_err()
        {
            return Err(SyscallError::Gone);
        }
    }
}

/// This ring's completion waiter set, cloned out of the table.
fn waiters_of(ring_id: RingId) -> Result<Arc<Watch>, SyscallError> {
    with_instance(ring_id, |inst| inst.watch.clone())
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
        IoUringOp::Accept => {
            process_accept(ring_id, sqe);
        }
    }
}

fn process_poll_add(ring_id: RingId, sqe: &IoUringSqe) {
    let handle = sqe.fd;
    let flags = PollFlags::from_raw(sqe.op_flags);
    let user_data = sqe.user_data;

    // Readiness first, on the process's table rather than the thread's: a ring
    // is process-wide. A handle without `WAIT` is not watchable, and answers as
    // if it were not there.
    let (ready, read_source, write_source) = process::with_fd_owner_data(|data| {
        let Ok(object) = data.handles.get_ref(handle, Rights::WAIT) else {
            return (false, None, None);
        };
        let readable = flags.readable() && ops::has_data(object);
        let writable = flags.writable() && ops::has_space(object);
        let rsrc = if flags.readable() { ops::read_source(object) } else { None };
        let wsrc = if flags.writable() { ops::write_source(object) } else { None };
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
    // The old poll on this handle goes first, so its unregistration cannot
    // undo the registration this one is about to make.
    let mut woken: Option<Arc<Watch>> = None;
    let mut guard = IO_URINGS.lock();
    let map = guard.as_mut().expect("io_uring not initialized");
    if let Some(instance) = map.get_mut(ring_id) {
        if let Some(pos) = instance.pending_polls.iter().position(|pp| pp.handle == handle) {
            take_poll(instance, pos);
        }

        for src in [&read_source, &write_source].into_iter().flatten() {
            src.add_watcher(ring_id);
        }

        let new_pp = PendingPoll {
            user_data,
            handle,
            flags,
            read_source: read_source.clone(),
            write_source: write_source.clone(),
        };

        if instance.pending_polls.len() < MAX_PENDING_POLLS {
            instance.pending_polls.push(new_pp);
        } else {
            instance.post_cqe(user_data, -(SyscallError::ResourceExhausted as i32), 0);
            let watch = instance.watch.clone();
            drop(guard);
            completion::post(completion::Subject::of(&watch), completion::Outcome::Ready);
            return;
        }

        // Recheck: close TOCTOU window between readiness check and PendingPoll
        // insertion. A concurrent wake (complete_pending_for_event) either already
        // ran and found no PendingPoll (recheck catches the data it left behind),
        // or is blocked on IO_URINGS and will find the PendingPoll after we release.
        let became_ready = read_source.as_ref().is_some_and(Source::is_ready)
            || write_source.as_ref().is_some_and(Source::is_ready);
        if became_ready {
            if let Some(pos) = instance.pending_polls.iter().position(|pp| pp.handle == handle) {
                let pp = take_poll(instance, pos);
                let mut result_flags = 0u32;
                if pp.flags.readable() { result_flags |= PollFlags::IN.raw(); }
                if pp.flags.writable() { result_flags |= PollFlags::OUT.raw(); }
                instance.post_cqe(pp.user_data, result_flags as i32, 0);
                woken = Some(instance.watch.clone());
            }
        }
    }
    drop(guard);
    if let Some(watch) = woken {
        completion::post(completion::Subject::of(&watch), completion::Outcome::Ready);
    }
}

fn process_accept(ring_id: RingId, sqe: &IoUringSqe) {
    let user_data = sqe.user_data;

    let acceptor = process::with_fd_owner_data(|data| {
        match data.handles.get_ref(sqe.fd, Rights::READ) {
            Ok(KObjectRef::Acceptor(a)) => Some(a.clone()),
            _ => None,
        }
    });

    let Some(acceptor) = acceptor else {
        post_cqe_locked(ring_id, user_data, -(SyscallError::InvalidArgument as i32), 0);
        return;
    };

    match acceptor.pop() {
        Some(conn) => {
            let installed = process::with_fd_owner_data(|data| {
                ops::install(
                    &mut data.handles,
                    KObjectRef::Connection(crate::object::service::ConnectionEnd::new(
                        conn.rx,
                        conn.tx,
                        conn.inbox,
                        conn.outbox,
                    )),
                )
            });
            match installed {
                Ok(h) => post_cqe_locked(ring_id, user_data, h.0 as i32, 0),
                Err(e) => post_cqe_locked(ring_id, user_data, -(e as i32), 0),
            }
        }
        None => {
            post_cqe_locked(ring_id, user_data, -(SyscallError::WouldBlock as i32), 0);
        }
    }
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
        instance.watch.clone()
    });
    drop(guard);
    if let Some(watch) = woken {
        completion::post(completion::Subject::of(&watch), completion::Outcome::Ready);
    }
}

// Wake path — called when a source becomes ready

/// Complete pending polls registered on `event`.
/// Called from wake paths AFTER releasing source locks (PIPES, device locks).
pub fn complete_pending_for_event(watchers: &[RingId], event: Source) {
    complete_pending_for_source(watchers, |pp| pp.watches(&event));
}

fn complete_pending_for_source(watchers: &[RingId], matches: impl Fn(&PendingPoll) -> bool) {
    if watchers.is_empty() { return; }

    // Collect the queues, wake after the table lock is gone: a wake posts
    // mailbox messages and may send a kick IPI, and neither needs IO_URINGS.
    let mut to_wake: Vec<Arc<Watch>> = Vec::new();
    let mut guard = IO_URINGS.lock();
    let map = guard.as_mut().expect("io_uring not initialized");

    for &ring_id in watchers {
        let Some(instance) = map.get_mut(ring_id) else { continue };

        let mut i = 0;
        while i < instance.pending_polls.len() {
            if matches(&instance.pending_polls[i]) {
                let pp = take_poll(instance, i);
                let mut result_flags = 0u32;
                if pp.flags.readable() { result_flags |= PollFlags::IN.raw(); }
                if pp.flags.writable() { result_flags |= PollFlags::OUT.raw(); }
                instance.post_cqe(pp.user_data, result_flags as i32, 0);
            } else {
                i += 1;
            }
        }

        to_wake.push(instance.watch.clone());
    }
    drop(guard);
    for watch in to_wake {
        completion::post(completion::Subject::of(&watch), completion::Outcome::Ready);
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

    let watches_a_closing_source =
        |pp: &PendingPoll| sources.iter().flatten().any(|s| pp.watches(s));

    let mut to_wake: Vec<Arc<Watch>> = Vec::new();
    let mut guard = IO_URINGS.lock();
    let map = guard.as_mut().expect("io_uring not initialized");
    for ring_id in affected {
        if let Some(instance) = map.get_mut(ring_id) {
            let mut i = 0;
            let mut cancelled = false;
            while i < instance.pending_polls.len() {
                if watches_a_closing_source(&instance.pending_polls[i]) {
                    let pp = take_poll(instance, i);
                    // Post error CQE so userspace knows the poll was cancelled
                    instance.post_cqe(pp.user_data, -(SyscallError::NotFound as i32), 0);
                    cancelled = true;
                } else {
                    i += 1;
                }
            }
            if cancelled {
                to_wake.push(instance.watch.clone());
            }
        }
    }
    drop(guard);
    for watch in to_wake {
        completion::post(completion::Subject::of(&watch), completion::Outcome::Ready);
    }
}

// Watcher list operations — dispatch to the source object

impl Source {
    /// Is the object ready right now? Called under the IO_URINGS lock during
    /// the TOCTOU recheck in `process_poll_add`.
    fn is_ready(&self) -> bool {
        match self {
            Self::PipeReadable(id) => pipe::has_data(*id),
            Self::PipeWritable(id) => pipe::has_space(*id),
            Self::Port(p) => p.has_pending(),
            Self::Keyboard => crate::keyboard::has_data(),
            Self::Mouse => crate::mouse::has_data(),
            Self::Network => crate::net::has_packet(),
            Self::VirtioSound => crate::drivers::virtio_sound::has_pending(),
            Self::Hda => crate::drivers::hda::has_pending(),
            // Never, and the variant's own doc is the argument: this recheck
            // asks "is the object ready", and for the log that question is
            // about a cursor the kernel does not hold. Answering `true` would
            // complete every poll immediately and turn a parked reader into a
            // spinning one.
            Self::Log => false,
        }
    }

    fn add_watcher(&self, ring_id: RingId) {
        match self {
            Self::PipeReadable(pipe_id) | Self::PipeWritable(pipe_id) => {
                pipe::add_io_uring_watcher(*pipe_id, ring_id);
            }
            Self::Keyboard => crate::keyboard::add_io_uring_watcher(ring_id),
            Self::Mouse => crate::mouse::add_io_uring_watcher(ring_id),
            Self::Network => crate::net::add_io_uring_watcher(ring_id),
            Self::VirtioSound => crate::drivers::virtio_sound::add_io_uring_watcher(ring_id),
            Self::Hda => crate::drivers::hda::add_io_uring_watcher(ring_id),
            Self::Log => crate::log::user::add_io_uring_watcher(ring_id),
            Self::Port(p) => p.add_watcher(ring_id),
        }
    }

    fn remove_watcher(&self, ring_id: RingId) {
        match self {
            Self::PipeReadable(pipe_id) | Self::PipeWritable(pipe_id) => {
                pipe::remove_io_uring_watcher(*pipe_id, ring_id);
            }
            Self::Keyboard => crate::keyboard::remove_io_uring_watcher(ring_id),
            Self::Mouse => crate::mouse::remove_io_uring_watcher(ring_id),
            Self::Network => crate::net::remove_io_uring_watcher(ring_id),
            Self::VirtioSound => crate::drivers::virtio_sound::remove_io_uring_watcher(ring_id),
            Self::Hda => crate::drivers::hda::remove_io_uring_watcher(ring_id),
            Self::Log => crate::log::user::remove_io_uring_watcher(ring_id),
            Self::Port(p) => p.remove_watcher(ring_id),
        }
    }

    fn watchers(&self) -> Vec<RingId> {
        match self {
            Self::PipeReadable(pipe_id) | Self::PipeWritable(pipe_id) => {
                pipe::io_uring_watchers(*pipe_id)
            }
            Self::Keyboard => crate::keyboard::io_uring_watchers(),
            Self::Mouse => crate::mouse::io_uring_watchers(),
            Self::Network => crate::net::io_uring_watchers(),
            Self::VirtioSound => crate::drivers::virtio_sound::io_uring_watchers(),
            Self::Hda => crate::drivers::hda::io_uring_watchers(),
            Self::Log => crate::log::user::io_uring_watchers(),
            Self::Port(p) => p.watchers(),
        }
    }
}

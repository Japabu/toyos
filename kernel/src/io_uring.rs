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
                for source in poll.sources.iter() {
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

/// A source whose whole lifetime is one object's.
///
/// **[`remove_fd`] takes only these, and that is what makes the mistake it
/// exists to stop a compile error rather than a review note.** Cancellation is
/// by source across every ring in the machine, which is what a pipe needs — a
/// client closing its end must complete the server's poll on the other, and an
/// fd number means nothing outside the process that owns it. Handing it a
/// source the closing object does *not* own cancels polls that belong to
/// processes which were never consulted, and there is now no way to write that:
/// [`Source::ended_by_its_last_handle`] is the only constructor.
pub struct EndedSource(Source);

impl Source {
    /// This source, if the last handle to the object naming it is what ends it.
    ///
    /// **Two sources answer `None`, and both are the machine's rather than any
    /// holder's.** [`Source::Log`] is named by every `SysCap`, and the machine's
    /// log is not something a capability going away ends: closing one is a
    /// process putting down its authority to read a stream that outlives every
    /// handle, and `/bin/logd`'s whole loop is read-then-park, so that was a
    /// daemon which stopped reading the moment anything anywhere closed a
    /// capability. [`Source::Keyboard`] is the machine's one keyboard, which no
    /// claim and no console creates or destroys: the `Device(Keyboard)` claim
    /// names it *and* so does every `Console` (`object::ops::read_source`), so
    /// the claim's holder closing its handle posted `-NotFound` into every
    /// pending poll on stdin in the machine — which is what libc's terminal read
    /// arms — for processes that hold no device. It stayed quiet only because
    /// the compositor takes the claim at boot and holds it until the machine
    /// stops; a restart, a handoff or a rearm would have cancelled every
    /// terminal read on the machine in between.
    ///
    /// **The question is the source's and asking the object was the defect.**
    /// What makes cancelling safe is that no *other kind* of object names the
    /// same source, and an exhaustive match over `KObjectRef` cannot state that
    /// — `object::ops` had one, and its argument was "a claim admits exactly one
    /// handle by construction, so every ring watching it is the one holder's",
    /// which is true of the claim and false of the source. The match is here
    /// because the fact is here, beside [`Source::is_ready`] and
    /// [`Source::watchers`], and a source added to this enum has to answer it.
    ///
    /// Every other source really is its object's: a pipe end, a connection, a
    /// port and the four remaining device classes each go away with their last
    /// handle, and nothing else in the kernel names any of them.
    pub fn ended_by_its_last_handle(self) -> Option<EndedSource> {
        // The negative controls restore the prior behaviour for one source
        // each, so the gate covering it reds on the tree that had it.
        // `log-close-cancels-any-syscap` covered both while the question was
        // asked of the object; the keyboard half has its own name now, because
        // a keyboard *claim* closing is the reachable stimulus for it and no
        // `SysCap` is involved.
        let ends = match self {
            Self::Log => crate::actuator::log_close_cancels_any_syscap(),
            Self::Keyboard => crate::actuator::keyboard_close_cancels_every_console(),
            Self::Mouse
            | Self::Network
            | Self::VirtioSound
            | Self::Hda
            | Self::Port(_)
            | Self::PipeReadable(_)
            | Self::PipeWritable(_) => true,
        };
        ends.then_some(EndedSource(self))
    }
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

/// The sources a pending poll is registered on — **never both `None`**.
///
/// A `PendingPoll` *is* its registration: the only thing that can complete one
/// is an event site walking a source's watcher list and finding this ring. A
/// poll holding no source is therefore a poll nothing in the machine can ever
/// complete, and the submitter learns nothing — it blocks until an unrelated
/// wake and its own recheck finds nothing. That is what this type removes:
/// [`Watched::of`] is the only constructor, it is the one place the emptiness
/// is decided, and past it no code path can push an unwakeable poll.
struct Watched {
    read: Option<Source>,
    write: Option<Source>,
}

impl Watched {
    /// The sources the requested directions name, or `None` when the object has
    /// no readiness to watch in either of them — a file, a namespace, a shared
    /// region, or a console asked only about writability. Its caller answers
    /// that with a CQE, because a poll is not something the kernel may accept
    /// and then never speak of again.
    fn of(read: Option<Source>, write: Option<Source>) -> Option<Self> {
        (read.is_some() || write.is_some()).then_some(Self { read, write })
    }

    fn iter(&self) -> impl Iterator<Item = &Source> {
        [&self.read, &self.write].into_iter().flatten()
    }

    fn is_ready(&self) -> bool {
        self.iter().any(Source::is_ready)
    }

    fn watches(&self, source: &Source) -> bool {
        self.iter().any(|s| s == source)
    }
}

struct PendingPoll {
    user_data: u64,
    /// The handle the poll was submitted against, and the dedup key. A handle
    /// is a slot in *this* process's table, so it is only ever compared with
    /// another poll on the same ring — which is the whole of what dedup needs.
    handle: RawHandle,
    flags: PollFlags,
    sources: Watched,
}

impl PendingPoll {
    fn watches(&self, source: &Source) -> bool {
        self.sources.watches(source)
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
    for source in poll.sources.iter() {
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

    /// The address of one completion entry. A pointer and not a `&mut`: this
    /// takes `&self`, and a `&mut` minted from a shared borrow is one two
    /// callers could hold at once over a page the process also maps.
    fn cqe_at(&self, index: u32) -> *mut IoUringCqe {
        let ptr = self.shm_phys.as_mut_ptr::<u8>();
        unsafe { ptr.add(CQ_RING_OFF as usize + 16 + index as usize * core::mem::size_of::<IoUringCqe>()) as *mut IoUringCqe }
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
        // One write of the whole entry, before the tail below publishes it.
        // SAFETY: `idx` is masked to the ring's size, and the whole instance is
        // touched under the `IO_URINGS` lock, so nothing else is writing here.
        unsafe { self.cqe_at(idx).write(IoUringCqe { user_data, result, flags }) };
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

/// Register a `POLL_ADD`, or answer it.
///
/// **A submission has an error channel, and it is the CQE.** Every way this can
/// refuse posts one, because the alternative is what this call used to do: a
/// `PendingPoll` carrying no source, which no event site can reach and no
/// recheck can complete, so the submitter went quiet instead of learning it had
/// made a mistake.
///
/// The handle is resolved by [`super::object::HandleError`]'s own rule and not
/// by one invented here (`kernel/src/object/handle.rs`): a handle
/// the process does not hold, one it closed, or one of the wrong type ends it,
/// and a right it does not carry is a word it may see. The three fatal kinds
/// are refused *outside* the table's guard, which is what `refuse_as_error`
/// requires — it does not come back.
fn process_poll_add(ring_id: RingId, sqe: &IoUringSqe) {
    let handle = sqe.fd;
    let flags = PollFlags::from_raw(sqe.op_flags);
    let user_data = sqe.user_data;

    // Readiness first, on the process's table rather than the thread's: a ring
    // is process-wide.
    let resolved = process::with_fd_owner_data(|data| {
        let object = data.handles.get_ref(handle, Rights::WAIT)?;
        let readable = flags.readable() && ops::has_data(object);
        let writable = flags.writable() && ops::has_space(object);
        let rsrc = if flags.readable() { ops::read_source(object) } else { None };
        let wsrc = if flags.writable() { ops::write_source(object) } else { None };
        Ok::<_, crate::object::HandleError>((readable || writable, rsrc, wsrc))
    });
    let (ready, read_source, write_source) = match resolved {
        Ok(seen) => seen,
        // Nothing is held here: `with_fd_owner_data` has given the guard up.
        Err(e) => {
            let refusal = e.refuse_as_error();
            post_cqe_locked(ring_id, user_data, -(refusal as i32), 0);
            return;
        }
    };

    if ready {
        // Already ready — post CQE immediately (one-shot: consumed)
        let mut result_flags = 0u32;
        if flags.readable() { result_flags |= PollFlags::IN.raw(); }
        if flags.writable() { result_flags |= PollFlags::OUT.raw(); }
        post_cqe_locked(ring_id, user_data, result_flags as i32, 0);
        return;
    }

    // Not ready, and not watchable either: the object has no readiness in the
    // directions asked for, so there is no registration to make and nothing
    // would ever complete this poll. `Poller::wait` treats a negative result as
    // "this registration is over, look at the handle again", which is the
    // honest answer for a file — always ready — and for a region, a namespace
    // or a ring, which are never ready at all.
    let Some(sources) = Watched::of(read_source, write_source) else {
        post_cqe_locked(ring_id, user_data, -(SyscallError::NotSupported as i32), 0);
        return;
    };

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

        // The cap is answered before anything is registered. Registering first
        // left the ring on every one of this poll's watcher lists with no poll
        // behind it, so a later event scanned a ring that had told the caller
        // it was full.
        if instance.pending_polls.len() >= MAX_PENDING_POLLS {
            instance.post_cqe(user_data, -(SyscallError::ResourceExhausted as i32), 0);
            let watch = instance.watch.clone();
            drop(guard);
            completion::post(completion::Subject::of(&watch), completion::Outcome::Ready);
            return;
        }

        for src in sources.iter() {
            src.add_watcher(ring_id);
        }
        instance.pending_polls.push(PendingPoll { user_data, handle, flags, sources });

        // Recheck: close TOCTOU window between readiness check and PendingPoll
        // insertion. A concurrent wake (complete_pending_for_event) either already
        // ran and found no PendingPoll (recheck catches the data it left behind),
        // or is blocked on IO_URINGS and will find the PendingPoll after we release.
        let became_ready =
            instance.pending_polls.last().expect("the poll just pushed").sources.is_ready();
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

/// The same rule as `SYS_ACCEPT`, which this is the submission form of.
///
/// It used to fold five refusals into one `-InvalidArgument` CQE, so a program
/// that submitted an `ACCEPT` on a handle it had closed learned only that its
/// argument was "nonsense" — where the syscall form of the same mistake ends
/// the process. `get` answers `WrongType` for a pipe presented as an acceptor,
/// which is why the type is asked of it rather than matched here.
fn process_accept(ring_id: RingId, sqe: &IoUringSqe) {
    let user_data = sqe.user_data;

    let acceptor = process::with_fd_owner_data(|data| {
        data.handles.get::<crate::object::port::Acceptor>(sqe.fd, Rights::READ)
    });

    let acceptor = match acceptor {
        Ok(a) => a,
        // Nothing held: `with_fd_owner_data` has given the guard up.
        Err(e) => {
            let refusal = e.refuse_as_error();
            post_cqe_locked(ring_id, user_data, -(refusal as i32), 0);
            return;
        }
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
pub fn remove_fd(sources: &[Option<EndedSource>]) {
    let mut affected: Vec<RingId> = Vec::new();
    for EndedSource(source) in sources.iter().flatten() {
        for &id in source.watchers().iter() {
            if !affected.contains(&id) {
                affected.push(id);
            }
        }
    }

    if affected.is_empty() { return; }

    let watches_a_closing_source =
        |pp: &PendingPoll| sources.iter().flatten().any(|EndedSource(s)| pp.watches(s));

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

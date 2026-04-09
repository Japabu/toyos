use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{Interest, Token};
use toyos_abi::Fd;
use toyos_abi::io_uring::*;

static NEXT_SELECTOR_ID: AtomicUsize = AtomicUsize::new(1);

/// An io_uring ring mapped into userspace.
struct Ring {
    fd: Fd,
    base: *mut u8,
    sq_size: u32,
    cq_size: u32,
}

// Ring is Send+Sync because:
// - fd is an opaque handle
// - base points to process-local shared memory, accessed under Mutex
unsafe impl Send for Ring {}
unsafe impl Sync for Ring {}

impl Ring {
    fn new(depth: u32) -> io::Result<Self> {
        let (fd, shm_token) = toyos_abi::syscall::io_uring_setup(depth)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("{e}")))?;
        let base = unsafe { toyos_abi::syscall::map_shared(shm_token) };
        // Read params from offset 0
        let params = unsafe { &*(base as *const IoUringParams) };
        Ok(Self {
            fd,
            base,
            sq_size: params.sq_ring_size,
            cq_size: params.cq_ring_size,
        })
    }

    fn sq_header(&self) -> &IoUringRingHeader {
        unsafe { &*(self.base.add(SQ_RING_OFF as usize) as *const IoUringRingHeader) }
    }

    fn cq_header(&self) -> &IoUringRingHeader {
        unsafe { &*(self.base.add(CQ_RING_OFF as usize) as *const IoUringRingHeader) }
    }

    fn sqe_at_mut(&self, index: u32) -> &mut IoUringSqe {
        unsafe {
            &mut *(self.base.add(SQES_OFF as usize + index as usize * core::mem::size_of::<IoUringSqe>()) as *mut IoUringSqe)
        }
    }

    fn cqe_at(&self, index: u32) -> &IoUringCqe {
        unsafe {
            &*(self.base.add(CQ_RING_OFF as usize + 16 + index as usize * core::mem::size_of::<IoUringCqe>()) as *const IoUringCqe)
        }
    }

    /// Submit a single SQE by writing it into the SQ ring.
    fn submit_sqe(&self, sqe: &IoUringSqe) {
        let sq = self.sq_header();
        let tail = sq.tail.load(Ordering::Acquire);
        let idx = tail & (self.sq_size - 1);
        *self.sqe_at_mut(idx) = *sqe;
        sq.tail.store(tail.wrapping_add(1), Ordering::Release);
    }

    /// Flush all pending SQEs and optionally wait for completions.
    fn enter(&self, to_submit: u32, min_complete: u32, timeout_nanos: u64) -> io::Result<u32> {
        toyos_abi::syscall::io_uring_enter(self.fd, to_submit, min_complete, timeout_nanos)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("{e}")))
    }

    /// Peek at the next CQE without consuming it.
    fn peek_cqe(&self) -> Option<IoUringCqe> {
        let cq = self.cq_header();
        let head = cq.head.load(Ordering::Acquire);
        let tail = cq.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let idx = head & (self.cq_size - 1);
        Some(*self.cqe_at(idx))
    }

    /// Advance the CQ head (consume one CQE).
    fn advance_cq(&self) {
        let cq = self.cq_header();
        let head = cq.head.load(Ordering::Acquire);
        cq.head.store(head.wrapping_add(1), Ordering::Release);
    }

    /// Count pending SQEs that haven't been submitted to the kernel yet.
    fn pending_sqes(&self) -> u32 {
        let sq = self.sq_header();
        let head = sq.head.load(Ordering::Acquire);
        let tail = sq.tail.load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        toyos_abi::syscall::close(self.fd);
    }
}

/// A registration that needs POLL_ADD re-armed after each event.
#[derive(Clone)]
struct Registration {
    fd: Fd,
    interest: Interest,
    token: Token,
}

#[derive(Debug)]
struct SelectorInner {
    registrations: Vec<(Fd, Interest, Token)>,
}

#[derive(Clone)]
pub struct Event {
    token: usize,
    flags: u8,
}

const FLAG_READABLE: u8 = 1;
const FLAG_WRITABLE: u8 = 2;

pub type Events = Vec<Event>;

#[derive(Debug)]
pub struct Selector {
    inner: Arc<Mutex<SelectorInner>>,
    ring: Arc<Ring>,
    id: usize,
}

// Ring is behind Arc<Mutex>, safe to debug-print the outer struct
impl std::fmt::Debug for Ring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ring").field("fd", &self.fd).finish()
    }
}

impl Selector {
    pub fn new() -> io::Result<Selector> {
        let ring = Ring::new(64)?;
        Ok(Selector {
            inner: Arc::new(Mutex::new(SelectorInner {
                registrations: Vec::new(),
            })),
            ring: Arc::new(ring),
            id: NEXT_SELECTOR_ID.fetch_add(1, Ordering::Relaxed),
        })
    }

    pub fn try_clone(&self) -> io::Result<Selector> {
        Ok(Selector {
            inner: Arc::clone(&self.inner),
            ring: Arc::clone(&self.ring),
            id: self.id,
        })
    }

    pub fn select(&self, events: &mut Events, timeout: Option<Duration>) -> io::Result<()> {
        events.clear();

        // Re-arm POLL_ADDs for all registrations
        let regs: Vec<(Fd, Interest, Token)> = {
            let inner = self.inner.lock().unwrap();
            inner.registrations.clone()
        };

        for &(fd, interest, token) in &regs {
            let mut sqe = IoUringSqe::default();
            sqe.op = IORING_OP_POLL_ADD;
            sqe.fd = fd.0;
            sqe.op_flags = interests_to_flags(interest);
            sqe.user_data = token.0 as u64;
            self.ring.submit_sqe(&sqe);
        }

        let pending = self.ring.pending_sqes();
        let timeout_nanos = match timeout {
            None => u64::MAX,
            Some(d) => d.as_nanos() as u64,
        };

        // Submit all pending SQEs and wait for at least 1 completion
        let min_complete = if regs.is_empty() { 0 } else { 1 };
        self.ring.enter(pending, min_complete, timeout_nanos)?;

        // Drain all CQEs
        while let Some(cqe) = self.ring.peek_cqe() {
            if cqe.result > 0 {
                let flags = poll_result_to_flags(cqe.result as u32);
                if let Some(existing) = events.iter_mut().find(|e| e.token == cqe.user_data as usize) {
                    existing.flags |= flags;
                } else {
                    events.push(Event {
                        token: cqe.user_data as usize,
                        flags,
                    });
                }
            }
            self.ring.advance_cq();
        }

        Ok(())
    }

    pub fn register_fd(&self, fd: Fd, token: Token, interest: Interest) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.registrations.push((fd, interest, token));
        Ok(())
    }

    pub fn reregister_fd(&self, fd: Fd, token: Token, interest: Interest) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(reg) = inner.registrations.iter_mut().find(|r| r.0 == fd) {
            reg.1 = interest;
            reg.2 = token;
        } else {
            inner.registrations.push((fd, interest, token));
        }
        Ok(())
    }

    pub fn deregister_fd(&self, fd: Fd) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.registrations.retain(|r| r.0 != fd);
        Ok(())
    }

    #[cfg(debug_assertions)]
    pub fn id(&self) -> usize {
        self.id
    }
}

fn interests_to_flags(interest: Interest) -> u32 {
    let mut flags = 0;
    if interest.is_readable() { flags |= IORING_POLL_IN; }
    if interest.is_writable() { flags |= IORING_POLL_OUT; }
    flags
}

fn poll_result_to_flags(result: u32) -> u8 {
    let mut flags = 0u8;
    if result & IORING_POLL_IN != 0 { flags |= FLAG_READABLE; }
    if result & IORING_POLL_OUT != 0 { flags |= FLAG_WRITABLE; }
    // If we got a result but no specific flags, assume readable
    if flags == 0 { flags = FLAG_READABLE; }
    flags
}

pub mod event {
    use super::*;
    use std::fmt;

    pub fn token(event: &Event) -> Token {
        Token(event.token)
    }

    pub fn is_readable(event: &Event) -> bool {
        event.flags & FLAG_READABLE != 0
    }

    pub fn is_writable(event: &Event) -> bool {
        event.flags & FLAG_WRITABLE != 0
    }

    pub fn is_error(_event: &Event) -> bool {
        false
    }

    pub fn is_read_closed(_event: &Event) -> bool {
        false
    }

    pub fn is_write_closed(_event: &Event) -> bool {
        false
    }

    pub fn is_priority(_event: &Event) -> bool {
        false
    }

    pub fn is_aio(_event: &Event) -> bool {
        false
    }

    pub fn is_lio(_event: &Event) -> bool {
        false
    }

    pub fn debug_details(f: &mut fmt::Formatter<'_>, event: &Event) -> fmt::Result {
        write!(
            f,
            "Event {{ token: {}, readable: {}, writable: {} }}",
            event.token,
            event.flags & FLAG_READABLE != 0,
            event.flags & FLAG_WRITABLE != 0
        )
    }
}

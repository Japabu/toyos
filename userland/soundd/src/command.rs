//! The ring the control thread hands the mix thread.
//!
//! Two threads and one direction: connects, disconnects and volume changes are
//! written by the control thread and drained by the mix thread at the top of
//! every cycle. Nothing travels back — a client's departure reaches the control
//! thread as a dead connection, not as a message.
//!
//! **A server never blocks on a client**, and this ring is where soundd could
//! have. It never drops a command (a ghost client: leaked shm, an app waiting on
//! a stream nothing mixes) and never asserts (soundd dead at a client's
//! choosing); [`submit`] waits instead, throttling the *control* thread, which
//! is the one that can afford it.

use core::sync::atomic::{AtomicU32, Ordering};

use toyos_abi::syscall;
use toyos_abi::RawHandle;
use toyos_mixer::Gain;

use crate::client::{ClientStream, Departure};
use crate::control::MAX_CONTROL_CLIENTS;

/// Deep enough that one pass of the control loop can never fill it: a pass
/// pushes at most one `AddClient` (there is one accept per wait) plus, per
/// connected client, one coalesced `SetVolume` and one `RemoveClient`.
const CMD_RING_SIZE: u32 = 256;
const _: () = assert!(CMD_RING_SIZE as usize >= 1 + 2 * MAX_CONTROL_CLIENTS);

pub(crate) enum MixCommand {
    AddClient(Box<ClientStream>),
    RemoveClient { client_id: usize, departure: Departure },
    SetVolume { client_id: usize, target: Gain },
}

pub(crate) struct CommandRing {
    slots: std::cell::UnsafeCell<[Option<MixCommand>; CMD_RING_SIZE as usize]>,
    write_idx: AtomicU32,
    read_idx: AtomicU32,
}

unsafe impl Send for CommandRing {}
unsafe impl Sync for CommandRing {}

impl CommandRing {
    pub(crate) fn new() -> Self {
        Self {
            slots: std::cell::UnsafeCell::new(std::array::from_fn(|_| None)),
            write_idx: AtomicU32::new(0),
            read_idx: AtomicU32::new(0),
        }
    }

    /// Hands the command back when the ring is full rather than dropping it (a
    /// ghost client: leaked shm, an app waiting on a stream nothing mixes) or
    /// asserting — a client chooses the load, since the control thread drains
    /// everything it has written before yielding. See `submit`, which waits.
    #[must_use]
    fn try_push(&self, cmd: MixCommand) -> Result<(), MixCommand> {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        if w.wrapping_sub(r) >= CMD_RING_SIZE {
            return Err(cmd);
        }
        let idx = (w % CMD_RING_SIZE) as usize;
        unsafe { (*self.slots.get())[idx] = Some(cmd); }
        self.write_idx.store(w.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    pub(crate) fn pop(&self) -> Option<MixCommand> {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        if w == r { return None; }
        let idx = (r % CMD_RING_SIZE) as usize;
        let cmd = unsafe { (*self.slots.get())[idx].take() };
        self.read_idx.store(r.wrapping_add(1), Ordering::Release);
        cmd
    }
}

/// Hand one command to the mix thread, waiting for room if the ring is full.
///
/// The mix thread drains the whole ring at the top of every cycle, so a full
/// ring means it has not run for a cycle and one device period is exactly how
/// long there is to wait. Throttling the control thread is the point: the
/// alternatives are dropping a command (a client stranded in the mix thread
/// forever) or asserting (soundd dead at a client's choosing). The retry is
/// unbounded because the mix thread is the process's main thread — if it has
/// stopped, soundd is already gone.
pub(crate) fn submit(cmd_ring: &CommandRing, cmd_pipe_write: RawHandle, cmd: MixCommand, period_nanos: u64) {
    let mut cmd = cmd;
    loop {
        let full = cmd_ring.try_push(cmd);
        let _ = syscall::write_nonblock(cmd_pipe_write, &[1]);
        match full {
            Ok(()) => return,
            Err(returned) => {
                cmd = returned;
                syscall::nanosleep(period_nanos);
            }
        }
    }
}

/// Tell the mix thread a stream ended, and how.
///
/// Every removal the control thread issues goes through here, so the witness it
/// holds — which of §7's four ways this stream ended — travels with the command
/// instead of being reconstructed from a flag on the other side.
pub(crate) fn remove(
    cmd_ring: &CommandRing,
    cmd_pipe_write: RawHandle,
    client_id: usize,
    departure: Departure,
    period_nanos: u64,
) {
    submit(
        cmd_ring,
        cmd_pipe_write,
        MixCommand::RemoveClient { client_id, departure },
        period_nanos,
    );
}

//! The device half of the mix loop, and the two devices behind it.
//!
//! Everything that differs between virtio-sound and HDA is a method below. The
//! mixer, the ramps, the DLL, the underrun accounting and the suspend/resume
//! structure are one body of code either way, which is what makes gate A one
//! instrument for both — and what [`Pipeline`] exists to keep honest, because
//! the two differ in *who owns a period soundd has not refilled* and nothing
//! else in the loop can be written without knowing which.

use toyos::AsHandle;
use toyos_abi::audio::AudioCompletionRecord;
use toyos_abi::syscall;
use toyos_abi::RawHandle;

use crate::{hda, virtio};

/// Who owns a period soundd has been given back and has not refilled.
///
/// The mix loop's free list was written for [`Pipeline::Queue`] throughout, and
/// three of its rules are that model showing: it holds a period back while a
/// client is mid-refill, it drains by not submitting, and it takes the lowest
/// free index first because the device plays what it is given in the order it
/// is given.
///
/// [`Pipeline::Ring`] breaks all three, which is what killed soundd on the T14.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pipeline {
    /// virtio-sound: a period soundd has not submitted is a period the device
    /// does not have. Holding one costs nothing, indefinitely, and the play
    /// order is the submit order.
    Queue,
    /// HDA: the engine owns every period for as long as it runs. It returns to
    /// buffer `i` exactly `num_buffers` periods after completing it and plays
    /// whatever is there, so a period soundd holds back is played as the
    /// silence `released` left in it *and* completed a second time — a
    /// completion for a buffer soundd still holds. The play order is the
    /// ring's, which is the lowest free index only while a batch does not wrap.
    Ring,
}

/// The device half of the mix loop.
///
/// Two implementations and no more: a framework before there are three is an
/// abstraction with no evidence behind it. Both are drivers in this process
/// now, and what differs between the two devices is exactly the methods below;
/// the mixer, the ramps, the DLL, the underrun accounting and the
/// suspend/resume structure are one body of code either way, which is what
/// makes gate A one instrument for both.
pub(crate) trait Backend {
    /// Who owns a freed period soundd does not refill.
    fn pipeline(&self) -> Pipeline;

    /// The handle a completion arrives on.
    fn handle(&self) -> RawHandle;

    /// Where period `idx`'s samples go. Device memory this process may write,
    /// mapped once at claim.
    fn buffer(&self, idx: usize) -> *mut u8;

    /// Completion records, oldest first, into `out`. `0` is nothing pending.
    fn completions(&mut self, out: &mut [AudioCompletionRecord]) -> usize;

    /// Period `idx` has played and is soundd's again.
    ///
    /// HDA's engine is cyclic and never stops on its own, so a period nobody
    /// refills is *replayed* — audible harm gate A's gap detector cannot see.
    /// Zeroing it as it frees makes a late soundd cost silence instead, which
    /// is exactly what virtio-sound's device does when it runs dry, so one
    /// instrument certifies both (§2.4). virtio's own implementation is empty
    /// for that reason and not by omission: nothing is published for a period
    /// that was not filled, so the device is never given it again.
    fn released(&mut self, idx: usize);

    /// Period `idx` holds `bytes` of PCM and is the device's to play.
    ///
    /// On a [`Pipeline::Ring`] that is a statement about the *contents*: the
    /// period was already the device's and this only says it now holds audio.
    fn submit(&mut self, idx: usize, bytes: usize);

    /// Stop the stream. Idempotent, and cheap when it is already stopped.
    fn stop(&mut self);
}

pub(crate) struct VirtioBackend {
    pub(crate) virtio: virtio::Virtio,
}

impl Backend for VirtioBackend {
    fn pipeline(&self) -> Pipeline {
        Pipeline::Queue
    }

    fn handle(&self) -> RawHandle {
        self.virtio.dev().as_handle()
    }

    fn buffer(&self, idx: usize) -> *mut u8 {
        self.virtio.buffer(idx)
    }

    fn completions(&mut self, out: &mut [AudioCompletionRecord]) -> usize {
        // Where the kernel used to service the event queue inside the same
        // syscall: the device's own view of an underrun, which this process's
        // counters cannot see.
        self.virtio.poll_events();
        self.virtio.completions(out)
    }

    fn released(&mut self, _idx: usize) {}

    fn submit(&mut self, idx: usize, bytes: usize) {
        self.virtio.submit(idx, bytes);
    }

    fn stop(&mut self) {
        self.virtio.stop();
    }
}

pub(crate) struct HdaBackend {
    pub(crate) hda: hda::Hda,
    pub(crate) buffers: Vec<*mut u8>,
    pub(crate) period_bytes: usize,
}

impl Backend for HdaBackend {
    fn pipeline(&self) -> Pipeline {
        Pipeline::Ring
    }

    fn handle(&self) -> RawHandle {
        self.hda.dev().as_handle()
    }

    fn buffer(&self, idx: usize) -> *mut u8 {
        self.buffers[idx]
    }

    fn completions(&mut self, out: &mut [AudioCompletionRecord]) -> usize {
        match self.hda.dev().completions() {
            Ok(record) => {
                out[0] = record;
                1
            }
            Err(syscall::SyscallError::WouldBlock) => 0,
            Err(e) => panic!("soundd: hda completions failed: {e:?}"),
        }
    }

    fn released(&mut self, idx: usize) {
        unsafe { core::ptr::write_bytes(self.buffers[idx], 0, self.period_bytes) };
    }

    fn submit(&mut self, _idx: usize, _bytes: usize) {
        self.hda.start();
    }

    fn stop(&mut self) {
        self.hda.stop();
    }
}

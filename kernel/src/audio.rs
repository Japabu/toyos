use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};
use crate::drivers::virtio_sound::SoundController;
use crate::io_uring::RingId;
use crate::sync::Lock;
use toyos_abi::audio::{AudioCompletionRecord, AudioInfo};

static AUDIO: Lock<Option<SoundController>> = Lock::new(None);
static AUDIO_INFO: Lock<Option<AudioInfo>> = Lock::new(None);
static IO_URING_WATCHERS: Lock<Vec<RingId>> = Lock::new(Vec::new());

// ---------------------------------------------------------------------------
// Completion record ring — ISR producer, syscall consumer
// ---------------------------------------------------------------------------

const RECORD_RING_CAP: u32 = 16;

/// SPSC ring of completion records. Producer is the MSI-X ISR (single CPU,
/// IF=0 — never concurrent with itself); consumer is whoever holds the
/// AUDIO lock in `drain_completed`. Indices are free-running u32s.
///
/// The ring can never fill: pending records carry pairwise-disjoint nonzero
/// masks (a buffer's bit re-enters a record only after `recycle`, which runs
/// at pop time), so occupancy is bounded by the DMA buffer count — enforced
/// by the const assert below. Overflow is a kernel bookkeeping bug, and the
/// producer panics rather than mutating slots the consumer may be reading.
struct RecordRing {
    slots: [UnsafeCell<AudioCompletionRecord>; RECORD_RING_CAP as usize],
    head: AtomicU32,
    tail: AtomicU32,
}

const _: () = assert!(
    RECORD_RING_CAP as usize >= crate::drivers::virtio_sound::TX_INFLIGHT_MAX,
    "record ring must hold one record per DMA buffer"
);

// SAFETY: slot access is arbitrated by the head/tail protocol above.
unsafe impl Sync for RecordRing {}

static RECORDS: RecordRing = RecordRing {
    slots: [const {
        UnsafeCell::new(AudioCompletionRecord { mask: 0, _pad: 0, timestamp_nanos: 0 })
    }; RECORD_RING_CAP as usize],
    head: AtomicU32::new(0),
    tail: AtomicU32::new(0),
};

/// Push one completion record from the MSI-X ISR. Lock-free, no allocation.
pub fn isr_push_completion(mask: u32, timestamp_nanos: u64) {
    let ring = &RECORDS;
    let head = ring.head.load(Ordering::Relaxed); // sole writer of head
    let tail = ring.tail.load(Ordering::Acquire);
    assert!(
        head.wrapping_sub(tail) < RECORD_RING_CAP,
        "audio: completion record ring overflow"
    );
    let slot = (head % RECORD_RING_CAP) as usize;
    // SAFETY: slot is outside [tail, head) — not visible to the consumer.
    unsafe {
        *ring.slots[slot].get() = AudioCompletionRecord { mask, _pad: 0, timestamp_nanos };
    }
    // Release: publish the record contents before the consumer can
    // observe the new head.
    ring.head.store(head.wrapping_add(1), Ordering::Release);
}

/// Pop the oldest pending record. Called under the AUDIO lock, which
/// serializes consumers — the tail store needs no CAS.
fn pop_completion() -> Option<AudioCompletionRecord> {
    let ring = &RECORDS;
    let tail = ring.tail.load(Ordering::Relaxed); // sole writer of tail
    // Acquire pairs with the producer's Release head store: the record
    // contents are visible before head covers them.
    let head = ring.head.load(Ordering::Acquire);
    if head == tail {
        return None;
    }
    let rec = unsafe { *ring.slots[(tail % RECORD_RING_CAP) as usize].get() };
    ring.tail.store(tail.wrapping_add(1), Ordering::Release);
    Some(rec)
}

/// Readiness: are completion records pending? Lock-free — used by fd
/// readiness checks, io_uring poll, and the scheduler's park-time recheck.
pub fn has_pending() -> bool {
    RECORDS.head.load(Ordering::Acquire) != RECORDS.tail.load(Ordering::Acquire)
}

// ---------------------------------------------------------------------------
// Controller access
// ---------------------------------------------------------------------------

pub fn register(controller: SoundController, info: AudioInfo) {
    *AUDIO.lock() = Some(controller);
    *AUDIO_INFO.lock() = Some(info);
}

pub fn audio_info() -> Option<AudioInfo> {
    *AUDIO_INFO.lock()
}

pub fn start() {
    if let Some(ctrl) = AUDIO.lock().as_mut() {
        ctrl.start();
    }
}

pub fn stop() {
    if let Some(ctrl) = AUDIO.lock().as_mut() {
        ctrl.stop();
    }
}

/// Submit a filled DMA buffer to the VirtIO device.
pub fn submit_buffer(idx: usize, len: u32) -> bool {
    if let Some(ctrl) = AUDIO.lock().as_mut() {
        ctrl.submit_buffer(idx, len)
    } else {
        false
    }
}

/// Copy up to `buf.len() / 16` pending completion records into `buf`
/// (oldest first) and recycle their descriptors. Returns bytes written;
/// 0 means nothing pending.
pub fn drain_completed(buf: &mut [u8]) -> usize {
    let max = buf.len() / AudioCompletionRecord::SIZE;
    let mut guard = AUDIO.lock();
    // The audio fd only exists once the device was claimed, which requires
    // a registered controller — absence is a kernel bug.
    let ctrl = guard.as_mut().expect("audio: drain without controller");
    ctrl.poll_events();
    let mut written = 0;
    for _ in 0..max {
        let Some(rec) = pop_completion() else { break };
        ctrl.recycle(rec.mask);
        // Field-wise serialization — never expose struct padding.
        buf[written..written + 4].copy_from_slice(&rec.mask.to_le_bytes());
        buf[written + 4..written + 8].copy_from_slice(&0u32.to_le_bytes());
        buf[written + 8..written + 16].copy_from_slice(&rec.timestamp_nanos.to_le_bytes());
        written += AudioCompletionRecord::SIZE;
    }
    written
}

pub fn add_io_uring_watcher(id: RingId) {
    let mut w = IO_URING_WATCHERS.lock();
    if !w.contains(&id) { w.push(id); }
}

pub fn remove_io_uring_watcher(id: RingId) {
    IO_URING_WATCHERS.lock().retain(|&x| x != id);
}

pub fn io_uring_watchers() -> Vec<RingId> {
    IO_URING_WATCHERS.lock().clone()
}

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use crate::drivers::virtio_sound::SoundController;
use crate::io_uring::RingId;
use crate::sync::Lock;
use toyos_abi::audio::AudioInfo;

static AUDIO: Lock<Option<SoundController>> = Lock::new(None);
static AUDIO_INFO: Lock<Option<AudioInfo>> = Lock::new(None);
static IO_URING_WATCHERS: Lock<Vec<RingId>> = Lock::new(Vec::new());
static COMPLETION_TS: AtomicU64 = AtomicU64::new(0);

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

pub fn set_completion_timestamp(ts: u64) {
    COMPLETION_TS.store(ts, Ordering::Release);
}

/// Drain completed TX buffers. Returns `Some((bitmask, timestamp))` if any completed,
/// `None` if nothing new.
pub fn drain_completed() -> Option<(u32, u64)> {
    let mask = if let Some(ctrl) = AUDIO.lock().as_mut() {
        ctrl.poll_completed()
    } else {
        0
    };
    if mask != 0 {
        let ts = COMPLETION_TS.swap(0, Ordering::Acquire);
        Some((mask, ts))
    } else {
        None
    }
}

/// Non-consuming readiness check — returns true if the IRQ has fired,
/// meaning completions are likely available.
pub fn has_pending() -> bool {
    if let Some(ctrl) = AUDIO.lock().as_ref() {
        ctrl.has_used()
    } else {
        false
    }
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

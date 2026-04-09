use alloc::vec::Vec;
use crate::drivers::virtio_sound::SoundController;
use crate::io_uring::RingId;
use crate::sync::Lock;
use toyos_abi::audio::AudioInfo;

static AUDIO: Lock<Option<SoundController>> = Lock::new(None);
static AUDIO_INFO: Lock<Option<AudioInfo>> = Lock::new(None);
static IO_URING_WATCHERS: Lock<Vec<RingId>> = Lock::new(Vec::new());

pub fn register(controller: SoundController, info: AudioInfo) {
    *AUDIO.lock() = Some(controller);
    *AUDIO_INFO.lock() = Some(info);
}

pub fn audio_info() -> Option<AudioInfo> {
    *AUDIO_INFO.lock()
}

/// Submit a filled DMA buffer to the VirtIO device.
pub fn submit_buffer(idx: usize, len: u32) -> bool {
    if let Some(ctrl) = AUDIO.lock().as_mut() {
        ctrl.submit_buffer(idx, len)
    } else {
        false
    }
}

/// Drain completed TX buffers. Returns `Some(bitmask)` if any completed,
/// `None` if nothing new.
pub fn drain_completed() -> Option<u32> {
    if let Some(ctrl) = AUDIO.lock().as_mut() {
        let mask = ctrl.poll_completed();
        if mask != 0 { Some(mask) } else { None }
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

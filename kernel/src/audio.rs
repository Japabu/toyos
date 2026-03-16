use crate::drivers::virtio_sound::SoundController;
use crate::sync::Lock;
use toyos_abi::audio::AudioInfo;

static AUDIO: Lock<Option<SoundController>> = Lock::new(None);
static AUDIO_INFO: Lock<Option<AudioInfo>> = Lock::new(None);

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

/// Poll for completed buffers. Returns bitmask of free buffer indices.
pub fn poll_completed() -> u32 {
    if let Some(ctrl) = AUDIO.lock().as_mut() {
        ctrl.poll_completed()
    } else {
        0
    }
}

use crate::drivers::virtio_sound::SoundController;
use crate::sync::Lock;

static AUDIO: Lock<Option<SoundController>> = Lock::new(None);

pub fn register(controller: SoundController) {
    *AUDIO.lock() = Some(controller);
}

pub fn write_samples(data: &[u8]) {
    if let Some(ctrl) = AUDIO.lock().as_mut() {
        // Write in chunks that fit a single DMA page (~4KB minus header)
        let chunk_size = 4080; // 4096 - 16 bytes for xfer header + alignment
        for chunk in data.chunks(chunk_size) {
            ctrl.write_samples(chunk);
        }
    }
}

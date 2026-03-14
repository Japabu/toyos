//! Audio output.

use crate::syscall;

/// Write PCM audio samples (s16le stereo 44100Hz) to the sound device.
pub fn write_samples(samples: &[u8]) {
    syscall::audio_write(samples);
}

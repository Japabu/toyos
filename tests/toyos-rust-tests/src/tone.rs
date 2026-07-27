//! Shared tone playback for the audio glitch tests (audio_tone,
//! audio_tone_load). The host-side harness records what the virtio-sound
//! device plays into a wav and asserts the tone contains no mid-signal
//! silence (underruns) and no hard discontinuities (clicks).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const FREQ_HZ: f64 = 440.0;
const DURATION_SECS: f64 = 3.0;
/// Loud enough that any dropout is unambiguous in the capture, with headroom
/// so mixing can never clip.
const AMPLITUDE: f64 = 16000.0;

/// Play a deterministic 440Hz sine and block until it has drained.
///
/// The sample value is derived from the absolute sample index, not from
/// per-callback state, so the generated signal is identical regardless of
/// how the callback invocations are sized or timed.
pub fn play_tone() {
    let host = cpal::default_host();
    let device = host.default_output_device().expect("no audio output device");
    let config = device.default_output_config().expect("no audio config");
    let sample_rate = config.sample_rate() as f64;
    let channels = config.channels() as usize;
    let total_samples = (sample_rate * DURATION_SECS) as u64;

    let done = Arc::new(AtomicBool::new(false));
    let done2 = done.clone();
    let position = Arc::new(AtomicU64::new(0));

    let stream = device
        .build_output_stream(
            config.into(),
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                let mut n = position.load(Ordering::Relaxed);
                for frame in data.chunks_exact_mut(channels) {
                    let value = if n < total_samples {
                        let phase = 2.0 * std::f64::consts::PI * FREQ_HZ * n as f64 / sample_rate;
                        (AMPLITUDE * phase.sin()) as i16
                    } else {
                        0
                    };
                    frame.fill(value);
                    n += 1;
                }
                position.store(n, Ordering::Relaxed);
                if n >= total_samples {
                    done2.store(true, Ordering::Relaxed);
                }
            },
            |err| eprintln!("audio error: {err}"),
            None,
        )
        .expect("failed to build audio stream");

    stream.play().expect("failed to play");

    while !done.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Let the tail of the tone drain through soundd and the device.
    std::thread::sleep(std::time::Duration::from_millis(200));
}

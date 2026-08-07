//! A client that stops producing mid-stream, on a machine whose audio device
//! is a cyclic DMA ring.
//!
//! The stall is the whole actuator. soundd's mix loop may leave a freed period
//! unfilled while a streaming client is still producing it (§5.10), and on
//! virtio-sound that costs nothing: a period soundd has not submitted is a
//! period the device does not have. HDA's engine owns every period for as long
//! as it runs and replays the ones nobody refilled, so a period held across a
//! lap is completed a second time — which is what killed soundd on the T14.
//!
//! Nothing in the ordinary tone clients reaches that state: they keep their
//! ring full, so soundd never defers at all (`deferred=0` on every `hda_tone`
//! run measured). This one empties it on purpose, for longer than the ring
//! takes to come round.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const FREQ_HZ: f64 = 440.0;
const AMPLITUDE: f64 = 16000.0;

/// Periods of tone between stalls, and how long each stall lasts.
///
/// The stall has to outlast one lap of the device ring — 8 periods, 23.2 ms —
/// or the engine never reaches a period soundd is still holding. 60 ms is two
/// and a half laps, and the run stages eight of them so the test does not rest
/// on catching one window.
const STALL: Duration = Duration::from_millis(60);
const STALLS: u32 = 8;
const CALLBACKS_BETWEEN_STALLS: u64 = 60;

fn main() {
    let host = cpal::default_host();
    let device = host.default_output_device().expect("no audio output device");
    let config = device.default_output_config().expect("no audio config");
    let sample_rate = config.sample_rate() as f64;
    let channels = config.channels() as usize;

    let stalls = Arc::new(AtomicU64::new(0));
    let stalls_cb = stalls.clone();
    let mut n: u64 = 0;
    let mut callbacks: u64 = 0;

    let stream = device
        .build_output_stream(
            config.into(),
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_exact_mut(channels) {
                    let phase = 2.0 * std::f64::consts::PI * FREQ_HZ * n as f64 / sample_rate;
                    frame.fill((AMPLITUDE * phase.sin()) as i16);
                    n += 1;
                }
                callbacks += 1;
                if callbacks % CALLBACKS_BETWEEN_STALLS == 0
                    && stalls_cb.load(Ordering::Relaxed) < STALLS as u64
                {
                    stalls_cb.fetch_add(1, Ordering::Relaxed);
                    std::thread::sleep(STALL);
                }
            },
            |err| eprintln!("audio error: {err}"),
            None,
        )
        .expect("failed to build audio stream");

    stream.play().expect("failed to play");
    while stalls.load(Ordering::Relaxed) < STALLS as u64 {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(200));
    drop(stream);
    println!("stalled {} times, soundd survived", stalls.load(Ordering::Relaxed));
}

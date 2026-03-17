use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub fn main(args: Vec<String>) {
    let freq: f32 = args.first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(440.0);

    let duration_secs: f32 = args.get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2.0);

    eprintln!("tone: {freq}Hz for {duration_secs}s");

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host.default_output_device().expect("no audio output device");
    let config = device.default_output_config().expect("no audio config");
    let sample_rate = config.sample_rate() as f32;
    let channels = config.channels() as usize;

    let phase = Arc::new(std::sync::Mutex::new(0.0f32));
    let done = Arc::new(AtomicBool::new(false));
    let done2 = done.clone();
    let total_samples = (sample_rate * duration_secs) as u64;
    let samples_written = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let samples_written2 = samples_written.clone();

    let stream = device
        .build_output_stream(
            config.into(),
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                let mut ph = phase.lock().unwrap();
                let increment = freq * 2.0 * std::f32::consts::PI / sample_rate;
                let frames = data.len() / channels;
                let mut count = samples_written2.load(Ordering::Relaxed);

                for frame in 0..frames {
                    if count >= total_samples {
                        for ch in 0..channels {
                            data[frame * channels + ch] = 0;
                        }
                    } else {
                        let sample = (*ph).sin();
                        let value = (sample * 16000.0) as i16;
                        for ch in 0..channels {
                            data[frame * channels + ch] = value;
                        }
                        *ph += increment;
                        count += 1;
                    }
                }

                samples_written2.store(count, Ordering::Relaxed);
                if count >= total_samples {
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

    // Let the last buffer drain
    std::thread::sleep(std::time::Duration::from_millis(100));
    eprintln!("tone: done");
}

use std::sync::{Arc, Condvar, Mutex};

/// The amplitude a `tone` with no volume argument plays at, and the level gate
/// A's peak window is centred on — it is not full scale, so asking for 1.0 is
/// louder than the default rather than equal to it.
const REFERENCE_AMPLITUDE: f32 = 16000.0;

pub fn main(args: Vec<String>) {
    let freq: f32 = args.first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(440.0);

    let duration_secs: f32 = args.get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2.0);

    let amplitude = match args.get(2) {
        None => REFERENCE_AMPLITUDE,
        Some(arg) => match arg.parse::<f32>() {
            Ok(v) if (0.0..=1.0).contains(&v) => v * f32::from(i16::MAX),
            _ => {
                eprintln!("tone: volume must be 0.0 to 1.0, got {arg:?}");
                std::process::exit(1);
            }
        },
    };

    eprintln!(
        "tone: {freq}Hz for {duration_secs}s at amplitude {}",
        amplitude as i32
    );

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host.default_output_device().expect("no audio output device");
    let config = device.default_output_config().expect("no audio config");
    let sample_rate = config.sample_rate() as f32;
    let channels = config.channels() as usize;

    let total_frames = (sample_rate * duration_secs) as u64;
    let done = Arc::new((Mutex::new(false), Condvar::new()));
    let done_cb = done.clone();
    let done_err = done.clone();

    let mut frames_written = 0u64;

    // The angle is the sample index times the increment, never a running sum:
    // a phase that accumulates has no bound, so each step is eventually rounded
    // against a total far larger than itself, and the tone the ear judges the
    // machine by picks up both a flat pitch and a noise floor of its own.
    let radians_per_frame = f64::from(freq) * 2.0 * std::f64::consts::PI / f64::from(sample_rate);
    let amplitude = f64::from(amplitude);

    let stream = device
        .build_output_stream(
            config.into(),
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                let frames = data.len() / channels;

                for frame in 0..frames {
                    if frames_written >= total_frames {
                        for ch in 0..channels {
                            data[frame * channels + ch] = 0;
                        }
                    } else {
                        let angle = frames_written as f64 * radians_per_frame;
                        let value = (angle.sin() * amplitude) as i16;
                        for ch in 0..channels {
                            data[frame * channels + ch] = value;
                        }
                        frames_written += 1;
                    }
                }

                if frames_written >= total_frames {
                    let (flag, cvar) = &*done_cb;
                    *flag.lock().unwrap() = true;
                    cvar.notify_one();
                }
            },
            move |err| {
                eprintln!("audio error: {err}");
                let (flag, cvar) = &*done_err;
                *flag.lock().unwrap() = true;
                cvar.notify_one();
            },
            None,
        )
        .expect("failed to build audio stream");

    stream.play().expect("failed to play");

    let (flag, cvar) = &*done;
    let mut finished = flag.lock().unwrap();
    while !*finished {
        finished = cvar.wait(finished).unwrap();
    }
    drop(finished);

    // Let the last buffer drain
    std::thread::sleep(std::time::Duration::from_millis(100));
    eprintln!("tone: done");
}

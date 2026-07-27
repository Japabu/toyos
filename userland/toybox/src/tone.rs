use std::sync::{Arc, Condvar, Mutex};

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

    let total_frames = (sample_rate * duration_secs) as u64;
    let done = Arc::new((Mutex::new(false), Condvar::new()));
    let done_cb = done.clone();
    let done_err = done.clone();

    let mut phase = 0.0f32;
    let mut frames_written = 0u64;

    let stream = device
        .build_output_stream(
            config.into(),
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                let increment = freq * 2.0 * std::f32::consts::PI / sample_rate;
                let frames = data.len() / channels;

                for frame in 0..frames {
                    if frames_written >= total_frames {
                        for ch in 0..channels {
                            data[frame * channels + ch] = 0;
                        }
                    } else {
                        let value = (phase.sin() * 16000.0) as i16;
                        for ch in 0..channels {
                            data[frame * channels + ch] = value;
                        }
                        phase += increment;
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

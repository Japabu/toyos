//! Audio glitch regression test, load variant: play the tone while two pure
//! busy-spin processes saturate the (single) CPU. Glitch-free playback under
//! load is what the scheduler's audio priority handling must guarantee.

#[path = "../tone.rs"]
mod tone;

use std::process::Command;
use std::time::{Duration, Instant};

/// Outlives tone startup + 3s playback + drain even under heavy contention.
const BURN_SECS: u64 = 6;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("burn") {
        burn(Duration::from_secs(BURN_SECS));
        return;
    }

    let burners: Vec<_> = (0..2)
        .map(|_| {
            Command::new("/bin/test_rs_audio_tone_load")
                .arg("burn")
                .spawn()
                .expect("spawn burner")
        })
        .collect();

    tone::play_tone();
    println!("tone done");

    for mut burner in burners {
        burner.wait().expect("wait burner");
    }
}

fn burn(duration: Duration) {
    let start = Instant::now();
    let mut i = 0u64;
    loop {
        i = i.wrapping_add(1);
        // Check the clock rarely so the load stays pure CPU, not syscalls.
        if i % (1 << 22) == 0 && start.elapsed() >= duration {
            return;
        }
    }
}

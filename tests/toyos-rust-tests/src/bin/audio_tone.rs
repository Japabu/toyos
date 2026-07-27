//! Audio glitch regression test, idle variant: play a deterministic 440Hz
//! sine on an otherwise idle system. The host harness asserts the wav the
//! virtio-sound device captured is glitch-free.

#[path = "../tone.rs"]
mod tone;

fn main() {
    tone::play_tone();
    println!("tone done");
}

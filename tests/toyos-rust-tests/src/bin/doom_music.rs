//! Runs doom's music actuator and reports whether the process lived.
//!
//! The playing is `sound::music_check` in `userland/doom`: it opens the shipped
//! SoundFont, converts one of the WAD's own MUS lumps and pushes the result at
//! the audio device. Only that binary can reach any of it. This side starts it
//! and answers the question a serial log cannot — whether it exited or died —
//! while the verdict on the sound is the host's capture of the device.

use std::process::Command;

fn main() {
    let mut child = Command::new("/bin/doom")
        .arg("--music-check")
        .spawn()
        .expect("spawn /bin/doom --music-check");
    let status = child.wait().expect("wait for /bin/doom");
    assert!(status.success(), "doom could not play its own music: {status:?}");
    println!("doom played its music");
}

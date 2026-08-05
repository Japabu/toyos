//! Runs doom's stalled-consumer actuator and reports whether the game lived.
//!
//! The flood itself is `sound::sound_stress` in `userland/doom`: the producer
//! is the game thread inside its own sound module, so nothing out here can
//! drive it. This side only starts it and answers the one question the host
//! cannot ask from a serial log alone — whether the process exited or died.

use std::process::Command;

fn main() {
    let mut child = Command::new("/bin/doom")
        .arg("--sound-stress")
        .spawn()
        .expect("spawn /bin/doom --sound-stress");
    let status = child.wait().expect("wait for /bin/doom");
    assert!(
        status.success(),
        "doom did not survive its own sound producer: {status:?}"
    );
    println!("doom survived the sound flood");
}

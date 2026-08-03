//! Claims the keyboard fd and prints what arrives, one line per event.
//!
//! Driven by the `i8042_keyboard` and `i8042_no_spurious_wake` host tests,
//! which boot a guest with no USB HID at all and inject through QMP once the
//! ready line appears. Not a standalone test: on its own it would time out
//! with nothing to report, which is why it is in RUST_SKIP.
//!
//! It holds a [`Translator`] because the kernel no longer does: the fd carries
//! a HID usage and a modifier mask, and what those type is a layout, which is
//! userland's. This is the same type and the same call `/bin/console` and
//! every window client make, so `tr=` below is what a real surface would put
//! on a real shell's stdin.

use std::time::{Duration, Instant};
use toyos::device::Keyboard;
use toyos_abi::input::RawKeyEvent;

const EVENT_SIZE: usize = std::mem::size_of::<RawKeyEvent>();

fn main() {
    let keyboard = Keyboard::open().expect("i8042_keyboard: no keyboard device");
    let mut translator = window::configured_translator();
    println!("===I8042_READY===");

    // Long enough for the host to inject and for the events to land; short
    // enough that a driver that delivers nothing fails rather than hangs.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 512];
    let mut seen = 0;
    while Instant::now() < deadline {
        let n = keyboard.read_nonblock(&mut buf).unwrap_or(0);
        if n == 0 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        for chunk in buf[..n].chunks_exact(EVENT_SIZE) {
            let key = window::KeyEvent { keycode: chunk[0], modifiers: chunk[1] };
            let translated = if key.pressed() {
                translator.press(key.keycode, key.mods())
            } else {
                window::Emit::EMPTY
            };
            println!(
                "kev usage=0x{:02x} mods=0x{:02x} tr={:?}",
                key.keycode,
                key.modifiers,
                translated.as_str()
            );
            seen += 1;
        }
    }
    println!("kev done seen={seen}");
}

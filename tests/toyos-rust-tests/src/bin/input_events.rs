//! Claims both input devices and prints every event either one produces.
//!
//! Driven by the `metal_sim_input` host test, which boots the machine shape
//! that gets flashed — no virtio device, no USB HID, a plain kernel — and
//! injects through QMP once the ready line appears. The line formats are the
//! ones `i8042_keyboard` and `i8042_mouse` already print, so the host parses
//! them with the same two functions. Not a standalone test: on its own it
//! would report nothing, which is why it is in RUST_SKIP.

use std::time::{Duration, Instant};
use toyos::device::{Keyboard, Mouse};
use toyos_abi::input::RawKeyEvent;

const KEY_SIZE: usize = std::mem::size_of::<RawKeyEvent>();
const MOUSE_SIZE: usize = 6;

fn main() {
    let keyboard = Keyboard::open().expect("input_events: no keyboard device");
    let mouse = Mouse::open().expect("input_events: no mouse device");
    let mut translator = window::configured_translator();
    println!("===INPUT_READY===");

    // Long enough for the host to inject both halves and for the events to
    // land; short enough that a path that delivers nothing fails rather than
    // hangs.
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut buf = [0u8; 1024];
    let (mut keys, mut pointer) = (0, 0);
    while Instant::now() < deadline {
        let mut idle = true;

        let n = keyboard.read_nonblock(&mut buf).unwrap_or(0);
        for chunk in buf[..n].chunks_exact(KEY_SIZE) {
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
            keys += 1;
            idle = false;
        }

        let n = mouse.read_nonblock(&mut buf).unwrap_or(0);
        for chunk in buf[..n].chunks_exact(MOUSE_SIZE) {
            println!(
                "mev buttons=0x{:02x} x={} y={}",
                chunk[0],
                u16::from_le_bytes([chunk[2], chunk[3]]),
                u16::from_le_bytes([chunk[4], chunk[5]]),
            );
            pointer += 1;
            idle = false;
        }

        if idle {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    println!("input done keys={keys} pointer={pointer}");
}

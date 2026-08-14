//! Claims both input devices and prints every event either one produces.
//!
//! Driven by `metal_sim_input` and `xhci_second_controller`, which inject
//! through QMP one step at a time and wait for these lines between steps — so
//! the host never has more in flight than the guest has taken. The line formats
//! are the ones `i8042_keyboard` and `i8042_mouse` already print, so the host
//! parses them with the same two functions. Not a standalone test: on its own
//! it would report nothing, which is why it is in RUST_SKIP.

use std::time::{Duration, Instant};
use toyos::device::{Keyboard, Mouse};
use toyos::endow::Endowments;
use toyos::syscap::SysCap;
use toyos_abi::syscall::{DeviceType, SYSCAP_LABEL};
use toyos_abi::input::RawKeyEvent;

const KEY_SIZE: usize = std::mem::size_of::<RawKeyEvent>();
const MOUSE_SIZE: usize = 6;

/// The host's end-of-run marker, and the only right button in its sequence.
/// PS/2 bit 1 is right and so is HID boot-mouse bit 1.
const RIGHT: u8 = 0x02;

fn main() {
    let cap = capability();
    let keyboard: Keyboard =
        cap.claim(DeviceType::Keyboard).expect("input_events: no keyboard device");
    let mouse: Mouse = cap.claim(DeviceType::Mouse).expect("input_events: no mouse device");
    let mut translator = window::configured_translator();
    println!("===INPUT_READY===");

    // A liveness ceiling, not a duration: the host's sequence ends on the
    // release of the right button, which nothing else in it produces, so a
    // path that delivers nothing fails rather than hangs.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut buf = [0u8; 1024];
    let (mut keys, mut pointer) = (0, 0);
    let mut right_down = false;
    let mut ended = false;
    while !ended && Instant::now() < deadline {
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
            // The release ends the run, not the press: the host reads the
            // button state after the last click, and a marker that swallowed
            // its own release would leave one held.
            if chunk[0] & RIGHT != 0 {
                right_down = true;
            } else if right_down {
                ended = true;
            }
        }

        if idle {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    println!("input done keys={keys} pointer={pointer}");
}

/// The device-minting capability the test estate is endowed
/// (`specs/capability-endowment-spec.md` §3). A claim is `/bin/init`'s to
/// mint everywhere else; here test-runner passes a `DEVICE` duplicate down, so
/// a boot can run several binaries that each need an input device.
fn capability() -> SysCap {
    Endowments::get()
        .take(SYSCAP_LABEL)
        .expect("the test estate is endowed a device-minting capability")
}

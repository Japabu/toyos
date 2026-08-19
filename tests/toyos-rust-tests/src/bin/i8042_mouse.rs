//! Claims the mouse fd and prints every pointer event that arrives.
//!
//! Driven by the `i8042_mouse` host test, which paces its injection against
//! these lines: a packet goes out for each one printed here, so this is the
//! host's clock as well as its evidence. Not a standalone test — in RUST_SKIP
//! for that reason.

use std::time::{Duration, Instant};
use toyos::device::Mouse;
use toyos::endow::Endowments;
use toyos::syscap::SysCap;
use toyos_abi::syscall::{DeviceType, SYSCAP_LABEL};

const EVENT_SIZE: usize = 6;

/// The host's end-of-run marker, and the only right button in its sequence.
/// PS/2 bit 1 is right and so is HID boot-mouse bit 1.
const RIGHT: u8 = 0x02;

/// A liveness ceiling, not a duration: the run ends on the marker's release,
/// so this only bounds a machine that lost it.
const RUN_CEILING: Duration = Duration::from_secs(60);

fn main() {
    let mouse: Mouse =
        capability().claim(DeviceType::Mouse).expect("i8042_mouse: no mouse device");
    println!("===I8042_MOUSE_READY===");

    let deadline = Instant::now() + RUN_CEILING;
    let mut buf = [0u8; 1024];
    let mut seen = 0;
    let mut right_down = false;
    let mut ended = false;
    while !ended && Instant::now() < deadline {
        let n = mouse.read_nonblock(&mut buf).unwrap_or(0);
        if n == 0 {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        for chunk in buf[..n].chunks_exact(EVENT_SIZE) {
            println!(
                "mev buttons=0x{:02x} x={} y={}",
                chunk[0],
                u16::from_le_bytes([chunk[2], chunk[3]]),
                u16::from_le_bytes([chunk[4], chunk[5]]),
            );
            seen += 1;
            // The release ends the run, not the press: the host's framing
            // assertion reads the button state after the last click, and a
            // marker that swallowed its own release would leave one held.
            if chunk[0] & RIGHT != 0 {
                right_down = true;
            } else if right_down {
                ended = true;
            }
        }
    }
    println!("mev done seen={seen}");
}

/// The device-minting capability the test estate is endowed. A claim is
/// `/bin/init`'s to mint everywhere else; here test-runner passes a `DEVICE`
/// duplicate down, so a boot can run several binaries that each need an input
/// device.
fn capability() -> SysCap {
    Endowments::get()
        .take(SYSCAP_LABEL)
        .expect("the test estate is endowed a device-minting capability")
}

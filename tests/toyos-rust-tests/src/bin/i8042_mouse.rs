//! Claims the mouse fd and prints every pointer event that arrives.
//!
//! Driven by the `i8042_mouse` host test, which injects through QMP once the
//! ready line appears. Not a standalone test — in RUST_SKIP for that reason.

use std::time::{Duration, Instant};
use toyos::device::Mouse;

const EVENT_SIZE: usize = 6;

fn main() {
    let mouse = Mouse::open().expect("i8042_mouse: no mouse device");
    println!("===I8042_MOUSE_READY===");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buf = [0u8; 1024];
    let mut seen = 0;
    while Instant::now() < deadline {
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
        }
    }
    println!("mev done seen={seen}");
}

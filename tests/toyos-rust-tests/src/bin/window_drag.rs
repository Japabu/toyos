//! A window the host drags by its title bar, reporting from inside where the
//! pointer landed.
//!
//! Needs a live compositor, which the shared boot does not have — it is in
//! `RUST_SKIP` and `metal_sim_window_drag` runs it on the metal-sim profile.
//!
//! It exists because the host cannot see where a window is. The compositor
//! places one and tells the client its size and nothing else, so a test that
//! aimed at a title bar from arithmetic would be a test that agrees with a copy
//! of the compositor's own layout constants. Instead this prints the
//! *content-local* coordinates of every press it is given, which is a
//! measurement of where the window is: the host clicks a known screen pixel,
//! reads back what that pixel is called in here, and has the window's origin.
//! The same press after a drag is what proves the window moved, and by how far.
//!
//! Small on purpose. The point of the run is that dragging a window costs the
//! area of the window, so a window that filled the screen would pass a gate
//! about screen-sized damage without meaning anything.

use std::time::{Duration, Instant};

use window::{Color, Event, Window, MOUSE_PRESS};

/// Short enough that the middle of the screen and the title bar are a known
/// distance apart — the host aims by this height and by the fact that the
/// compositor centres what it is given a size for — and tall enough that the
/// drag's own displacement lands inside the content again, which is how the
/// host proves the window moved.
const WIDTH: u32 = 400;
const HEIGHT: u32 = 160;

/// The host's sequence puts two presses inside this content: one naming the
/// pixel under the middle of the screen, one naming it again after the drag.
/// The press that starts the drag is on the title bar and the compositor keeps
/// it, so the second is the end of the sequence.
const PRESSES: usize = 2;

/// A liveness ceiling, not a duration: it costs nothing when the presses
/// arrive, and bounds a run where the injected pointer never reached this
/// window at all.
const RUN_CEILING: Duration = Duration::from_secs(30);

fn main() {
    let mut window = Window::create_with_title(WIDTH, HEIGHT, "drag")
        .unwrap_or_else(|e| panic!("the compositor would not give this a window: {e}"));

    let fb = window.framebuffer();
    fb.fill_rect(0, 0, WIDTH as usize, HEIGHT as usize, Color { r: 0x20, g: 0x40, b: 0x60 });
    window.present();

    println!("drag probe: {WIDTH}x{HEIGHT} window up");
    println!("===DRAG_READY===");

    let deadline = Instant::now() + RUN_CEILING;
    let mut presses = 0;
    while presses < PRESSES && Instant::now() < deadline {
        match window.poll_event(Duration::from_millis(200).as_nanos() as u64) {
            Some(Event::MouseInput(ev)) if ev.event_type == MOUSE_PRESS && ev.changed == 1 => {
                presses += 1;
                println!("drag probe: press at {},{}", ev.x, ev.y);
            }
            Some(Event::Close) => break,
            _ => {}
        }
    }
    println!("drag probe: {presses} presses seen");
}

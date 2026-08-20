//! The desktop.
//!
//! Four layers, and the split is the point. `toyos_desktop` decides — where a
//! window goes, what changed since the last frame, what the pointer hit, who
//! has the keyboard, what a key means, what is visible in a damaged region —
//! and every one of those is a pure function with host tests that run in
//! milliseconds. [`session`] does the rest: it claims the devices, drains the
//! client connections without ever blocking on one, allocates and grants the
//! shared
//! buffers, and hands finished rectangles to the panel. [`render`] turns a
//! compose plan into pixels, and [`client`] is the framing and the protocol's
//! failure vocabulary.
//!
//! What is left in this file is the policy the other three read: the numbers
//! that are decisions rather than derivations.

mod client;
mod render;
mod session;
mod stats;

use std::time::Duration;

use client::MAX_PENDING_CONNS;
use toyos::poller::Poller;

pub const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(400);
pub const FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667); // ~60fps
pub const STATS_INTERVAL: Duration = Duration::from_secs(2);

/// How long one pass may spend draining clients before it must composite.
///
/// Without it a client that never stops sending keeps its handle ready forever
/// and the loop never reaches the composite — a freeze with a different shape and
/// the same result. The drain's promise is "everything pending", and this is
/// the clause that makes it "or one frame's worth, whichever is sooner".
pub const DRAIN_BUDGET: Duration = FRAME_INTERVAL;

pub const FLAG_HARDWARE_CURSOR: u32 = 1 << 0;

/// Handles the compositor watches that are not windows: keyboard, mouse,
/// listener.
pub const FIXED_POLL_HANDLES: u32 = 3;

/// Hard ceiling on live windows, from the poller rather than from memory.
///
/// Every window's handle is registered in the same batch as the three fixed
/// ones
/// and the pending connections, and [`Poller::MAX_HANDLES`] is the widest set
/// one poller can carry. Unlike the memory budget this does not move when the
/// resolution does, which is why the poller is sized from it:
/// `MSG_SET_RESOLUTION` can make windows cheaper mid-run, and a poller sized
/// for the old screen would then be too small. At any resolution this machine
/// can actually scan out, the memory budget is far below this and is what
/// binds.
pub const MAX_WINDOW_SLOTS: u32 = Poller::MAX_HANDLES - FIXED_POLL_HANDLES - MAX_PENDING_CONNS;

/// Edge of the cursor sprites, in pixels — the size they are rasterized at and
/// the size the software cursor damages.
pub const CURSOR_PX: u32 = 20;

/// What the launcher offers: the label it shows and the program it starts.
pub const LAUNCHER_APPS: &[(&str, &str)] =
    &[("Terminal", "/bin/terminal"), ("Files", "/bin/files")];

fn main() {
    let mut session = session::Session::start();
    loop {
        session.pass();
    }
}

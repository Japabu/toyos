use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicU16, Ordering};
use crate::io_uring::RingId;
use crate::sync::Lock;
pub use toyos_abi::input::MouseEvent;

static MOUSE_BUF: Lock<VecDeque<MouseEvent>> = Lock::new(VecDeque::new());
static LAST_X: AtomicU16 = AtomicU16::new(0);
static LAST_Y: AtomicU16 = AtomicU16::new(0);
static IO_URING_WATCHERS: Lock<Vec<RingId>> = Lock::new(Vec::new());

/// Which physical pointer a report came from. Buttons are tracked per source
/// and published as their OR: with two live pointers a report carrying
/// buttons=0 from one of them would otherwise release a button the other is
/// holding, and the button flaps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PointerSource {
    Usb = 0,
    Ps2 = 1,
}

static BUTTONS: [AtomicU8; 2] = [AtomicU8::new(0), AtomicU8::new(0)];

/// Where a report puts the pointer. Absolute (USB tablet) and relative
/// (boot mouse, TrackPoint) accumulate into the same position because the
/// machine has one cursor.
#[derive(Clone, Copy, Debug)]
pub enum Motion {
    Relative { dx: i32, dy: i32 },
    Absolute { x: u16, y: u16 },
}

/// Relative counts scaled into the 0..32767 absolute space the compositor
/// consumes. Shared, so a device that feels wrong gets a per-source scale in
/// its own driver — never a change here, which would silently retune every
/// other pointer.
const REL_SCALE: i32 = 64;

pub fn add_io_uring_watcher(id: RingId) {
    let mut w = IO_URING_WATCHERS.lock();
    if !w.contains(&id) { w.push(id); }
}

pub fn remove_io_uring_watcher(id: RingId) {
    IO_URING_WATCHERS.lock().retain(|&x| x != id);
}

/// Wake every thread blocked on mouse input.
pub fn wake_waiters() {
    crate::sched::waitqs::wake_all(&crate::sched::waitqs::MOUSE);
}

pub fn io_uring_watchers() -> Vec<RingId> {
    IO_URING_WATCHERS.lock().clone()
}

/// Queue one pointer update. Returns true iff an event was queued.
///
/// The single production path from a pointer of any kind into `MOUSE_BUF`:
/// it owns the per-source button merge and the relative→absolute
/// accumulation, so a second pointer cannot contradict the first.
pub fn handle_motion(
    source: PointerSource,
    buttons: u8,
    motion: Motion,
    scroll: i8,
) -> bool {
    let prev = BUTTONS[source as usize].swap(buttons, Ordering::Relaxed);
    let merged = BUTTONS.iter().fold(0, |acc, b| acc | b.load(Ordering::Relaxed));

    let last_x = LAST_X.load(Ordering::Relaxed);
    let last_y = LAST_Y.load(Ordering::Relaxed);
    let (abs_x, abs_y) = match motion {
        Motion::Absolute { x, y } => (x, y),
        Motion::Relative { dx, dy } => (
            (last_x as i32 + dx * REL_SCALE).clamp(0, 32767) as u16,
            (last_y as i32 + dy * REL_SCALE).clamp(0, 32767) as u16,
        ),
    };

    if abs_x == last_x && abs_y == last_y && scroll == 0 && buttons == prev {
        return false;
    }
    LAST_X.store(abs_x, Ordering::Relaxed);
    LAST_Y.store(abs_y, Ordering::Relaxed);
    MOUSE_BUF.lock().push_back(MouseEvent { buttons: merged, scroll, abs_x, abs_y });
    true
}

/// Process a HID mouse/tablet report. Returns the number of events queued.
///
/// 6-byte tablet report: [buttons, x_lo, x_hi, y_lo, y_hi, scroll]
/// 3/4-byte boot mouse report: [buttons, dx, dy, scroll?]
pub fn handle_report(report: &[u8]) -> usize {
    let queued = if report.len() >= 6 {
        handle_motion(
            PointerSource::Usb,
            report[0],
            Motion::Absolute {
                x: u16::from_le_bytes([report[1], report[2]]),
                y: u16::from_le_bytes([report[3], report[4]]),
            },
            report[5] as i8,
        )
    } else if report.len() >= 3 {
        handle_motion(
            PointerSource::Usb,
            report[0],
            Motion::Relative { dx: report[1] as i8 as i32, dy: report[2] as i8 as i32 },
            if report.len() > 3 { report[3] as i8 } else { 0 },
        )
    } else {
        false
    };
    queued as usize
}

pub fn has_data() -> bool {
    !MOUSE_BUF.lock().is_empty()
}

pub fn try_read_event() -> Option<MouseEvent> {
    MOUSE_BUF.lock().pop_front()
}

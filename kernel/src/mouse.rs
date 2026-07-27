use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicU16, Ordering};
use crate::io_uring::RingId;
use crate::sync::Lock;
pub use toyos_abi::input::MouseEvent;

static MOUSE_BUF: Lock<VecDeque<MouseEvent>> = Lock::new(VecDeque::new());
static LAST_BUTTONS: AtomicU8 = AtomicU8::new(0);
static LAST_X: AtomicU16 = AtomicU16::new(0);
static LAST_Y: AtomicU16 = AtomicU16::new(0);
static IO_URING_WATCHERS: Lock<Vec<RingId>> = Lock::new(Vec::new());

pub fn add_io_uring_watcher(id: RingId) {
    let mut w = IO_URING_WATCHERS.lock();
    if !w.contains(&id) { w.push(id); }
}

pub fn remove_io_uring_watcher(id: RingId) {
    IO_URING_WATCHERS.lock().retain(|&x| x != id);
}

pub fn io_uring_watchers() -> Vec<RingId> {
    IO_URING_WATCHERS.lock().clone()
}

/// Process a HID mouse/tablet report.
///
/// 6-byte tablet report: [buttons, x_lo, x_hi, y_lo, y_hi, scroll]
/// 3/4-byte boot mouse report: [buttons, dx, dy, scroll?]
pub fn handle_report(report: &[u8]) {
    if report.len() >= 6 {
        // USB tablet: absolute coordinates
        let buttons = report[0];
        let abs_x = u16::from_le_bytes([report[1], report[2]]);
        let abs_y = u16::from_le_bytes([report[3], report[4]]);
        let scroll = report[5] as i8;
        let prev = LAST_BUTTONS.swap(buttons, Ordering::Relaxed);
        if abs_x == LAST_X.load(Ordering::Relaxed)
            && abs_y == LAST_Y.load(Ordering::Relaxed)
            && scroll == 0
            && buttons == prev
        {
            return;
        }
        LAST_X.store(abs_x, Ordering::Relaxed);
        LAST_Y.store(abs_y, Ordering::Relaxed);
        MOUSE_BUF.lock().push_back(MouseEvent { buttons, scroll, abs_x, abs_y });
    } else if report.len() >= 3 {
        // Boot protocol mouse: relative coordinates — convert to absolute
        // by accumulating into LAST_X/LAST_Y (clamped to 0–32767).
        let buttons = report[0];
        let dx = report[1] as i8 as i32;
        let dy = report[2] as i8 as i32;
        let scroll = if report.len() > 3 { report[3] as i8 } else { 0 };
        let prev = LAST_BUTTONS.swap(buttons, Ordering::Relaxed);
        if dx == 0 && dy == 0 && scroll == 0 && buttons == prev {
            return;
        }
        // Scale relative motion (~-127..127) into absolute space
        let abs_x = (LAST_X.load(Ordering::Relaxed) as i32 + dx * 64).clamp(0, 32767) as u16;
        let abs_y = (LAST_Y.load(Ordering::Relaxed) as i32 + dy * 64).clamp(0, 32767) as u16;
        LAST_X.store(abs_x, Ordering::Relaxed);
        LAST_Y.store(abs_y, Ordering::Relaxed);
        MOUSE_BUF.lock().push_back(MouseEvent { buttons, scroll, abs_x, abs_y });
    }
}

pub fn has_data() -> bool {
    !MOUSE_BUF.lock().is_empty()
}

pub fn try_read_event() -> Option<MouseEvent> {
    MOUSE_BUF.lock().pop_front()
}

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};
use crate::io_uring::RingId;
use crate::sync::Lock;
pub use toyos_abi::input::MouseEvent;

static MOUSE_BUF: Lock<VecDeque<MouseEvent>> = Lock::new(VecDeque::new());
static LAST_BUTTONS: AtomicU8 = AtomicU8::new(0);
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

/// Process a HID boot protocol mouse report (3+ bytes).
pub fn handle_report(report: &[u8]) {
    let buttons = report[0];
    let dx = report[1] as i8;
    let dy = report[2] as i8;
    let scroll = if report.len() > 3 { report[3] as i8 } else { 0 };
    let prev = LAST_BUTTONS.swap(buttons, Ordering::Relaxed);
    if dx == 0 && dy == 0 && scroll == 0 && buttons == prev {
        return;
    }
    MOUSE_BUF.lock().push_back(MouseEvent { buttons, dx, dy, scroll });
}

pub fn has_data() -> bool {
    !MOUSE_BUF.lock().is_empty()
}

pub fn try_read_event() -> Option<MouseEvent> {
    MOUSE_BUF.lock().pop_front()
}

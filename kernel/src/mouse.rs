use alloc::collections::VecDeque;
use crate::sync::SyncCell;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MouseEvent {
    pub buttons: u8,
    pub dx: i8,
    pub dy: i8,
}

static MOUSE_BUF: SyncCell<VecDeque<MouseEvent>> = SyncCell::new(VecDeque::new());

/// Process a HID boot protocol mouse report (3+ bytes).
pub fn handle_report(report: &[u8]) {
    let buttons = report[0];
    let dx = report[1] as i8;
    let dy = report[2] as i8;
    MOUSE_BUF.get_mut().push_back(MouseEvent { buttons, dx, dy });
}

pub fn has_data() -> bool {
    !MOUSE_BUF.get().is_empty()
}

pub fn try_read_event() -> Option<MouseEvent> {
    MOUSE_BUF.get_mut().pop_front()
}

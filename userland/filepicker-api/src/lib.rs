use std::os::toyos::message::{self, Message};
use toyos_abi::services;

// Message types for filepicker protocol
pub const MSG_FILEPICKER_REQUEST: u32 = 1;
pub const MSG_FILEPICKER_RESULT: u32 = 2;

#[derive(Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum PickerMode {
    Open = 0,
    Save = 1,
}

#[repr(C)]
pub struct FilePickerRequest {
    pub mode: u8,
    pub start_dir_len: u16,
    pub start_dir: [u8; 256],
}

/// Request the system file picker. Blocks until the user picks a file or cancels.
/// Returns `Some(path)` if a file was chosen, `None` if cancelled.
pub fn pick_file(mode: PickerMode, start_dir: &str) -> Option<String> {
    let pid = services::find("filepicker")?.0;

    let mut req = FilePickerRequest {
        mode: mode as u8,
        start_dir_len: start_dir.len().min(256) as u16,
        start_dir: [0u8; 256],
    };
    let len = req.start_dir_len as usize;
    req.start_dir[..len].copy_from_slice(&start_dir.as_bytes()[..len]);

    message::send(pid, Message::new(MSG_FILEPICKER_REQUEST, req)).ok()?;

    // Block waiting for response
    loop {
        let msg = message::recv();
        if msg.msg_type() == MSG_FILEPICKER_RESULT {
            let bytes = msg.take_bytes();
            if bytes.is_empty() {
                return None; // Cancelled
            }
            return String::from_utf8(bytes).ok();
        }
        // Ignore other messages (shouldn't happen normally)
    }
}

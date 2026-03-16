use toyos_abi::message;
use toyos_abi::services;
use toyos_abi::Pid;

// Message types for filepicker protocol
pub const MSG_FILEPICKER_REQUEST: u32 = 1;
pub const MSG_FILEPICKER_RESULT: u32 = 2;

#[derive(Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum PickerMode {
    Open = 0,
    Save = 1,
}

/// Request the system file picker. Blocks until the user picks a file or cancels.
/// Returns `Some(path)` if a file was chosen, `None` if cancelled.
pub fn pick_file(mode: PickerMode, start_dir: &str) -> Option<String> {
    let pid = services::find("filepicker")?.0;

    let path_bytes = start_dir.as_bytes();
    let len = path_bytes.len().min(message::MAX_PAYLOAD - 1);
    let mut data = [0u8; message::MAX_PAYLOAD];
    data[0] = mode as u8;
    data[1..1 + len].copy_from_slice(&path_bytes[..len]);
    message::send_bytes(Pid(pid), MSG_FILEPICKER_REQUEST, &data[..1 + len]);

    loop {
        let msg = message::recv();
        if msg.msg_type == MSG_FILEPICKER_RESULT {
            let bytes = msg.bytes();
            if bytes.is_empty() {
                return None;
            }
            return String::from_utf8(bytes.to_vec()).ok();
        }
    }
}

use toyos::services;

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
    let conn = services::connect("filepicker").ok()?;

    let path_bytes = start_dir.as_bytes();
    let len = path_bytes.len().min(4095);
    let mut data = [0u8; 4096];
    data[0] = mode as u8;
    data[1..1 + len].copy_from_slice(&path_bytes[..len]);
    conn.send_bytes(MSG_FILEPICKER_REQUEST, &data[..1 + len]).ok();

    let header = conn.recv_header().ok()?;
    if header.msg_type == MSG_FILEPICKER_RESULT && header.len > 0 {
        let mut buf = [0u8; 4096];
        let n = conn.recv_bytes(&header, &mut buf).unwrap_or(0);
        String::from_utf8(buf[..n].to_vec()).ok()
    } else {
        None
    }
}

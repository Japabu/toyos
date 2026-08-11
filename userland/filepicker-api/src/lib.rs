use toyos::endow::{self, EndowError};

pub const MSG_FILEPICKER_REQUEST: u32 = 1;
pub const MSG_FILEPICKER_RESULT: u32 = 2;

#[derive(Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum PickerMode {
    Open = 0,
    Save = 1,
}

/// Why no path came back, when the user did not simply cancel.
///
/// **`None` used to be six different things** — no process serving
/// `filepicker` yet, a refused send, a refused header read, a reply that is not
/// `MSG_FILEPICKER_RESULT`, a path that is not UTF-8, and the user changing
/// their mind — and a caller could not tell them apart, so an editor that
/// opened its picker before the compositor had spawned one reported a
/// cancellation. The boot-race cause is gone: the `filepicker` port exists
/// before any process does, so a connection works from this program's first
/// instruction. What is left is worth naming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickError {
    /// This program was given no `filepicker` connector.
    NotEndowed,
    /// The picker exited, or the connection died mid-request.
    PickerGone,
    /// The picker answered with something this exchange does not allow.
    Protocol(u32),
    /// The picker answered with a path that is not UTF-8.
    NotUtf8,
}

impl core::fmt::Display for PickError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotEndowed => write!(f, "this program was given no file picker"),
            Self::PickerGone => write!(f, "the file picker is gone"),
            Self::Protocol(t) => write!(f, "the file picker answered with message type {t}"),
            Self::NotUtf8 => write!(f, "the file picker answered with a path that is not text"),
        }
    }
}

impl From<EndowError> for PickError {
    fn from(e: EndowError) -> Self {
        match e {
            EndowError::NotEndowed => Self::NotEndowed,
            EndowError::ServerGone | EndowError::Refused(_) => Self::PickerGone,
        }
    }
}

/// Ask the system file picker for a path.
///
/// `Ok(None)` is the user cancelling and nothing else.
pub fn pick_file(mode: PickerMode, start_dir: &str) -> Result<Option<String>, PickError> {
    let conn = endow::service("filepicker")?;

    let path_bytes = start_dir.as_bytes();
    let len = path_bytes.len().min(4095);
    let mut data = [0u8; 4096];
    data[0] = mode as u8;
    data[1..1 + len].copy_from_slice(&path_bytes[..len]);
    conn.send_bytes(MSG_FILEPICKER_REQUEST, &data[..1 + len])
        .map_err(|_| PickError::PickerGone)?;

    let header = conn.recv_header().map_err(|_| PickError::PickerGone)?;
    if header.msg_type != MSG_FILEPICKER_RESULT {
        return Err(PickError::Protocol(header.msg_type));
    }
    if header.len() == 0 {
        return Ok(None);
    }
    let mut buf = [0u8; 4096];
    let n = conn.recv_bytes(&header, &mut buf).map_err(|_| PickError::PickerGone)?;
    String::from_utf8(buf[..n].to_vec()).map(Some).map_err(|_| PickError::NotUtf8)
}

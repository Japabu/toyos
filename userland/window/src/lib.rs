use std::os::toyos::io;
use std::os::toyos::message::{self, Message};
use std::sync::OnceLock;

fn compositor_pid() -> u32 {
    static PID: OnceLock<u32> = OnceLock::new();
    *PID.get_or_init(|| io::find_pid("compositor").expect("no compositor running"))
}

// Client → Compositor
pub const MSG_CREATE_WINDOW: u32 = 1;
pub const MSG_PRESENT: u32 = 2;

// Compositor → Client
pub const MSG_WINDOW_CREATED: u32 = 1;
pub const MSG_KEY_INPUT: u32 = 2;

#[repr(C)]
pub struct CreateWindowRequest {
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
pub struct WindowInfo {
    pub buffer: *mut u8,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
}

#[repr(C)]
pub struct KeyEvent {
    pub len: u8,
    pub bytes: [u8; 16],
}

pub enum Event {
    KeyInput(KeyEvent),
}

/// Receive the next window event from the compositor.
pub fn recv_event() -> Event {
    let msg = message::recv();
    match msg.msg_type() {
        MSG_KEY_INPUT => Event::KeyInput(msg.take_payload()),
        other => panic!("unknown window event type: {other}"),
    }
}

pub struct Window {
    buffer: *mut u8,
    width: u32,
    height: u32,
    pixel_format: u32,
}

impl Window {
    /// Request a window from the compositor. Blocks until the window is created.
    /// Pass 0 for width/height to let the compositor decide.
    pub fn create(width: u32, height: u32) -> Self {
        message::send(compositor_pid(), Message::new(
            MSG_CREATE_WINDOW,
            CreateWindowRequest { width, height },
        ));

        let response = message::recv();
        assert_eq!(response.msg_type(), MSG_WINDOW_CREATED, "unexpected message from compositor");
        let info: WindowInfo = response.take_payload();

        Self {
            buffer: info.buffer,
            width: info.width,
            height: info.height,
            pixel_format: info.pixel_format,
        }
    }

    /// Notify the compositor that the buffer has been updated.
    pub fn present(&self) {
        message::send(compositor_pid(), Message::signal(MSG_PRESENT));
    }

    pub fn buffer_ptr(&self) -> *mut u8 {
        self.buffer
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn pixel_format(&self) -> u32 {
        self.pixel_format
    }
}

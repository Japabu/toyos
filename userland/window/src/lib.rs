pub mod framebuffer;

pub use framebuffer::{Color, Framebuffer};

use toyos_abi::syscall;
use std::os::toyos::message::{self, Message};
use std::sync::OnceLock;

fn compositor_pid() -> u32 {
    static PID: OnceLock<u32> = OnceLock::new();
    *PID.get_or_init(|| syscall::find_pid("compositor").expect("no compositor running"))
}

// Client → Compositor
pub const MSG_CREATE_WINDOW: u32 = 1;
pub const MSG_PRESENT: u32 = 2;
pub const MSG_CLIPBOARD_SET: u32 = 3;

// Compositor → Client
pub const MSG_WINDOW_CREATED: u32 = 1;
pub const MSG_KEY_INPUT: u32 = 2;
pub const MSG_WINDOW_RESIZED: u32 = 3;
pub const MSG_WINDOW_CLOSE: u32 = 4;
pub const MSG_MOUSE_INPUT: u32 = 5;
pub const MSG_CLIPBOARD_PASTE: u32 = 6;
pub const MSG_FRAME: u32 = 7;

#[repr(C)]
pub struct CreateWindowRequest {
    pub width: u32,
    pub height: u32,
    pub title_len: u8,
    pub title: [u8; 31],
}

pub const MOUSE_MOVE: u8 = 0;
pub const MOUSE_PRESS: u8 = 1;
pub const MOUSE_RELEASE: u8 = 2;
pub const MOUSE_SCROLL: u8 = 3;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MouseEvent {
    pub x: u16,
    pub y: u16,
    pub buttons: u8,
    pub event_type: u8,
    pub changed: u8,
    pub scroll: i8,
}

#[repr(C)]
pub struct WindowInfo {
    pub token: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
}

#[repr(C)]
pub struct ResizeInfo {
    pub token: u32,
    pub old_token: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
}

pub const MOD_SHIFT: u8 = 1;
pub const MOD_CTRL: u8 = 2;
pub const MOD_ALT: u8 = 4;
pub const MOD_GUI: u8 = 8;
pub const MOD_RELEASED: u8 = 0x10;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KeyEvent {
    pub keycode: u8,
    pub modifiers: u8,
    pub len: u8,
    pub translated: [u8; 5],
}

impl KeyEvent {
    pub const EMPTY: Self = Self { keycode: 0, modifiers: 0, len: 0, translated: [0; 5] };

    pub fn pressed(&self) -> bool { self.modifiers & MOD_RELEASED == 0 }
    pub fn released(&self) -> bool { self.modifiers & MOD_RELEASED != 0 }
    pub fn shift(&self) -> bool { self.modifiers & MOD_SHIFT != 0 }
    pub fn ctrl(&self) -> bool { self.modifiers & MOD_CTRL != 0 }
    pub fn alt(&self) -> bool { self.modifiers & MOD_ALT != 0 }
    pub fn gui(&self) -> bool { self.modifiers & MOD_GUI != 0 }
}

pub enum Event {
    KeyInput(KeyEvent),
    MouseInput(MouseEvent),
    ClipboardPaste(Vec<u8>),
    Resized,
    Close,
    Frame,
}

/// Set the system clipboard contents.
pub fn clipboard_set(text: &str) {
    message::send(compositor_pid(), Message::from_bytes(MSG_CLIPBOARD_SET, text.as_bytes()));
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
        Self::create_with_title(width, height, "")
    }

    pub fn create_with_title(width: u32, height: u32, title: &str) -> Self {
        let mut req = CreateWindowRequest {
            width,
            height,
            title_len: 0,
            title: [0; 31],
        };
        let bytes = title.as_bytes();
        let len = bytes.len().min(31);
        req.title[..len].copy_from_slice(&bytes[..len]);
        req.title_len = len as u8;
        message::send(compositor_pid(), Message::new(MSG_CREATE_WINDOW, req));

        let response = message::recv();
        assert_eq!(response.msg_type(), MSG_WINDOW_CREATED, "unexpected message from compositor");
        let info: WindowInfo = response.take_payload();
        let buffer = syscall::map_shared(info.token);

        Self {
            buffer,
            width: info.width,
            height: info.height,
            pixel_format: info.pixel_format,
        }
    }

    /// Receive the next window event from the compositor. Blocks until an event arrives.
    pub fn recv_event(&mut self) -> Event {
        let msg = message::recv();
        self.decode_event(msg)
    }

    /// Wait up to `timeout_nanos` for an event. Returns `None` on timeout.
    pub fn poll_event(&mut self, timeout_nanos: u64) -> Option<Event> {
        let result = syscall::poll_timeout(&[], timeout_nanos);
        if result.messages() {
            Some(self.recv_event())
        } else {
            None
        }
    }

    fn decode_event(&mut self, msg: Message) -> Event {
        match msg.msg_type() {
            MSG_KEY_INPUT => Event::KeyInput(msg.take_payload()),
            MSG_MOUSE_INPUT => Event::MouseInput(msg.take_payload()),
            MSG_WINDOW_RESIZED => {
                let info: ResizeInfo = msg.take_payload();
                syscall::release_shared(info.old_token);
                self.buffer = syscall::map_shared(info.token);
                self.width = info.width;
                self.height = info.height;
                self.pixel_format = info.pixel_format;
                Event::Resized
            }
            MSG_CLIPBOARD_PASTE => Event::ClipboardPaste(msg.take_bytes()),
            MSG_WINDOW_CLOSE => Event::Close,
            MSG_FRAME => Event::Frame,
            other => panic!("unknown window event type: {other}"),
        }
    }

    /// Notify the compositor that the buffer has been updated.
    pub fn present(&self) {
        message::send(compositor_pid(), Message::signal(MSG_PRESENT));
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn framebuffer(&self) -> Framebuffer {
        Framebuffer::new(
            self.buffer,
            self.width as usize,
            self.height as usize,
            self.width as usize,
            self.pixel_format,
        )
    }
}

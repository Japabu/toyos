pub mod framebuffer;

pub use framebuffer::{Color, Framebuffer, Screen, Traffic};

use toyos::ipc;
use toyos::poller::{Poller, IORING_POLL_IN};
use toyos::services;
use toyos::surface;
use toyos::Connection;
use toyos::shm::SharedMemory;
/// Re-exported because [`KeyPress`] is made of them and a client that holds
/// its own translator — `/bin/console`, a test that stands in for a surface —
/// should not have to name a second crate to do what this one does.
pub use toyos_keymap::{Emit, Mods, Translator};

// Window flags
pub const WINDOW_FLAG_TOPMOST: u8 = 1;

// Client → Compositor
pub const MSG_CREATE_WINDOW: u32 = 1;
pub const MSG_PRESENT: u32 = 2;
pub const MSG_CLIPBOARD_SET: u32 = 3;
pub const MSG_DESTROY_WINDOW: u32 = 4;
pub const MSG_SET_CURSOR: u32 = 5;
pub const MSG_SET_RESOLUTION: u32 = 6;
pub const MSG_GET_RESOLUTION: u32 = 7;

// Cursor styles
pub const CURSOR_DEFAULT: u8 = 0;
pub const CURSOR_CROSSHAIR: u8 = 1;
pub const CURSOR_RESIZE: u8 = 2;

// Compositor → Client
pub const MSG_WINDOW_CREATED: u32 = 1;
pub const MSG_KEY_INPUT: u32 = 2;
pub const MSG_WINDOW_RESIZED: u32 = 3;
pub const MSG_WINDOW_CLOSE: u32 = 4;
pub const MSG_MOUSE_INPUT: u32 = 5;
pub const MSG_CLIPBOARD_PASTE: u32 = 6;
pub const MSG_FRAME: u32 = 7;
pub const MSG_RESOLUTION_CHANGED: u32 = 8;
/// The compositor will not create the window that was asked for. Payload is a
/// [`WindowRefused`]. The only server→client message that answers
/// `MSG_CREATE_WINDOW` other than [`MSG_WINDOW_CREATED`], and the reason
/// [`Window::create`] is fallible: without it a compositor that cannot afford
/// another window has no move except to serve it or to drop the connection.
pub const MSG_WINDOW_REFUSED: u32 = 9;

// Shared-memory clipboard (for payloads > 116 bytes)
pub const MSG_CLIPBOARD_SET_SHM: u32 = 10;
pub const MSG_CLIPBOARD_PASTE_SHM: u32 = 11;

/// Either direction: [`surface::LAYOUT_CONFIG`] changed, re-read it.
///
/// The compositor translates nothing, so it does not act on this — it is the
/// root of the surface tree and its job is to give every window the same
/// answer. A client sends it after writing the config; the compositor sends it
/// to every window it holds.
pub const MSG_LAYOUT_CHANGED: u32 = 12;

/// Wire reasons carried by [`MSG_WINDOW_REFUSED`]. A client that does not know
/// a reason still knows it was refused, so adding one is backwards compatible
/// with an older client — [`CreateError::Refused`] carries the raw value.
pub const REFUSED_AT_CAPACITY: u32 = 1;
pub const REFUSED_TOO_LARGE: u32 = 2;

toyos::ipc_payload! {
    pub struct WindowRefused {
        pub reason: u32,
    }
}

/// Why creating a window failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateError {
    /// Nothing is serving the `compositor` service, or it went away between
    /// the request and the reply.
    NoCompositor,
    /// The compositor is already holding as many windows as it can afford.
    AtCapacity,
    /// The requested size is bigger than the screen it would be drawn on.
    TooLarge,
    /// Refused for a reason this build of the client does not know.
    Refused(u32),
    /// The compositor answered `MSG_CREATE_WINDOW` with a message that is
    /// neither [`MSG_WINDOW_CREATED`] nor [`MSG_WINDOW_REFUSED`].
    Protocol(u32),
}

impl std::fmt::Display for CreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCompositor => write!(f, "no compositor is running"),
            Self::AtCapacity => write!(f, "the compositor is at its window limit"),
            Self::TooLarge => write!(f, "the window is larger than the screen"),
            Self::Refused(reason) => write!(f, "the compositor refused (reason {reason})"),
            Self::Protocol(msg_type) => {
                write!(f, "the compositor answered with message type {msg_type}")
            }
        }
    }
}

impl std::error::Error for CreateError {}

impl CreateError {
    fn from_wire(reason: u32) -> Self {
        match reason {
            REFUSED_AT_CAPACITY => Self::AtCapacity,
            REFUSED_TOO_LARGE => Self::TooLarge,
            other => Self::Refused(other),
        }
    }
}

toyos::ipc_payload! {
    /// A rectangle of a surface, in that surface's own pixels.
    ///
    /// The payload of [`MSG_PRESENT`], where it is the client's claim about
    /// which of its pixels changed. A compositor that has to assume the whole
    /// window changed repaints the whole window for one typed character —
    /// which is what this exists to stop — so the claim is load-bearing and
    /// the compositor clamps it to the window rather than trusting it.
    pub struct Rect {
        pub x: u32,
        pub y: u32,
        pub w: u32,
        pub h: u32,
    }

    pub struct ClipboardShmMsg {
        pub token: u32,
        pub len: u32,
    }

    pub struct ResolutionRequest {
        pub width: u32,
        pub height: u32,
    }

    pub struct ResolutionInfo {
        pub width: u32,
        pub height: u32,
    }

    pub struct CreateWindowRequest {
        pub width: u32,
        pub height: u32,
        pub flags: u8,
        pub title_len: u8,
        pub title: [u8; 30],
    }
}

impl Rect {
    pub fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.w).max(other.x + other.w);
        let bottom = (self.y + self.h).max(other.y + other.h);
        Self { x, y, w: right - x, h: bottom - y }
    }
}

pub const MOUSE_MOVE: u8 = 0;
pub const MOUSE_PRESS: u8 = 1;
pub const MOUSE_RELEASE: u8 = 2;
pub const MOUSE_SCROLL: u8 = 3;

toyos::ipc_payload! {
    pub struct MouseEvent {
        pub x: u16,
        pub y: u16,
        pub buttons: u8,
        pub event_type: u8,
        pub changed: u8,
        pub scroll: i8,
    }

    pub struct WindowInfo {
        pub token: u32,
        pub width: u32,
        pub height: u32,
        pub stride: u32,
        pub pixel_format: u32,
    }

    pub struct ResizeInfo {
        pub token: u32,
        pub old_token: u32,
        pub width: u32,
        pub height: u32,
        pub stride: u32,
        pub pixel_format: u32,
    }
}

pub const MOD_SHIFT: u8 = 1;
pub const MOD_CTRL: u8 = 2;
pub const MOD_ALT: u8 = 4;
pub const MOD_GUI: u8 = 8;
pub const MOD_RELEASED: u8 = 0x10;

toyos::ipc_payload! {
    /// The kernel's [`toyos_abi::input::RawKeyEvent`], as the compositor
    /// forwards it.
    ///
    /// Byte-identical to it and asserted so below: the compositor reads the
    /// keyboard device into an array of these and hands whole ones on, which
    /// is what makes "the compositor translates nothing" a property of the
    /// wire and not of a convention.
    pub struct KeyEvent {
        pub keycode: u8,
        pub modifiers: u8,
    }
}

const _: () = assert!(
    core::mem::size_of::<KeyEvent>() == core::mem::size_of::<toyos_abi::input::RawKeyEvent>(),
);

impl KeyEvent {
    pub const EMPTY: Self = Self { keycode: 0, modifiers: 0 };

    pub fn pressed(&self) -> bool { self.modifiers & MOD_RELEASED == 0 }
    pub fn released(&self) -> bool { self.modifiers & MOD_RELEASED != 0 }
    pub fn shift(&self) -> bool { self.modifiers & MOD_SHIFT != 0 }
    pub fn ctrl(&self) -> bool { self.modifiers & MOD_CTRL != 0 }
    pub fn alt(&self) -> bool { self.modifiers & MOD_ALT != 0 }
    pub fn gui(&self) -> bool { self.modifiers & MOD_GUI != 0 }

    pub fn mods(&self) -> Mods {
        Mods { shift: self.shift(), ctrl: self.ctrl(), alt: self.alt() }
    }
}

/// The same two bytes, for a window that hosts surfaces of its own and has to
/// pass a transition further down.
impl From<KeyEvent> for toyos_abi::input::RawKeyEvent {
    fn from(key: KeyEvent) -> Self {
        Self { keycode: key.keycode, modifiers: key.modifiers }
    }
}

impl From<toyos_abi::input::RawKeyEvent> for KeyEvent {
    fn from(key: toyos_abi::input::RawKeyEvent) -> Self {
        Self { keycode: key.keycode, modifiers: key.modifiers }
    }
}

/// A transition and what it types here, from [`Window::press`].
///
/// Separate from [`KeyEvent`] because they are answers to different questions:
/// [`KeyEvent`] is the wire form, identical on every surface the transition
/// passes through, and this is one surface's reading of it under one layout.
pub struct KeyPress {
    pub keycode: u8,
    pub modifiers: u8,
    text: Emit,
}

impl KeyPress {
    /// What this press types. Empty for a release, a modifier, and every key
    /// the layout leaves undefined.
    pub fn text(&self) -> &str {
        self.text.as_str()
    }

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
    /// The keyboard layout config changed; this window's translator has
    /// already been re-read. A client that hosts surfaces of its own passes
    /// the same message down to them.
    LayoutChanged,
    Frame,
}

/// The layout named in [`surface::LAYOUT_CONFIG`], or the built-in default
/// when there is no config to read.
///
/// Every translator in the system starts here and comes back here whenever it
/// is told the file moved, which is what stops two surfaces disagreeing about
/// a layout the user only chose once.
pub fn configured_translator() -> Translator {
    let mut translator = Translator::new();
    load_layout(&mut translator);
    translator
}

/// Point `translator` at the configured layout. A config naming a layout that
/// does not exist leaves it where it was — the alternative is silently
/// falling back to `us` on a machine whose owner asked for something else.
pub fn load_layout(translator: &mut Translator) {
    let Ok(config) = std::fs::read_to_string(surface::LAYOUT_CONFIG) else {
        return;
    };
    let name = config.trim();
    if !name.is_empty() && !translator.set_layout(name) {
        eprintln!("window: {:?} names no layout this build has", name);
    }
}

/// Set the system clipboard contents (standalone, uses a temporary compositor connection).
pub fn clipboard_set(text: &str) {
    use std::sync::Mutex;
    static CLIPBOARD_SHM: Mutex<Option<SharedMemory>> = Mutex::new(None);

    let conn = services::connect("compositor").expect("compositor not running");
    let bytes = text.as_bytes();
    if bytes.len() <= 4096 {
        let _ = conn.send_bytes(MSG_CLIPBOARD_SET, bytes);
    } else {
        let mut shm = SharedMemory::allocate(bytes.len());
        shm.as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
        let _ = conn.send(MSG_CLIPBOARD_SET_SHM, &ClipboardShmMsg { token: shm.token(), len: bytes.len() as u32 });
        *CLIPBOARD_SHM.lock().unwrap() = Some(shm);
    }
}

pub struct Window {
    conn: Connection,
    poller: Poller,
    shm: SharedMemory,
    width: u32,
    height: u32,
    pixel_format: u32,
    /// This window's own layout state. One per client process, which is what
    /// keeps a dead key typed into one window from composing with a letter
    /// typed into another.
    translator: Translator,
}

impl Window {
    /// Ask the compositor for a window. `0, 0` asks it to pick a size.
    ///
    /// Fails rather than panics: the compositor is entitled to say no — it
    /// bounds what it will hold — and "you may not have a window" is an
    /// ordinary answer to an ordinary request, not a bug in the caller.
    pub fn create(width: u32, height: u32) -> Result<Self, CreateError> {
        Self::create_with_title(width, height, "")
    }

    pub fn create_with_title(width: u32, height: u32, title: &str) -> Result<Self, CreateError> {
        Self::create_with_flags(width, height, title, 0)
    }

    pub fn create_topmost(width: u32, height: u32, title: &str) -> Result<Self, CreateError> {
        Self::create_with_flags(width, height, title, WINDOW_FLAG_TOPMOST)
    }

    fn create_with_flags(
        width: u32,
        height: u32,
        title: &str,
        flags: u8,
    ) -> Result<Self, CreateError> {
        let conn = services::connect("compositor").map_err(|_| CreateError::NoCompositor)?;

        let mut req = CreateWindowRequest {
            width,
            height,
            flags,
            title_len: 0,
            title: [0; 30],
        };
        let bytes = title.as_bytes();
        let len = bytes.len().min(30);
        req.title[..len].copy_from_slice(&bytes[..len]);
        req.title_len = len as u8;
        conn.send(MSG_CREATE_WINDOW, &req).map_err(|_| CreateError::NoCompositor)?;

        // Header first, then the payload the message type calls for: the two
        // answers carry different structs, and a payload shorter than the type
        // it was asked for is a refusal from `recv_payload`, not a window.
        let header = conn.recv_header().map_err(|_| CreateError::NoCompositor)?;
        match header.msg_type {
            MSG_WINDOW_CREATED => {}
            MSG_WINDOW_REFUSED => {
                let refused: WindowRefused = conn
                    .recv_payload(&header)
                    .map_err(|_| CreateError::NoCompositor)?;
                return Err(CreateError::from_wire(refused.reason));
            }
            other => return Err(CreateError::Protocol(other)),
        }
        let info: WindowInfo = conn
            .recv_payload(&header)
            .map_err(|_| CreateError::NoCompositor)?;

        let buf_size = info.stride as usize * info.height as usize * 4;
        let shm = SharedMemory::map(info.token, buf_size);

        let poller = Poller::new(1);
        Ok(Self {
            conn,
            poller,
            shm,
            width: info.width,
            height: info.height,
            pixel_format: info.pixel_format,
            translator: configured_translator(),
        })
    }

    /// Which layout this window is translating with.
    pub fn layout(&self) -> &'static str {
        self.translator.layout()
    }

    /// `key` with what it types on this window under the layout in force.
    ///
    /// **Called once per press by whoever wants the characters, and not at
    /// all by anyone who does not.** That is not a convenience: it advances
    /// the dead-key state, so a surface that is lending its keys to a child —
    /// a terminal while the layout wizard has them — must not call it, or a
    /// `^` the wizard consumed would still be waiting for a base character
    /// when the wizard exits.
    pub fn press(&mut self, key: KeyEvent) -> KeyPress {
        let text =
            if key.pressed() { self.translator.press(key.keycode, key.mods()) } else { Emit::EMPTY };
        KeyPress { keycode: key.keycode, modifiers: key.modifiers, text }
    }

    /// Tell the compositor that [`surface::LAYOUT_CONFIG`] changed, so every
    /// other window re-reads it too.
    pub fn notify_layout_changed(&self) {
        let _ = self.conn.signal(MSG_LAYOUT_CHANGED);
    }

    pub fn recv_event(&mut self) -> Event {
        let Ok(header) = self.conn.recv_header() else {
            return Event::Close;
        };
        self.decode_event(&header)
    }

    pub fn poll_event(&mut self, timeout_nanos: u64) -> Option<Event> {
        self.poller.poll_add(&self.conn, IORING_POLL_IN, 0);
        let mut ready = false;
        self.poller.wait(1, timeout_nanos, |_| ready = true);
        if ready {
            Some(self.recv_event())
        } else {
            None
        }
    }

    /// A message the compositor cannot have meant closes the window rather
    /// than killing the client: this side is a library inside somebody else's
    /// program, and it has a way to say "the session is over".
    fn decode_event(&mut self, header: &ipc::IpcHeader) -> Event {
        match header.msg_type {
            MSG_KEY_INPUT => match self.conn.recv_payload::<KeyEvent>(header) {
                Ok(key) => Event::KeyInput(key),
                Err(_) => Event::Close,
            },
            MSG_LAYOUT_CHANGED => {
                load_layout(&mut self.translator);
                Event::LayoutChanged
            }
            MSG_MOUSE_INPUT => match self.conn.recv_payload(header) {
                Ok(ev) => Event::MouseInput(ev),
                Err(_) => Event::Close,
            },
            MSG_WINDOW_RESIZED => {
                let Ok(info) = self.conn.recv_payload::<ResizeInfo>(header) else {
                    return Event::Close;
                };
                let buf_size = info.stride as usize * info.height as usize * 4;
                self.shm = SharedMemory::map(info.token, buf_size);
                self.width = info.width;
                self.height = info.height;
                self.pixel_format = info.pixel_format;
                Event::Resized
            }
            MSG_CLIPBOARD_PASTE => {
                let mut buf = [0u8; 4096];
                let Ok(n) = self.conn.recv_bytes(header, &mut buf) else {
                    return Event::Close;
                };
                Event::ClipboardPaste(buf[..n].to_vec())
            }
            MSG_CLIPBOARD_PASTE_SHM => {
                let Ok(info) = self.conn.recv_payload::<ClipboardShmMsg>(header) else {
                    return Event::Close;
                };
                let shm = SharedMemory::map(info.token, info.len as usize);
                let data = shm.as_slice()[..info.len as usize].to_vec();
                Event::ClipboardPaste(data)
            }
            MSG_WINDOW_CLOSE => Event::Close,
            MSG_FRAME => Event::Frame,
            _ => Event::Close,
        }
    }

    pub fn set_cursor(&self, style: u8) {
        let _ = self.conn.send(MSG_SET_CURSOR, &(style as u32));
    }

    pub fn set_clipboard(&self, text: &str) {
        let _ = self.conn.send_bytes(MSG_CLIPBOARD_SET, text.as_bytes());
    }

    /// Hand over the whole window.
    ///
    /// For a client that draws without keeping track of where — the honest
    /// answer for one of those is "all of it", and a damage rect it made up
    /// would cost the compositor correctness rather than time.
    pub fn present(&self) {
        self.present_damage(Rect { x: 0, y: 0, w: self.width, h: self.height });
    }

    /// Hand over `damage` and nothing else.
    ///
    /// The rect is in window pixels. Pixels outside it are the compositor's to
    /// leave alone, so a client that names less than it changed leaves a stale
    /// screen — [`Screen::take_damage`] is where the rect comes from for a
    /// client that composes through one.
    pub fn present_damage(&self, damage: Rect) {
        let _ = self.conn.send(MSG_PRESENT, &damage);
    }

    pub fn fd(&self) -> toyos_abi::Fd {
        self.conn.fd()
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn framebuffer(&self) -> Framebuffer {
        Framebuffer::new(
            self.shm.as_ptr(),
            self.width as usize,
            self.height as usize,
            self.width as usize,
            self.pixel_format,
        )
    }

    /// The same buffer as [`Window::framebuffer`], for a client that composes
    /// elsewhere and only ever hands finished pixels over.
    pub fn screen(&self) -> Screen {
        Screen::new(
            self.shm.as_ptr(),
            self.width as usize,
            self.height as usize,
            self.width as usize,
            self.pixel_format,
        )
    }
}

impl std::fmt::Debug for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Window")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let _ = self.conn.signal(MSG_DESTROY_WINDOW);
    }
}

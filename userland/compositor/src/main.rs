mod framebuffer;

use std::io::{Read, Write};
use std::os::toyos::io::{self, AsRawFd};
use std::os::toyos::message::{self, Message};
use std::process::{Command, Stdio};

use framebuffer::{Color, Framebuffer};

const MARGIN: usize = 40;
const TITLE_BAR_HEIGHT: usize = 28;
const BORDER_WIDTH: usize = 1;

const DESKTOP_COLOR: Color = Color { r: 0x2d, g: 0x2d, b: 0x2d };
const TITLE_BAR_COLOR: Color = Color { r: 0x33, g: 0x33, b: 0x33 };
const BORDER_COLOR: Color = Color { r: 0x55, g: 0x55, b: 0x55 };
const TITLE_TEXT_COLOR: Color = Color { r: 0xcc, g: 0xcc, b: 0xcc };

const CURSOR_WIDTH: usize = 12;
const CURSOR_HEIGHT: usize = 17;
const CURSOR_SPRITE: [&[u8]; CURSOR_HEIGHT] = [
    b"B...........",
    b"BB..........",
    b"BWB.........",
    b"BWWB........",
    b"BWWWB.......",
    b"BWWWWB......",
    b"BWWWWWB.....",
    b"BWWWWWWB....",
    b"BWWWWWWWB...",
    b"BWWWWWWWWB..",
    b"BWWWWWBBBBB.",
    b"BWWBWWB.....",
    b"BWBBWWB.....",
    b"BB..BWWB....",
    b"B...BWWB....",
    b"....BWWB....",
    b".....BB.....",
];

struct Cursor {
    x: i32,
    y: i32,
    saved_bg: Vec<Color>,
    visible: bool,
}

impl Cursor {
    fn new(x: i32, y: i32) -> Self {
        Self {
            x,
            y,
            saved_bg: vec![Color { r: 0, g: 0, b: 0 }; CURSOR_WIDTH * CURSOR_HEIGHT],
            visible: false,
        }
    }

    fn hide(&mut self, fb: &Framebuffer) {
        if !self.visible { return; }
        for cy in 0..CURSOR_HEIGHT {
            for cx in 0..CURSOR_WIDTH {
                let sx = self.x as usize + cx;
                let sy = self.y as usize + cy;
                fb.put_pixel(sx, sy, self.saved_bg[cy * CURSOR_WIDTH + cx]);
            }
        }
        self.visible = false;
    }

    fn show(&mut self, fb: &Framebuffer) {
        for cy in 0..CURSOR_HEIGHT {
            for cx in 0..CURSOR_WIDTH {
                let sx = self.x as usize + cx;
                let sy = self.y as usize + cy;
                self.saved_bg[cy * CURSOR_WIDTH + cx] = fb.read_pixel(sx, sy);
            }
        }
        for cy in 0..CURSOR_HEIGHT {
            let row = CURSOR_SPRITE[cy];
            for cx in 0..CURSOR_WIDTH {
                let sx = self.x as usize + cx;
                let sy = self.y as usize + cy;
                match row[cx] {
                    b'B' => fb.put_pixel(sx, sy, Color { r: 0, g: 0, b: 0 }),
                    b'W' => fb.put_pixel(sx, sy, Color { r: 255, g: 255, b: 255 }),
                    _ => {}
                }
            }
        }
        self.visible = true;
    }
}

#[repr(C)]
struct FramebufferInfo {
    addr: u64,
    width: u32,
    height: u32,
    stride: u32,
    pixel_format: u32,
}

fn read_fb_info(fd: u64) -> FramebufferInfo {
    let mut info = FramebufferInfo {
        addr: 0,
        width: 0,
        height: 0,
        stride: 0,
        pixel_format: 0,
    };
    let buf = unsafe {
        std::slice::from_raw_parts_mut(
            &mut info as *mut FramebufferInfo as *mut u8,
            std::mem::size_of::<FramebufferInfo>(),
        )
    };
    let n = io::read_fd(fd, buf);
    assert!(
        n == std::mem::size_of::<FramebufferInfo>(),
        "failed to read framebuffer info"
    );
    info
}

struct WindowState {
    pid: u32,
    buffer: Vec<u8>,
    content_x: usize,
    content_y: usize,
    width: usize,
    height: usize,
}

fn main() {
    io::register_name("compositor").expect("compositor already running");

    let kb_fd = io::open_device(io::DeviceType::Keyboard).expect("failed to claim keyboard");
    let mouse_fd = io::open_device(io::DeviceType::Mouse).expect("failed to claim mouse");
    let fb_fd = io::open_device(io::DeviceType::Framebuffer).expect("failed to claim framebuffer");

    let fb_info = read_fb_info(fb_fd);
    let screen = Framebuffer::new(
        fb_info.addr,
        fb_info.width,
        fb_info.height,
        fb_info.stride,
        fb_info.pixel_format,
    );

    let font_data = std::fs::read("/initrd/font.bin").expect("failed to read font");
    let font = font::Font::new(&font_data);

    screen.clear(DESKTOP_COLOR);

    let mut child = Command::new("/initrd/terminal")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn terminal");

    let _terminal_stdin = child.stdin.take().unwrap();
    let mut terminal_stdout = child.stdout.take().unwrap();
    let terminal_stdout_fd = terminal_stdout.as_raw_fd();

    let stdout = std::io::stdout();

    let mut windows: Vec<WindowState> = Vec::new();
    let screen_w = screen.width() as i32;
    let screen_h = screen.height() as i32;
    let mut cursor = Cursor::new(screen_w / 2, screen_h / 2);
    cursor.show(&screen);

    loop {
        let ready = io::poll(&[kb_fd, mouse_fd, terminal_stdout_fd]);

        if ready.fd(0) {
            let mut buf = [0u8; 64];
            let n = io::read_fd(kb_fd, &mut buf);
            if n > 0 {
                if let Some(win) = windows.first() {
                    let mut event = window::KeyEvent {
                        len: n as u8,
                        bytes: [0u8; 16],
                    };
                    event.bytes[..n.min(16)].copy_from_slice(&buf[..n.min(16)]);
                    message::send(win.pid, Message::new(window::MSG_KEY_INPUT, event));
                }
            }
        }

        if ready.fd(1) {
            let mut buf = [0u8; 3];
            let n = io::read_fd(mouse_fd, &mut buf);
            if n >= 3 {
                let dx = buf[1] as i8;
                let dy = buf[2] as i8;
                cursor.hide(&screen);
                cursor.x = (cursor.x + dx as i32).clamp(0, screen_w - 1);
                cursor.y = (cursor.y + dy as i32).clamp(0, screen_h - 1);
                cursor.show(&screen);
            }
        }

        if ready.fd(2) {
            let mut buf = [0u8; 4096];
            let n = terminal_stdout.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            stdout.lock().write_all(&buf[..n]).ok();
        }

        if ready.messages() {
            let msg = message::recv();
            let sender = msg.sender();
            match msg.msg_type() {
                window::MSG_CREATE_WINDOW => {
                    let _req: window::CreateWindowRequest = msg.take_payload();

                    let screen_w = screen.width();
                    let screen_h = screen.height();

                    let win_x = MARGIN;
                    let win_y = MARGIN;
                    let win_w = screen_w - MARGIN * 2;
                    let win_h = screen_h - MARGIN * 2;

                    let content_x = win_x + BORDER_WIDTH;
                    let content_y = win_y + BORDER_WIDTH + TITLE_BAR_HEIGHT;
                    let content_w = win_w - BORDER_WIDTH * 2;
                    let content_h = win_h - BORDER_WIDTH * 2 - TITLE_BAR_HEIGHT;

                    screen.fill_rect(win_x, win_y, win_w, win_h, BORDER_COLOR);
                    screen.fill_rect(
                        win_x + BORDER_WIDTH,
                        win_y + BORDER_WIDTH,
                        win_w - BORDER_WIDTH * 2,
                        TITLE_BAR_HEIGHT,
                        TITLE_BAR_COLOR,
                    );
                    let title_x = win_x + BORDER_WIDTH + 8;
                    let title_y = win_y + BORDER_WIDTH + (TITLE_BAR_HEIGHT - 16) / 2;
                    font.draw_string(&screen, title_x, title_y, "Terminal", TITLE_TEXT_COLOR, TITLE_BAR_COLOR);

                    let buffer = vec![0u8; content_w * content_h * 4];
                    let buffer_ptr = buffer.as_ptr() as *mut u8;
                    let pixel_format = screen.pixel_format_raw();

                    windows.push(WindowState {
                        pid: sender,
                        buffer,
                        content_x,
                        content_y,
                        width: content_w,
                        height: content_h,
                    });

                    message::send(sender, Message::new(
                        window::MSG_WINDOW_CREATED,
                        window::WindowInfo {
                            buffer: buffer_ptr,
                            width: content_w as u32,
                            height: content_h as u32,
                            stride: content_w as u32,
                            pixel_format,
                        },
                    ));
                }
                window::MSG_PRESENT => {
                    if let Some(win) = windows.iter().find(|w| w.pid == sender) {
                        cursor.hide(&screen);
                        screen.blit(
                            win.content_x,
                            win.content_y,
                            win.width,
                            win.height,
                            &win.buffer,
                        );
                        cursor.show(&screen);
                    }
                }
                _ => {}
            }
        }
    }

    drop(_terminal_stdin);
    drop(terminal_stdout);
    child.wait().ok();
}

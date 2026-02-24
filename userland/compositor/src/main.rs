mod font;
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

#[repr(C)]
struct FramebufferInfo {
    addr: u64,
    width: u32,
    height: u32,
    stride: u32,
    pixel_format: u32,
}

fn read_fb_info() -> FramebufferInfo {
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
    let n = io::read_fd(3, buf);
    assert!(
        n == std::mem::size_of::<FramebufferInfo>(),
        "Failed to read framebuffer info from FD 3"
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
    io::set_stdin_raw(true);

    let fb_info = read_fb_info();
    let screen = Framebuffer::new(
        fb_info.addr,
        fb_info.width,
        fb_info.height,
        fb_info.stride,
        fb_info.pixel_format,
    );

    let font = font::Font::new(include_bytes!(concat!(env!("OUT_DIR"), "/font.bin")));

    screen.clear(DESKTOP_COLOR);

    let mut child = Command::new("/initrd/terminal")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn terminal");

    let _terminal_stdin = child.stdin.take().unwrap();
    let mut terminal_stdout = child.stdout.take().unwrap();
    let terminal_stdout_fd = terminal_stdout.as_raw_fd();

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    let mut windows: Vec<WindowState> = Vec::new();

    loop {
        let ready = io::poll(&[0, terminal_stdout_fd]);

        if ready.fd(0) {
            let mut buf = [0u8; 64];
            let n = stdin.lock().read(&mut buf).unwrap_or(0);
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
                        screen.blit(
                            win.content_x,
                            win.content_y,
                            win.width,
                            win.height,
                            &win.buffer,
                        );
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

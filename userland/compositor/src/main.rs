mod framebuffer;

use std::io::{Read, Write};
use std::os::toyos::io::{self, AsRawFd};
use std::os::toyos::message::{self, Message};
use std::process::{Command, Stdio};

use framebuffer::{Color, CursorImage, Framebuffer};

const TITLE_BAR_HEIGHT: usize = 28;
const BORDER_WIDTH: usize = 1;
const RESIZE_HANDLE_SIZE: usize = 16;
const MIN_CONTENT_WIDTH: usize = 200;
const MIN_CONTENT_HEIGHT: usize = 100;
const INITIAL_MARGIN: usize = 40;

const DESKTOP_COLOR: Color = Color { r: 0x2d, g: 0x2d, b: 0x2d };
const TITLE_BAR_COLOR: Color = Color { r: 0x33, g: 0x33, b: 0x33 };
const BORDER_COLOR: Color = Color { r: 0x55, g: 0x55, b: 0x55 };
const TITLE_TEXT_COLOR: Color = Color { r: 0xcc, g: 0xcc, b: 0xcc };

#[repr(C)]
struct FramebufferInfo {
    token: [u32; 2],
    width: u32,
    height: u32,
    stride: u32,
    pixel_format: u32,
}

fn read_fb_info(fd: u64) -> FramebufferInfo {
    let mut info = FramebufferInfo {
        token: [0; 2],
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
    token: u32,
    buffer: *mut u8,
    buffer_size: usize,
    content_x: usize,
    content_y: usize,
    width: usize,
    height: usize,
    buf_width: usize,
    buf_height: usize,
}

enum HitZone {
    Desktop,
    TitleBar(usize),
    ResizeCorner(usize),
}

fn hit_test(windows: &[WindowState], x: i32, y: i32) -> HitZone {
    for (idx, win) in windows.iter().enumerate().rev() {
        let win_x = win.content_x as i32 - BORDER_WIDTH as i32;
        let win_y = win.content_y as i32 - BORDER_WIDTH as i32 - TITLE_BAR_HEIGHT as i32;
        let win_w = win.width as i32 + BORDER_WIDTH as i32 * 2;
        let win_h = win.height as i32 + BORDER_WIDTH as i32 * 2 + TITLE_BAR_HEIGHT as i32;

        if x >= win_x && x < win_x + win_w && y >= win_y && y < win_y + win_h {
            // Bottom-right corner = resize handle
            let corner_x = win_x + win_w - RESIZE_HANDLE_SIZE as i32;
            let corner_y = win_y + win_h - RESIZE_HANDLE_SIZE as i32;
            if x >= corner_x && y >= corner_y {
                return HitZone::ResizeCorner(idx);
            }
            // Title bar = drag handle
            let title_y_end = win_y + BORDER_WIDTH as i32 + TITLE_BAR_HEIGHT as i32;
            if y < title_y_end {
                return HitZone::TitleBar(idx);
            }
            return HitZone::Desktop;
        }
    }
    HitZone::Desktop
}

enum Interaction {
    None,
    Dragging { window_idx: usize },
    Resizing { window_idx: usize },
}

fn redraw(
    screen: &Framebuffer,
    font: &font::Font,
    windows: &[WindowState],
    cursor_x: i32,
    cursor_y: i32,
    cursor: &CursorImage,
) {
    screen.clear(DESKTOP_COLOR);

    for win in windows {
        let win_x = win.content_x - BORDER_WIDTH;
        let win_y = win.content_y - BORDER_WIDTH - TITLE_BAR_HEIGHT;
        let win_w = win.width + BORDER_WIDTH * 2;
        let win_h = win.height + BORDER_WIDTH * 2 + TITLE_BAR_HEIGHT;

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
        font.draw_string(screen, title_x, title_y, "Terminal", TITLE_TEXT_COLOR, TITLE_BAR_COLOR);

        // Blit content, clipped to buffer dimensions
        let blit_w = win.width.min(win.buf_width);
        let blit_h = win.height.min(win.buf_height);
        let buffer_slice = unsafe { std::slice::from_raw_parts(win.buffer, win.buffer_size) };
        screen.blit(win.content_x, win.content_y, blit_w, blit_h, win.buf_width, buffer_slice);
    }

    screen.draw_cursor(cursor_x, cursor_y, cursor);
}

fn main() {
    io::register_name("compositor").expect("compositor already running");

    let kb_fd = io::open_device(io::DeviceType::Keyboard).expect("failed to claim keyboard");
    let mouse_fd = io::open_device(io::DeviceType::Mouse).expect("failed to claim mouse");
    let fb_fd = io::open_device(io::DeviceType::Framebuffer).expect("failed to claim framebuffer");

    let fb_info = read_fb_info(fb_fd);
    let fb_addrs = [
        io::map_shared(fb_info.token[0]) as u64,
        io::map_shared(fb_info.token[1]) as u64,
    ];
    let mut screen = Framebuffer::new(
        fb_addrs,
        fb_info.width,
        fb_info.height,
        fb_info.stride,
        fb_info.pixel_format,
    );

    let font_data = std::fs::read("/initrd/font.bin").expect("failed to read font");
    let font = font::Font::new(&font_data);
    let cursor_data = std::fs::read("/initrd/cursor.bin").expect("failed to read cursor");
    let cursor = CursorImage::new(&cursor_data);

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
    let mut cursor_x = screen_w / 2;
    let mut cursor_y = screen_h / 2;
    let mut dirty = true;
    let mut prev_buttons: u8 = 0;
    let mut interaction = Interaction::None;

    loop {
        if dirty {
            redraw(&screen, &font, &windows, cursor_x, cursor_y, &cursor);
            io::gpu_present();
            screen.swap();
            dirty = false;
        }

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
                let buttons = buf[0];
                let dx = buf[1] as i8;
                let dy = buf[2] as i8;

                cursor_x = (cursor_x + dx as i32).clamp(0, screen_w - 1);
                cursor_y = (cursor_y + dy as i32).clamp(0, screen_h - 1);

                let left = buttons & 1 != 0;
                let was_left = prev_buttons & 1 != 0;

                // Left button just pressed — start interaction
                if left && !was_left {
                    match hit_test(&windows, cursor_x, cursor_y) {
                        HitZone::TitleBar(idx) => {
                            interaction = Interaction::Dragging { window_idx: idx };
                        }
                        HitZone::ResizeCorner(idx) => {
                            interaction = Interaction::Resizing { window_idx: idx };
                        }
                        HitZone::Desktop => {}
                    }
                }

                // Apply movement while button held
                if left {
                    match interaction {
                        Interaction::Dragging { window_idx } => {
                            let win = &mut windows[window_idx];
                            let min_x = BORDER_WIDTH as i32;
                            let min_y = (BORDER_WIDTH + TITLE_BAR_HEIGHT) as i32;
                            win.content_x = (win.content_x as i32 + dx as i32).max(min_x) as usize;
                            win.content_y = (win.content_y as i32 + dy as i32).max(min_y) as usize;
                        }
                        Interaction::Resizing { window_idx } => {
                            let win = &mut windows[window_idx];
                            win.width = (win.width as i32 + dx as i32)
                                .max(MIN_CONTENT_WIDTH as i32) as usize;
                            win.height = (win.height as i32 + dy as i32)
                                .max(MIN_CONTENT_HEIGHT as i32) as usize;
                        }
                        Interaction::None => {}
                    }
                }

                // Left button released — finalize interaction
                if !left && was_left {
                    if let Interaction::Resizing { window_idx } = interaction {
                        let win = &mut windows[window_idx];
                        io::free_shared(win.token);
                        let buf_size = win.width * win.height * 4;
                        let token = io::alloc_shared(buf_size);
                        let buffer = io::map_shared(token);
                        io::grant_shared(token, win.pid);
                        win.token = token;
                        win.buffer = buffer;
                        win.buffer_size = buf_size;
                        win.buf_width = win.width;
                        win.buf_height = win.height;
                        let pixel_format = screen.pixel_format_raw();
                        message::send(win.pid, Message::new(
                            window::MSG_WINDOW_RESIZED,
                            window::WindowInfo {
                                token,
                                width: win.width as u32,
                                height: win.height as u32,
                                stride: win.width as u32,
                                pixel_format,
                            },
                        ));
                    }
                    interaction = Interaction::None;
                }

                prev_buttons = buttons;
                dirty = true;
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

                    let win_x = INITIAL_MARGIN;
                    let win_y = INITIAL_MARGIN;
                    let win_w = screen_w - INITIAL_MARGIN * 2;
                    let win_h = screen_h - INITIAL_MARGIN * 2;

                    let content_x = win_x + BORDER_WIDTH;
                    let content_y = win_y + BORDER_WIDTH + TITLE_BAR_HEIGHT;
                    let content_w = win_w - BORDER_WIDTH * 2;
                    let content_h = win_h - BORDER_WIDTH * 2 - TITLE_BAR_HEIGHT;

                    let buf_size = content_w * content_h * 4;
                    let token = io::alloc_shared(buf_size);
                    let buffer = io::map_shared(token);
                    io::grant_shared(token, sender);
                    let pixel_format = screen.pixel_format_raw();

                    windows.push(WindowState {
                        pid: sender,
                        token,
                        buffer,
                        buffer_size: buf_size,
                        content_x,
                        content_y,
                        width: content_w,
                        height: content_h,
                        buf_width: content_w,
                        buf_height: content_h,
                    });

                    message::send(sender, Message::new(
                        window::MSG_WINDOW_CREATED,
                        window::WindowInfo {
                            token,
                            width: content_w as u32,
                            height: content_h as u32,
                            stride: content_w as u32,
                            pixel_format,
                        },
                    ));
                    dirty = true;
                }
                window::MSG_PRESENT => {
                    if let Some(_win) = windows.iter().find(|w| w.pid == sender) {
                        dirty = true;
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

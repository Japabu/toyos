mod framebuffer;

use std::os::toyos::io;
use std::os::toyos::message::{self, Message};
use std::process::Command;

use framebuffer::{Color, Framebuffer};

const TITLE_BAR_HEIGHT: usize = 28;
const BORDER_WIDTH: usize = 1;
const RESIZE_HANDLE_SIZE: usize = 16;
const CLOSE_BUTTON_WIDTH: usize = 28;
const MIN_CONTENT_WIDTH: usize = 200;
const MIN_CONTENT_HEIGHT: usize = 100;
const INITIAL_MARGIN: usize = 40;
const CASCADE_OFFSET: usize = 30;
const TASKBAR_HEIGHT: usize = 32;
const TASKBAR_ITEM_WIDTH: usize = 160;
const TASKBAR_PADDING: usize = 4;

const DESKTOP_COLOR: Color = Color { r: 0x1a, g: 0x1a, b: 0x2e };
const FOCUSED_TITLE_COLOR: Color = Color { r: 0x3a, g: 0x3a, b: 0x4e };
const UNFOCUSED_TITLE_COLOR: Color = Color { r: 0x28, g: 0x28, b: 0x32 };
const FOCUSED_BORDER_COLOR: Color = Color { r: 0x58, g: 0x58, b: 0x6e };
const UNFOCUSED_BORDER_COLOR: Color = Color { r: 0x38, g: 0x38, b: 0x42 };
const FOCUSED_TITLE_TEXT: Color = Color { r: 0xe0, g: 0xe0, b: 0xe8 };
const UNFOCUSED_TITLE_TEXT: Color = Color { r: 0x60, g: 0x60, b: 0x70 };
const CLOSE_BUTTON_COLOR: Color = Color { r: 0xc0, g: 0x40, b: 0x40 };
const CLOSE_BUTTON_BG: Color = Color { r: 0x50, g: 0x28, b: 0x28 };
const TASKBAR_COLOR: Color = Color { r: 0x18, g: 0x18, b: 0x25 };
const TASKBAR_ACTIVE_COLOR: Color = Color { r: 0x30, g: 0x30, b: 0x45 };
const TASKBAR_TEXT_COLOR: Color = Color { r: 0x80, g: 0x80, b: 0x90 };
const TASKBAR_ACTIVE_TEXT: Color = Color { r: 0xe0, g: 0xe0, b: 0xe8 };
const TASKBAR_NEW_COLOR: Color = Color { r: 0x40, g: 0x60, b: 0x40 };
const TASKBAR_NEW_TEXT: Color = Color { r: 0x80, g: 0xc0, b: 0x80 };

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
    title: String,
}

enum HitZone {
    Desktop,
    TitleBar(usize),
    CloseButton(usize),
    Content(usize),
    ResizeCorner(usize),
    TaskbarItem(usize),
    TaskbarNew,
}

fn hit_test(windows: &[WindowState], x: i32, y: i32, screen_w: i32, screen_h: i32) -> HitZone {
    // Taskbar at bottom of screen
    if y >= screen_h - TASKBAR_HEIGHT as i32 {
        let new_x = screen_w - TASKBAR_HEIGHT as i32;
        if x >= new_x {
            return HitZone::TaskbarNew;
        }
        let tab_x = x as usize / TASKBAR_ITEM_WIDTH;
        if tab_x < windows.len() {
            return HitZone::TaskbarItem(tab_x);
        }
        return HitZone::Desktop;
    }

    for (idx, win) in windows.iter().enumerate().rev() {
        let win_x = win.content_x as i32 - BORDER_WIDTH as i32;
        let win_y = win.content_y as i32 - BORDER_WIDTH as i32 - TITLE_BAR_HEIGHT as i32;
        let win_w = win.width as i32 + BORDER_WIDTH as i32 * 2;
        let win_h = win.height as i32 + BORDER_WIDTH as i32 * 2 + TITLE_BAR_HEIGHT as i32;

        if x >= win_x && x < win_x + win_w && y >= win_y && y < win_y + win_h {
            // Close button (right side of title bar)
            let close_x = win_x + win_w - BORDER_WIDTH as i32 - CLOSE_BUTTON_WIDTH as i32;
            let title_y_end = win_y + BORDER_WIDTH as i32 + TITLE_BAR_HEIGHT as i32;
            if x >= close_x && y < title_y_end {
                return HitZone::CloseButton(idx);
            }
            // Bottom-right corner = resize handle
            let corner_x = win_x + win_w - RESIZE_HANDLE_SIZE as i32;
            let corner_y = win_y + win_h - RESIZE_HANDLE_SIZE as i32;
            if x >= corner_x && y >= corner_y {
                return HitZone::ResizeCorner(idx);
            }
            // Title bar = drag handle
            if y < title_y_end {
                return HitZone::TitleBar(idx);
            }
            return HitZone::Content(idx);
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
    cursor: &sprite::Sprite,
) {
    screen.clear(DESKTOP_COLOR);

    let focused_idx = windows.len().wrapping_sub(1);

    for (i, win) in windows.iter().enumerate() {
        let focused = i == focused_idx;
        let border_color = if focused { FOCUSED_BORDER_COLOR } else { UNFOCUSED_BORDER_COLOR };
        let title_color = if focused { FOCUSED_TITLE_COLOR } else { UNFOCUSED_TITLE_COLOR };
        let text_color = if focused { FOCUSED_TITLE_TEXT } else { UNFOCUSED_TITLE_TEXT };

        let win_x = win.content_x - BORDER_WIDTH;
        let win_y = win.content_y - BORDER_WIDTH - TITLE_BAR_HEIGHT;
        let win_w = win.width + BORDER_WIDTH * 2;
        let win_h = win.height + BORDER_WIDTH * 2 + TITLE_BAR_HEIGHT;

        screen.fill_rect(win_x, win_y, win_w, win_h, border_color);
        screen.fill_rect(
            win_x + BORDER_WIDTH,
            win_y + BORDER_WIDTH,
            win_w - BORDER_WIDTH * 2,
            TITLE_BAR_HEIGHT,
            title_color,
        );

        // Title text
        let title_x = win_x + BORDER_WIDTH + 8;
        let title_y = win_y + BORDER_WIDTH + (TITLE_BAR_HEIGHT - 16) / 2;
        let title = if win.title.is_empty() { "Window" } else { &win.title };
        font.draw_string(screen, title_x, title_y, title, text_color, title_color);

        // Close button
        let close_x = win_x + win_w - BORDER_WIDTH - CLOSE_BUTTON_WIDTH;
        let close_bg = if focused { CLOSE_BUTTON_BG } else { title_color };
        let close_fg = if focused { CLOSE_BUTTON_COLOR } else { UNFOCUSED_TITLE_TEXT };
        screen.fill_rect(close_x, win_y + BORDER_WIDTH, CLOSE_BUTTON_WIDTH, TITLE_BAR_HEIGHT, close_bg);
        let x_char_x = close_x + (CLOSE_BUTTON_WIDTH - 8) / 2;
        let x_char_y = win_y + BORDER_WIDTH + (TITLE_BAR_HEIGHT - 16) / 2;
        font.draw_char(screen, x_char_x, x_char_y, 'X', close_fg, close_bg);

        // Blit content, clipped to buffer dimensions
        let blit_w = win.width.min(win.buf_width);
        let blit_h = win.height.min(win.buf_height);
        let buffer_slice = unsafe { std::slice::from_raw_parts(win.buffer, win.buffer_size) };
        screen.blit(win.content_x, win.content_y, blit_w, blit_h, win.buf_width, buffer_slice);
    }

    // Taskbar
    let screen_w = screen.width();
    let screen_h = screen.height();
    let taskbar_y = screen_h - TASKBAR_HEIGHT;
    screen.fill_rect(0, taskbar_y, screen_w, TASKBAR_HEIGHT, TASKBAR_COLOR);

    for (i, _win) in windows.iter().enumerate() {
        let focused = i == focused_idx;
        let tab_x = i * TASKBAR_ITEM_WIDTH;
        let bg = if focused { TASKBAR_ACTIVE_COLOR } else { TASKBAR_COLOR };
        let fg = if focused { TASKBAR_ACTIVE_TEXT } else { TASKBAR_TEXT_COLOR };
        screen.fill_rect(
            tab_x + 1,
            taskbar_y + TASKBAR_PADDING,
            TASKBAR_ITEM_WIDTH - 2,
            TASKBAR_HEIGHT - TASKBAR_PADDING * 2,
            bg,
        );
        let text_x = tab_x + 8;
        let text_y = taskbar_y + (TASKBAR_HEIGHT - 16) / 2;
        let max_chars = (TASKBAR_ITEM_WIDTH - 16) / font.width();
        let title = if _win.title.is_empty() { "Window" } else { &_win.title };
        let display: String = title.chars().take(max_chars).collect();
        font.draw_string(screen, text_x, text_y, &display, fg, bg);
    }

    // "+" button on right
    let new_x = screen_w - TASKBAR_HEIGHT;
    screen.fill_rect(
        new_x + 1,
        taskbar_y + TASKBAR_PADDING,
        TASKBAR_HEIGHT - 2,
        TASKBAR_HEIGHT - TASKBAR_PADDING * 2,
        TASKBAR_NEW_COLOR,
    );
    let plus_x = new_x + (TASKBAR_HEIGHT - 8) / 2;
    let plus_y = taskbar_y + (TASKBAR_HEIGHT - 16) / 2;
    font.draw_char(screen, plus_x, plus_y, '+', TASKBAR_NEW_TEXT, TASKBAR_NEW_COLOR);

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

    let ttf_data = std::fs::read("/initrd/JetBrainsMono-Regular.ttf").expect("failed to read font");
    let font = font::Font::new(&ttf_data, 8, 16);
    let cursor_svg = std::fs::read("/initrd/cursor-bold.svg").expect("failed to read cursor");
    let cursor_default = sprite::Sprite::from_svg_colored(&cursor_svg, 20, [255, 255, 255]);
    let resize_svg = std::fs::read("/initrd/arrow-down-right-bold.svg").expect("failed to read resize cursor");
    let cursor_resize = sprite::Sprite::from_svg_colored(&resize_svg, 20, [255, 255, 255]);

    Command::new("/initrd/terminal")
        .spawn()
        .expect("failed to spawn terminal");

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
            let active_cursor = match interaction {
                Interaction::Resizing { .. } => &cursor_resize,
                _ => match hit_test(&windows, cursor_x, cursor_y, screen_w, screen_h) {
                    HitZone::ResizeCorner(_) => &cursor_resize,
                    _ => &cursor_default,
                },
            };
            redraw(&screen, &font, &windows, cursor_x, cursor_y, active_cursor);
            io::gpu_present();
            screen.swap();
            dirty = false;
        }

        let ready = io::poll(&[kb_fd, mouse_fd]);

        if ready.fd(0) {
            let mut events = [window::KeyEvent::EMPTY; 8];
            let buf = unsafe {
                std::slice::from_raw_parts_mut(
                    events.as_mut_ptr() as *mut u8,
                    std::mem::size_of_val(&events),
                )
            };
            let n = io::read_fd(kb_fd, buf);
            for event in &events[..n / std::mem::size_of::<window::KeyEvent>()] {
                if event.alt() && event.keycode == 0x2B {
                    // Alt+Tab: rotate focus
                    if windows.len() > 1 {
                        let win = windows.pop().unwrap();
                        windows.insert(0, win);
                        dirty = true;
                    }
                } else if event.ctrl() && event.keycode == 0x11 {
                    // Ctrl+N: spawn terminal
                    Command::new("/initrd/terminal").spawn().ok();
                } else if event.len > 0 {
                    if let Some(win) = windows.last() {
                        message::send(win.pid, Message::new(window::MSG_KEY_INPUT, *event));
                    }
                }
            }
        }

        if ready.fd(1) {
            let mut buf = [0u8; 4];
            let n = io::read_fd(mouse_fd, &mut buf);
            if n >= 3 {
                let buttons = buf[0];
                let dx = buf[1] as i8;
                let dy = buf[2] as i8;
                let scroll = if n >= 4 { buf[3] as i8 } else { 0 };

                cursor_x = (cursor_x + dx as i32).clamp(0, screen_w - 1);
                cursor_y = (cursor_y + dy as i32).clamp(0, screen_h - 1);

                let left = buttons & 1 != 0;
                let was_left = prev_buttons & 1 != 0;

                // Helper to build a MouseEvent for the focused window
                let make_mouse_event = |win: &WindowState, event_type: u8, changed: u8, scroll: i8| {
                    let local_x = (cursor_x - win.content_x as i32).max(0) as u16;
                    let local_y = (cursor_y - win.content_y as i32).max(0) as u16;
                    window::MouseEvent {
                        x: local_x,
                        y: local_y,
                        buttons,
                        event_type,
                        changed,
                        scroll,
                    }
                };

                // Left button just pressed — start interaction
                if left && !was_left {
                    match hit_test(&windows, cursor_x, cursor_y, screen_w, screen_h) {
                        HitZone::CloseButton(idx) => {
                            let win = windows.remove(idx);
                            message::send(win.pid, Message::signal(window::MSG_WINDOW_CLOSE));
                        }
                        HitZone::TitleBar(idx) => {
                            let win = windows.remove(idx);
                            windows.push(win);
                            let new_idx = windows.len() - 1;
                            interaction = Interaction::Dragging { window_idx: new_idx };
                        }
                        HitZone::ResizeCorner(idx) => {
                            let win = windows.remove(idx);
                            windows.push(win);
                            let new_idx = windows.len() - 1;
                            interaction = Interaction::Resizing { window_idx: new_idx };
                        }
                        HitZone::Content(idx) => {
                            if idx != windows.len() - 1 {
                                let win = windows.remove(idx);
                                windows.push(win);
                            }
                            let win = windows.last().unwrap();
                            let ev = make_mouse_event(win, window::MOUSE_PRESS, 1, 0);
                            message::send(win.pid, Message::new(window::MSG_MOUSE_INPUT, ev));
                        }
                        HitZone::TaskbarItem(idx) => {
                            if idx < windows.len() && idx != windows.len() - 1 {
                                let win = windows.remove(idx);
                                windows.push(win);
                            }
                        }
                        HitZone::TaskbarNew => {
                            Command::new("/initrd/terminal").spawn().ok();
                        }
                        HitZone::Desktop => {}
                    }
                }

                // Left button released — send release to focused window + finalize interaction
                if !left && was_left {
                    if let Some(win) = windows.last() {
                        let ev = make_mouse_event(win, window::MOUSE_RELEASE, 1, 0);
                        message::send(win.pid, Message::new(window::MSG_MOUSE_INPUT, ev));
                    }
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

                // Apply movement while button held (drag/resize)
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

                // Scroll events — forward to focused window
                if scroll != 0 {
                    if let Some(win) = windows.last() {
                        if let HitZone::Content(_) = hit_test(&windows, cursor_x, cursor_y, screen_w, screen_h) {
                            let ev = make_mouse_event(win, window::MOUSE_SCROLL, 0, scroll);
                            message::send(win.pid, Message::new(window::MSG_MOUSE_INPUT, ev));
                        }
                    }
                }

                prev_buttons = buttons;
                dirty = true;
            }
        }

        if ready.messages() {
            let msg = message::recv();
            let sender = msg.sender();
            match msg.msg_type() {
                window::MSG_CREATE_WINDOW => {
                    let req: window::CreateWindowRequest = msg.take_payload();
                    let title = if req.title_len > 0 {
                        let len = (req.title_len as usize).min(31);
                        String::from_utf8_lossy(&req.title[..len]).into_owned()
                    } else {
                        String::new()
                    };

                    let screen_w = screen.width();
                    let screen_h = screen.height();

                    let offset = CASCADE_OFFSET * (windows.len() % 10);
                    let win_x = INITIAL_MARGIN + offset;
                    let win_y = INITIAL_MARGIN + offset;
                    let win_w = screen_w - INITIAL_MARGIN * 2;
                    let win_h = screen_h - INITIAL_MARGIN * 2 - TASKBAR_HEIGHT;

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
                        title,
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
}

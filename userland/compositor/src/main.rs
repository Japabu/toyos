use std::os::toyos::message::{self, Message};
use toyos_abi::syscall;
use std::process::Command;

use window::{Color, Framebuffer};

const TITLE_BAR_HEIGHT: usize = 28;
const BORDER_WIDTH: usize = 1;
const RESIZE_HANDLE_SIZE: usize = 16;
const BUTTON_WIDTH: usize = 28;
const MIN_CONTENT_WIDTH: usize = 200;
const MIN_CONTENT_HEIGHT: usize = 100;
const INITIAL_MARGIN: usize = 40;
const CASCADE_OFFSET: usize = 30;
const TASKBAR_HEIGHT: usize = 32;
const TASKBAR_ITEM_WIDTH: usize = 160;
const TASKBAR_PADDING: usize = 4;
const DOUBLE_CLICK_NS: u64 = 400_000_000;
const FRAME_INTERVAL_NS: u64 = 16_666_667; // ~60fps

const FOCUSED_TITLE_COLOR: Color = Color { r: 0x3a, g: 0x3a, b: 0x4e };
const UNFOCUSED_TITLE_COLOR: Color = Color { r: 0x28, g: 0x28, b: 0x32 };
const FOCUSED_BORDER_COLOR: Color = Color { r: 0x58, g: 0x58, b: 0x6e };
const UNFOCUSED_BORDER_COLOR: Color = Color { r: 0x38, g: 0x38, b: 0x42 };
const FOCUSED_TITLE_TEXT: Color = Color { r: 0xe0, g: 0xe0, b: 0xe8 };
const UNFOCUSED_TITLE_TEXT: Color = Color { r: 0x60, g: 0x60, b: 0x70 };
const CLOSE_BUTTON_BG: Color = Color { r: 0x50, g: 0x28, b: 0x28 };
const TASKBAR_COLOR: Color = Color { r: 0x18, g: 0x18, b: 0x25 };
const TASKBAR_ACTIVE_COLOR: Color = Color { r: 0x30, g: 0x30, b: 0x45 };
const TASKBAR_TEXT_COLOR: Color = Color { r: 0x80, g: 0x80, b: 0x90 };
const TASKBAR_ACTIVE_TEXT: Color = Color { r: 0xe0, g: 0xe0, b: 0xe8 };
const TASKBAR_NEW_COLOR: Color = Color { r: 0x40, g: 0x60, b: 0x40 };
const TASKBAR_NEW_TEXT: Color = Color { r: 0x80, g: 0xc0, b: 0x80 };
const TASKBAR_MINIMIZED_COLOR: Color = Color { r: 0x20, g: 0x20, b: 0x30 };
const TASKBAR_MINIMIZED_TEXT: Color = Color { r: 0x50, g: 0x50, b: 0x60 };
const LAUNCHER_WIDTH: usize = 160;
const LAUNCHER_ITEM_HEIGHT: usize = 28;
const LAUNCHER_BG: Color = Color { r: 0x20, g: 0x20, b: 0x30 };
const LAUNCHER_TEXT: Color = Color { r: 0xe0, g: 0xe0, b: 0xe8 };

struct LauncherEntry {
    name: &'static str,
    path: &'static str,
}

const LAUNCHER_APPS: &[LauncherEntry] = &[
    LauncherEntry { name: "Terminal", path: "/initrd/terminal" },
    LauncherEntry { name: "Files", path: "/initrd/files" },
    LauncherEntry { name: "Monitor", path: "/initrd/monitor" },
];

const FLAG_HARDWARE_CURSOR: u32 = 1 << 0;

#[repr(C)]
struct FramebufferInfo {
    token: [u32; 2],
    cursor_token: u32,
    width: u32,
    height: u32,
    stride: u32,
    pixel_format: u32,
    flags: u32,
}

fn read_fb_info(fd: u64) -> FramebufferInfo {
    let mut info = FramebufferInfo {
        token: [0; 2],
        cursor_token: 0,
        width: 0,
        height: 0,
        stride: 0,
        pixel_format: 0,
        flags: 0,
    };
    let buf = unsafe {
        std::slice::from_raw_parts_mut(
            &mut info as *mut FramebufferInfo as *mut u8,
            std::mem::size_of::<FramebufferInfo>(),
        )
    };
    let n = syscall::read_fd(fd, buf);
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
    minimized: bool,
    topmost: bool,
    mode: WindowMode,
    saved_x: usize,
    saved_y: usize,
    saved_w: usize,
    saved_h: usize,
    presented: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum WindowMode {
    Normal,
    Maximized,
    SnappedLeft,
    SnappedRight,
}

struct TitleBarIcons {
    minimize: sprite::Sprite,
    maximize: sprite::Sprite,
    close: sprite::Sprite,
}

enum HitZone {
    Desktop,
    TitleBar(usize),
    MinimizeButton(usize),
    MaximizeButton(usize),
    CloseButton(usize),
    Content(usize),
    ResizeCorner(usize),
    TaskbarItem(usize),
    TaskbarNew,
    LauncherItem(usize),
}

fn focused_window_idx(windows: &[WindowState]) -> Option<usize> {
    windows
        .iter()
        .enumerate()
        .rev()
        .find(|(_, w)| !w.minimized)
        .map(|(i, _)| i)
}

/// Bring window at `idx` to front, keeping topmost windows always on top.
/// Returns the new index of the moved window.
fn bring_to_front(windows: &mut Vec<WindowState>, idx: usize) -> usize {
    if idx == windows.len() - 1 {
        return idx;
    }
    let win = windows.remove(idx);
    if win.topmost {
        // Topmost windows go to the very end
        windows.push(win);
        windows.len() - 1
    } else {
        // Non-topmost windows go before the first topmost window
        let insert_at = windows.iter().position(|w| w.topmost).unwrap_or(windows.len());
        windows.insert(insert_at, win);
        insert_at
    }
}

fn resize_window(win: &mut WindowState, new_w: usize, new_h: usize, pixel_format: u32) {
    let old_token = win.token;
    let buf_size = new_w * new_h * 4;
    let token = syscall::alloc_shared(buf_size);
    let buffer = syscall::map_shared(token);
    syscall::grant_shared(token, win.pid);
    win.token = token;
    win.buffer = buffer;
    win.buffer_size = buf_size;
    win.width = new_w;
    win.height = new_h;
    win.buf_width = new_w;
    win.buf_height = new_h;
    message::send(
        win.pid,
        Message::new(
            window::MSG_WINDOW_RESIZED,
            window::ResizeInfo {
                token,
                old_token,
                width: new_w as u32,
                height: new_h as u32,
                stride: new_w as u32,
                pixel_format,
            },
        ),
    ).ok();
    syscall::release_shared(old_token);
}

fn save_if_normal(win: &mut WindowState) {
    if win.mode == WindowMode::Normal {
        win.saved_x = win.content_x;
        win.saved_y = win.content_y;
        win.saved_w = win.width;
        win.saved_h = win.height;
    }
}

fn maximize_window(win: &mut WindowState, screen_w: usize, screen_h: usize, pixel_format: u32) {
    save_if_normal(win);
    win.mode = WindowMode::Maximized;
    win.content_x = BORDER_WIDTH;
    win.content_y = BORDER_WIDTH + TITLE_BAR_HEIGHT;
    let new_w = screen_w - BORDER_WIDTH * 2;
    let new_h = screen_h - TASKBAR_HEIGHT - BORDER_WIDTH * 2 - TITLE_BAR_HEIGHT;
    resize_window(win, new_w, new_h, pixel_format);
}

fn snap_left(win: &mut WindowState, screen_w: usize, screen_h: usize, pixel_format: u32) {
    save_if_normal(win);
    win.mode = WindowMode::SnappedLeft;
    win.content_x = BORDER_WIDTH;
    win.content_y = BORDER_WIDTH + TITLE_BAR_HEIGHT;
    let new_w = screen_w / 2 - BORDER_WIDTH * 2;
    let new_h = screen_h - TASKBAR_HEIGHT - BORDER_WIDTH * 2 - TITLE_BAR_HEIGHT;
    resize_window(win, new_w, new_h, pixel_format);
}

fn snap_right(win: &mut WindowState, screen_w: usize, screen_h: usize, pixel_format: u32) {
    save_if_normal(win);
    win.mode = WindowMode::SnappedRight;
    win.content_x = screen_w / 2 + BORDER_WIDTH;
    win.content_y = BORDER_WIDTH + TITLE_BAR_HEIGHT;
    let new_w = screen_w / 2 - BORDER_WIDTH * 2;
    let new_h = screen_h - TASKBAR_HEIGHT - BORDER_WIDTH * 2 - TITLE_BAR_HEIGHT;
    resize_window(win, new_w, new_h, pixel_format);
}

fn restore_window(win: &mut WindowState, pixel_format: u32) {
    win.mode = WindowMode::Normal;
    win.content_x = win.saved_x;
    win.content_y = win.saved_y;
    let w = win.saved_w;
    let h = win.saved_h;
    resize_window(win, w, h, pixel_format);
}

/// Scale RGB image and convert to native framebuffer pixel format (4 bytes/pixel).
fn scale_wallpaper(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    bgr: bool,
) -> Vec<u8> {
    let mut dst = vec![0u8; dst_w * dst_h * 4];
    for y in 0..dst_h {
        let sy = y * src_h / dst_h;
        for x in 0..dst_w {
            let sx = x * src_w / dst_w;
            let si = (sy * src_w + sx) * 3;
            let di = (y * dst_w + x) * 4;
            if bgr {
                dst[di] = src[si + 2];
                dst[di + 1] = src[si + 1];
                dst[di + 2] = src[si];
            } else {
                dst[di] = src[si];
                dst[di + 1] = src[si + 1];
                dst[di + 2] = src[si + 2];
            }
        }
    }
    dst
}

fn draw_icon_centered(
    screen: &Framebuffer,
    icon: &sprite::Sprite,
    area_x: usize,
    area_y: usize,
    area_w: usize,
    area_h: usize,
) {
    let ix = area_x + area_w.saturating_sub(icon.width()) / 2;
    let iy = area_y + area_h.saturating_sub(icon.height()) / 2;
    icon.draw(screen.ptr(), screen.stride(), screen.width(), screen.height(), screen.pixel_format_raw(), ix, iy);
}

fn launcher_rect(windows: &[WindowState], screen_h: i32) -> (i32, i32, i32, i32) {
    let lx = (windows.len() * TASKBAR_ITEM_WIDTH) as i32;
    let lh = (LAUNCHER_APPS.len() * LAUNCHER_ITEM_HEIGHT) as i32;
    let ly = screen_h - TASKBAR_HEIGHT as i32 - lh;
    (lx, ly, LAUNCHER_WIDTH as i32, lh)
}

#[derive(Clone, Copy)]
struct DirtyRect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl DirtyRect {
    fn full(screen_w: usize, screen_h: usize) -> Self {
        Self { x: 0, y: 0, w: screen_w, h: screen_h }
    }

    fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.w).max(other.x + other.w);
        let bottom = (self.y + self.h).max(other.y + other.h);
        Self { x, y, w: right - x, h: bottom - y }
    }

    fn clamp(self, screen_w: usize, screen_h: usize) -> Self {
        let x = self.x.min(screen_w);
        let y = self.y.min(screen_h);
        let w = self.w.min(screen_w - x);
        let h = self.h.min(screen_h - y);
        Self { x, y, w, h }
    }

    fn overlaps(self, other: Self) -> bool {
        self.x < other.x + other.w && self.x + self.w > other.x
            && self.y < other.y + other.h && self.y + self.h > other.y
    }
}

fn window_screen_rect(win: &WindowState) -> DirtyRect {
    let x = win.content_x.saturating_sub(BORDER_WIDTH);
    let y = win.content_y.saturating_sub(BORDER_WIDTH + TITLE_BAR_HEIGHT);
    let w = win.width + BORDER_WIDTH * 2;
    let h = win.height + BORDER_WIDTH * 2 + TITLE_BAR_HEIGHT;
    DirtyRect { x, y, w, h }
}

fn mark_dirty(dirty_rect: &mut Option<DirtyRect>, r: DirtyRect) {
    *dirty_rect = Some(match dirty_rect.take() {
        Some(old) => old.union(r),
        None => r,
    });
}

fn hit_test(windows: &[WindowState], x: i32, y: i32, screen_h: i32, launcher_open: bool) -> HitZone {
    // Launcher popup
    if launcher_open {
        let (lx, ly, lw, lh) = launcher_rect(windows, screen_h);
        if x >= lx && x < lx + lw && y >= ly && y < ly + lh {
            let item = ((y - ly) / LAUNCHER_ITEM_HEIGHT as i32) as usize;
            if item < LAUNCHER_APPS.len() {
                return HitZone::LauncherItem(item);
            }
        }
    }

    // Taskbar at bottom of screen
    if y >= screen_h - TASKBAR_HEIGHT as i32 {
        let tab_x = x as usize / TASKBAR_ITEM_WIDTH;
        if tab_x < windows.len() {
            return HitZone::TaskbarItem(tab_x);
        }
        let new_x = windows.len() * TASKBAR_ITEM_WIDTH;
        if x >= new_x as i32 && x < (new_x + TASKBAR_HEIGHT) as i32 {
            return HitZone::TaskbarNew;
        }
        return HitZone::Desktop;
    }

    for (idx, win) in windows.iter().enumerate().rev() {
        if win.minimized {
            continue;
        }

        let win_x = win.content_x as i32 - BORDER_WIDTH as i32;
        let win_y = win.content_y as i32 - BORDER_WIDTH as i32 - TITLE_BAR_HEIGHT as i32;
        let win_w = win.width as i32 + BORDER_WIDTH as i32 * 2;
        let win_h = win.height as i32 + BORDER_WIDTH as i32 * 2 + TITLE_BAR_HEIGHT as i32;

        if x >= win_x && x < win_x + win_w && y >= win_y && y < win_y + win_h {
            let title_y_end = win_y + BORDER_WIDTH as i32 + TITLE_BAR_HEIGHT as i32;

            // Buttons from right: close, maximize, minimize
            let close_x = win_x + win_w - BORDER_WIDTH as i32 - BUTTON_WIDTH as i32;
            if x >= close_x && y < title_y_end {
                return HitZone::CloseButton(idx);
            }
            let max_x = close_x - BUTTON_WIDTH as i32;
            if x >= max_x && x < close_x && y < title_y_end {
                return HitZone::MaximizeButton(idx);
            }
            let min_x = max_x - BUTTON_WIDTH as i32;
            if x >= min_x && x < max_x && y < title_y_end {
                return HitZone::MinimizeButton(idx);
            }

            // Resize corner (not for snapped/maximized windows)
            if win.mode == WindowMode::Normal {
                let corner_x = win_x + win_w - RESIZE_HANDLE_SIZE as i32;
                let corner_y = win_y + win_h - RESIZE_HANDLE_SIZE as i32;
                if x >= corner_x && y >= corner_y {
                    return HitZone::ResizeCorner(idx);
                }
            }

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
    icons: &TitleBarIcons,
    wallpaper: &[u8],
    launcher_open: bool,
    stats: &SystemStats,
    region: DirtyRect,
) {
    // Only blit wallpaper within the dirty region
    let wp_offset = (region.y * screen.width() + region.x) * 4;
    screen.blit(region.x, region.y, region.w, region.h, screen.width(), &wallpaper[wp_offset..]);

    let focused_idx = focused_window_idx(windows);

    for (i, win) in windows.iter().enumerate() {
        if win.minimized {
            continue;
        }
        if region.overlaps(window_screen_rect(win)) {
            draw_window(screen, font, win, Some(i) == focused_idx, icons, region);
        }
    }

    let taskbar_rect = DirtyRect { x: 0, y: screen.height() - TASKBAR_HEIGHT, w: screen.width(), h: TASKBAR_HEIGHT };
    if region.overlaps(taskbar_rect) {
        draw_taskbar(screen, font, windows, focused_idx, stats);
    }

    // Draw launcher popup last so it's always on top of windows
    if launcher_open {
        let (lx, ly, lw, lh) = launcher_rect(windows, screen.height() as i32);
        let launcher_dirty = DirtyRect { x: lx as usize, y: ly as usize, w: lw as usize, h: lh as usize };
        if region.overlaps(launcher_dirty) {
            draw_launcher(screen, font, lx as usize, ly as usize, lw as usize, lh as usize);
        }
    }
}

fn draw_window(
    screen: &Framebuffer,
    font: &font::Font,
    win: &WindowState,
    focused: bool,
    icons: &TitleBarIcons,
    clip: DirtyRect,
) {
    let border_color = if focused { FOCUSED_BORDER_COLOR } else { UNFOCUSED_BORDER_COLOR };
    let title_color = if focused { FOCUSED_TITLE_COLOR } else { UNFOCUSED_TITLE_COLOR };
    let text_color = if focused { FOCUSED_TITLE_TEXT } else { UNFOCUSED_TITLE_TEXT };

    let win_x = win.content_x - BORDER_WIDTH;
    let win_y = win.content_y - BORDER_WIDTH - TITLE_BAR_HEIGHT;
    let win_w = win.width + BORDER_WIDTH * 2;

    let title_bar = DirtyRect { x: win_x, y: win_y, w: win_w, h: BORDER_WIDTH * 2 + TITLE_BAR_HEIGHT };
    if clip.overlaps(title_bar) {
        screen.fill_rect(win_x, win_y, win_w, BORDER_WIDTH + TITLE_BAR_HEIGHT, border_color);
        screen.fill_rect(
            win_x + BORDER_WIDTH,
            win_y + BORDER_WIDTH,
            win_w - BORDER_WIDTH * 2,
            TITLE_BAR_HEIGHT,
            title_color,
        );

        let title_x = win_x + BORDER_WIDTH + 8;
        let title_y = win_y + BORDER_WIDTH + (TITLE_BAR_HEIGHT - 16) / 2;
        let title = if win.title.is_empty() { "Window" } else { &win.title };
        font.draw_string(screen, title_x, title_y, title, text_color, title_color);

        let close_x = win_x + win_w - BORDER_WIDTH - BUTTON_WIDTH;
        let close_bg = if focused { CLOSE_BUTTON_BG } else { title_color };
        screen.fill_rect(close_x, win_y + BORDER_WIDTH, BUTTON_WIDTH, TITLE_BAR_HEIGHT, close_bg);
        draw_icon_centered(screen, &icons.close, close_x, win_y + BORDER_WIDTH, BUTTON_WIDTH, TITLE_BAR_HEIGHT);

        let max_x = close_x - BUTTON_WIDTH;
        screen.fill_rect(max_x, win_y + BORDER_WIDTH, BUTTON_WIDTH, TITLE_BAR_HEIGHT, title_color);
        draw_icon_centered(screen, &icons.maximize, max_x, win_y + BORDER_WIDTH, BUTTON_WIDTH, TITLE_BAR_HEIGHT);

        let min_x = max_x - BUTTON_WIDTH;
        screen.fill_rect(min_x, win_y + BORDER_WIDTH, BUTTON_WIDTH, TITLE_BAR_HEIGHT, title_color);
        draw_icon_centered(screen, &icons.minimize, min_x, win_y + BORDER_WIDTH, BUTTON_WIDTH, TITLE_BAR_HEIGHT);
    }

    // Draw side/bottom borders only if clip overlaps them
    let content_bottom = win.content_y + win.height;
    if win.content_y < content_bottom {
        // Left border
        screen.fill_rect(win_x, win.content_y, BORDER_WIDTH, win.height, border_color);
        // Right border
        screen.fill_rect(win.content_x + win.width, win.content_y, BORDER_WIDTH, win.height, border_color);
    }
    // Bottom border
    screen.fill_rect(win_x, content_bottom, win_w, BORDER_WIDTH, border_color);

    // Clip content blit to dirty region
    let blit_w = win.width.min(win.buf_width);
    let blit_h = win.height.min(win.buf_height);
    let cx = win.content_x.max(clip.x);
    let cy = win.content_y.max(clip.y);
    let cr = (win.content_x + blit_w).min(clip.x + clip.w);
    let cb = (win.content_y + blit_h).min(clip.y + clip.h);
    if cx < cr && cy < cb {
        let src_x = cx - win.content_x;
        let src_y = cy - win.content_y;
        let src_offset = (src_y * win.buf_width + src_x) * 4;
        let buffer_slice = unsafe { std::slice::from_raw_parts(win.buffer, win.buffer_size) };
        screen.blit(cx, cy, cr - cx, cb - cy, win.buf_width, &buffer_slice[src_offset..]);
    }
}

struct SystemStats {
    used_mb: u64,
    total_mb: u64,
    cpu_pct: u64,
}

fn draw_taskbar(
    screen: &Framebuffer,
    font: &font::Font,
    windows: &[WindowState],
    focused_idx: Option<usize>,
    stats: &SystemStats,
) {
    let screen_w = screen.width();
    let screen_h = screen.height();
    let taskbar_y = screen_h - TASKBAR_HEIGHT;
    screen.fill_rect(0, taskbar_y, screen_w, TASKBAR_HEIGHT, TASKBAR_COLOR);

    let text_y = taskbar_y + (TASKBAR_HEIGHT - 16) / 2;

    for (i, win) in windows.iter().enumerate() {
        let focused = Some(i) == focused_idx;
        let tab_x = i * TASKBAR_ITEM_WIDTH;
        let (bg, fg) = if win.minimized {
            (TASKBAR_MINIMIZED_COLOR, TASKBAR_MINIMIZED_TEXT)
        } else if focused {
            (TASKBAR_ACTIVE_COLOR, TASKBAR_ACTIVE_TEXT)
        } else {
            (TASKBAR_COLOR, TASKBAR_TEXT_COLOR)
        };
        screen.fill_rect(
            tab_x + 1,
            taskbar_y + TASKBAR_PADDING,
            TASKBAR_ITEM_WIDTH - 2,
            TASKBAR_HEIGHT - TASKBAR_PADDING * 2,
            bg,
        );
        let max_chars = (TASKBAR_ITEM_WIDTH - 16) / font.width();
        let title = if win.title.is_empty() { "Window" } else { &win.title };
        let display: String = title.chars().take(max_chars).collect();
        font.draw_string(screen, tab_x + 8, text_y, &display, fg, bg);
    }

    // "+" button after window tabs
    let new_x = windows.len() * TASKBAR_ITEM_WIDTH;
    screen.fill_rect(
        new_x + 1,
        taskbar_y + TASKBAR_PADDING,
        TASKBAR_HEIGHT - 2,
        TASKBAR_HEIGHT - TASKBAR_PADDING * 2,
        TASKBAR_NEW_COLOR,
    );
    let plus_x = new_x + (TASKBAR_HEIGHT - 8) / 2;
    font.draw_char(screen, plus_x, text_y, '+', TASKBAR_NEW_TEXT, TASKBAR_NEW_COLOR);

    // System stats + clock on the right
    let time = syscall::clock_realtime();
    let hours = (time >> 16) & 0xFF;
    let minutes = (time >> 8) & 0xFF;

    let status_str = format!(
        "{}M/{}M  CPU {}%  {:02}:{:02}",
        stats.used_mb, stats.total_mb, stats.cpu_pct, hours, minutes
    );
    let status_w = status_str.len() * font.width();
    let status_x = screen_w - status_w - 12;
    font.draw_string(screen, status_x, text_y, &status_str, TASKBAR_ACTIVE_TEXT, TASKBAR_COLOR);

}

fn draw_launcher(screen: &Framebuffer, font: &font::Font, x: usize, y: usize, w: usize, h: usize) {
    screen.fill_rect(x, y, w, h, LAUNCHER_BG);
    for (i, app) in LAUNCHER_APPS.iter().enumerate() {
        let item_y = y + i * LAUNCHER_ITEM_HEIGHT;
        let text_y = item_y + (LAUNCHER_ITEM_HEIGHT - 16) / 2;
        font.draw_string(screen, x + 12, text_y, app.name, LAUNCHER_TEXT, LAUNCHER_BG);
    }
}

/// Render the cursor sprite (RGBA) into a 64x64 BGRA hardware cursor buffer.
fn upload_cursor(cursor_buf: *mut u8, sprite: &sprite::Sprite, hw_cursor: bool) {
    let data = sprite.data();
    let w = sprite.width();
    let h = sprite.height();
    // Clear the full 64x64 buffer
    unsafe { core::ptr::write_bytes(cursor_buf, 0, 64 * 64 * 4); }
    // Copy sprite pixels, converting RGBA → BGRA
    for y in 0..h.min(64) {
        for x in 0..w.min(64) {
            let si = (y * w + x) * 4;
            let di = (y * 64 + x) * 4;
            unsafe {
                let dst = cursor_buf.add(di);
                *dst = data[si + 2];       // B
                *dst.add(1) = data[si + 1]; // G
                *dst.add(2) = data[si];     // R
                *dst.add(3) = data[si + 3]; // A
            }
        }
    }
    if hw_cursor {
        syscall::gpu_set_cursor(0, 0);
    }
}

/// Draw the cursor sprite directly into the framebuffer (software cursor fallback).
fn draw_software_cursor(screen: &Framebuffer, sprite: &sprite::Sprite, cx: i32, cy: i32) {
    let data = sprite.data();
    let sw = sprite.width();
    let sh = sprite.height();
    let screen_w = screen.width();
    let screen_h = screen.height();

    for sy in 0..sh {
        let py = cy as usize + sy;
        if py >= screen_h { break; }
        for sx in 0..sw {
            let px = cx as usize + sx;
            if px >= screen_w { break; }
            let si = (sy * sw + sx) * 4;
            let alpha = data[si + 3] as u32;
            if alpha == 0 { continue; }
            let sr = data[si] as u32;
            let sg = data[si + 1] as u32;
            let sb = data[si + 2] as u32;
            if alpha == 255 {
                screen.put_pixel(px, py, Color { r: sr as u8, g: sg as u8, b: sb as u8 });
            } else {
                let bg = screen.get_pixel(px, py);
                let inv = 255 - alpha;
                let r = ((sr * alpha + bg.r as u32 * inv) / 255) as u8;
                let g = ((sg * alpha + bg.g as u32 * inv) / 255) as u8;
                let b = ((sb * alpha + bg.b as u32 * inv) / 255) as u8;
                screen.put_pixel(px, py, Color { r, g, b });
            }
        }
    }
}

fn main() {
    syscall::register_name("compositor").expect("compositor already running");

    let kb_fd = syscall::open_device(syscall::DeviceType::Keyboard).expect("failed to claim keyboard");
    let mouse_fd = syscall::open_device(syscall::DeviceType::Mouse).expect("failed to claim mouse");
    let fb_fd = syscall::open_device(syscall::DeviceType::Framebuffer).expect("failed to claim framebuffer");

    let fb_info = read_fb_info(fb_fd);
    let fb_addr = syscall::map_shared(fb_info.token[0]);
    let screen = Framebuffer::new(
        fb_addr,
        fb_info.width as usize,
        fb_info.height as usize,
        fb_info.stride as usize,
        fb_info.pixel_format,
    );

    // Set up cursor
    let hw_cursor = fb_info.flags & FLAG_HARDWARE_CURSOR != 0;
    let cursor_buf = syscall::map_shared(fb_info.cursor_token);
    let cursor_svg = std::fs::read("/initrd/cursor-bold.svg").expect("failed to read cursor");
    let cursor_default = sprite::Sprite::from_svg_colored(&cursor_svg, 20, [255, 255, 255]);
    let resize_svg =
        std::fs::read("/initrd/arrow-down-right-bold.svg").expect("failed to read resize cursor");
    let cursor_resize = sprite::Sprite::from_svg_colored(&resize_svg, 20, [255, 255, 255]);
    upload_cursor(cursor_buf, &cursor_default, hw_cursor);
    let mut current_cursor_is_resize = false;

    let font_data = std::fs::read("/initrd/JetBrainsMono-8x16.font").expect("failed to read font");
    let font = font::Font::from_prebuilt(&font_data);

    eprintln!("compositor: decoding wallpaper...");
    let wallpaper = {
        let jpg_data = std::fs::read("/initrd/wallpaper.jpg").expect("failed to read wallpaper");
        let img = image::load_from_memory_with_format(&jpg_data, image::ImageFormat::Jpeg)
            .expect("failed to decode wallpaper");
        let rgb = img.to_rgb8();
        eprintln!(
            "compositor: wallpaper decoded {}x{}, scaling to {}x{}",
            rgb.width(), rgb.height(), screen.width(), screen.height()
        );
        scale_wallpaper(
            rgb.as_raw(),
            rgb.width() as usize,
            rgb.height() as usize,
            screen.width(),
            screen.height(),
            screen.pixel_format_raw() != 0,
        )
    };

    let icons = TitleBarIcons {
        minimize: sprite::Sprite::from_svg_colored(
            &std::fs::read("/initrd/minus-bold.svg").expect("failed to read minimize icon"),
            14,
            [255, 255, 255],
        ),
        maximize: sprite::Sprite::from_svg_colored(
            &std::fs::read("/initrd/square-bold.svg").expect("failed to read maximize icon"),
            14,
            [255, 255, 255],
        ),
        close: sprite::Sprite::from_svg_colored(
            &std::fs::read("/initrd/x-bold.svg").expect("failed to read close icon"),
            14,
            [255, 255, 255],
        ),
    };

    eprintln!("compositor: ready");

    let mut windows: Vec<WindowState> = Vec::new();
    let screen_w = screen.width() as i32;
    let screen_h = screen.height() as i32;
    let mut cursor_x = screen_w / 2;
    let mut cursor_y = screen_h / 2;
    if hw_cursor {
        syscall::gpu_move_cursor(cursor_x as u32, cursor_y as u32);
    }
    let mut dirty_rect: Option<DirtyRect> = Some(DirtyRect::full(screen_w as usize, screen_h as usize));
    let mut prev_buttons: u8 = 0;
    let mut interaction = Interaction::None;
    let mut last_title_click_time: u64 = 0;
    let mut last_title_click_pid: u32 = 0;
    let mut clipboard = String::new();
    Command::new("/initrd/filepicker").spawn().ok();
    let mut launcher_open = false;
    let mut prev_busy_ticks: u64 = 0;
    let mut prev_total_ticks: u64 = 0;
    let mut cpu_pct: u64 = 0;
    let mut last_taskbar_update: u64 = 0;
    let mut cached_stats = SystemStats { used_mb: 0, total_mb: 0, cpu_pct: 0 };

    loop {
        // Drain all pending events before compositing
        let mut waited = false;
        loop {
            let timeout = if waited { 1 } else { FRAME_INTERVAL_NS };
            let ready = syscall::poll_timeout(&[kb_fd, mouse_fd], timeout);

            if !ready.fd(0) && !ready.fd(1) && !ready.messages() {
                break;
            }
            waited = true;

        if ready.fd(0) {
            let mut events = [window::KeyEvent::EMPTY; 8];
            let buf = unsafe {
                std::slice::from_raw_parts_mut(
                    events.as_mut_ptr() as *mut u8,
                    std::mem::size_of_val(&events),
                )
            };
            let n = syscall::read_fd(kb_fd, buf);
            for event in &events[..n / std::mem::size_of::<window::KeyEvent>()] {
                if launcher_open && event.pressed() && event.keycode == 0x29 {
                    // Escape: close launcher
                    launcher_open = false;
                    mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                } else if event.pressed() && event.alt() && event.keycode == 0x2B {
                    // Alt+Tab: rotate focus among non-topmost windows
                    let first_topmost = windows.iter().position(|w| w.topmost).unwrap_or(windows.len());
                    if first_topmost > 1 {
                        let win = windows.remove(first_topmost - 1);
                        windows.insert(0, win);
                        let first_topmost = windows.iter().position(|w| w.topmost).unwrap_or(windows.len());
                        if first_topmost > 0 {
                            windows[first_topmost - 1].minimized = false;
                        }
                        mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                    }
                } else if event.pressed() && event.gui() {
                    if let Some(idx) = focused_window_idx(&windows) {
                        let pixel_format = screen.pixel_format_raw();
                        match event.keycode {
                            0x50 => {
                                // Super+Left: snap left or restore
                                if windows[idx].mode == WindowMode::SnappedLeft {
                                    restore_window(&mut windows[idx], pixel_format);
                                } else {
                                    snap_left(
                                        &mut windows[idx],
                                        screen_w as usize,
                                        screen_h as usize,
                                        pixel_format,
                                    );
                                }
                            }
                            0x4F => {
                                // Super+Right: snap right or restore
                                if windows[idx].mode == WindowMode::SnappedRight {
                                    restore_window(&mut windows[idx], pixel_format);
                                } else {
                                    snap_right(
                                        &mut windows[idx],
                                        screen_w as usize,
                                        screen_h as usize,
                                        pixel_format,
                                    );
                                }
                            }
                            0x52 => {
                                // Super+Up: maximize or restore
                                if windows[idx].mode == WindowMode::Maximized {
                                    restore_window(&mut windows[idx], pixel_format);
                                } else {
                                    maximize_window(
                                        &mut windows[idx],
                                        screen_w as usize,
                                        screen_h as usize,
                                        pixel_format,
                                    );
                                }
                            }
                            0x51 => {
                                // Super+Down: restore or minimize
                                if windows[idx].mode != WindowMode::Normal {
                                    restore_window(&mut windows[idx], pixel_format);
                                } else {
                                    windows[idx].minimized = true;
                                }
                            }
                            0x14 => {
                                // GUI+Q: close focused window
                                let win = windows.remove(idx);
                                message::send(win.pid, Message::signal(window::MSG_WINDOW_CLOSE)).ok();
                            }
                            0x19 => {
                                // GUI+V: paste clipboard
                                if !clipboard.is_empty() {
                                    message::send(
                                        windows[idx].pid,
                                        Message::from_bytes(
                                            window::MSG_CLIPBOARD_PASTE,
                                            clipboard.as_bytes(),
                                        ),
                                    ).ok();
                                }
                            }
                            _ => {
                                // Forward other GUI combos to focused app
                                message::send(
                                    windows[idx].pid,
                                    Message::new(window::MSG_KEY_INPUT, *event),
                                ).ok();
                            }
                        }
                        mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                    }
                } else if event.pressed() && event.ctrl() && event.keycode == 0x11 {
                    // Ctrl+N: spawn terminal
                    Command::new("/initrd/terminal").spawn().ok();
                } else {
                    if let Some(idx) = focused_window_idx(&windows) {
                        message::send(
                            windows[idx].pid,
                            Message::new(window::MSG_KEY_INPUT, *event),
                        ).ok();
                    }
                }
            }
        }

        if ready.fd(1) {
            // Drain all pending mouse events in one read
            let mut buf = [0u8; 512];
            let n = syscall::read_fd(mouse_fd, &mut buf);
            let event_count = n / 4;

            // Accumulate deltas and track button transitions
            let mut total_dx: i32 = 0;
            let mut total_dy: i32 = 0;
            let mut total_scroll: i32 = 0;
            let mut buttons = prev_buttons;
            let mut press_happened = false;
            let mut release_happened = false;

            for i in 0..event_count {
                let off = i * 4;
                let new_buttons = buf[off];
                total_dx += buf[off + 1] as i8 as i32;
                total_dy += buf[off + 2] as i8 as i32;
                total_scroll += buf[off + 3] as i8 as i32;

                let new_left = new_buttons & 1 != 0;
                let old_left = buttons & 1 != 0;
                if new_left && !old_left { press_happened = true; }
                if !new_left && old_left { release_happened = true; }
                buttons = new_buttons;
            }

            if event_count > 0 {
                let left = buttons & 1 != 0;

                // Move cursor once for all accumulated deltas
                cursor_x = (cursor_x + total_dx).clamp(0, screen_w - 1);
                cursor_y = (cursor_y + total_dy).clamp(0, screen_h - 1);
                if hw_cursor {
                    syscall::gpu_move_cursor(cursor_x as u32, cursor_y as u32);
                } else {
                    // Software cursor: mark old and new cursor regions dirty
                    let cw = 20usize;
                    let ch = 20usize;
                    let old_cx = (cursor_x - total_dx).clamp(0, screen_w - 1) as usize;
                    let old_cy = (cursor_y - total_dy).clamp(0, screen_h - 1) as usize;
                    mark_dirty(&mut dirty_rect, DirtyRect { x: old_cx, y: old_cy, w: cw, h: ch });
                    mark_dirty(&mut dirty_rect, DirtyRect { x: cursor_x as usize, y: cursor_y as usize, w: cw, h: ch });
                }

                // Update cursor shape
                let want_resize = match interaction {
                    Interaction::Resizing { .. } => true,
                    _ => matches!(
                        hit_test(&windows, cursor_x, cursor_y, screen_h, launcher_open),
                        HitZone::ResizeCorner(_)
                    ),
                };
                if want_resize != current_cursor_is_resize {
                    current_cursor_is_resize = want_resize;
                    if want_resize {
                        upload_cursor(cursor_buf, &cursor_resize, hw_cursor);
                    } else {
                        upload_cursor(cursor_buf, &cursor_default, hw_cursor);
                    }
                }

                let make_mouse_event =
                    |win: &WindowState, event_type: u8, changed: u8, scroll: i8| {
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

                // Left button pressed during this batch
                if press_happened {
                    match hit_test(&windows, cursor_x, cursor_y, screen_h, launcher_open) {
                        HitZone::CloseButton(idx) => {
                            let win = windows.remove(idx);
                            message::send(win.pid, Message::signal(window::MSG_WINDOW_CLOSE)).ok();
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        HitZone::MinimizeButton(idx) => {
                            windows[idx].minimized = true;
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        HitZone::MaximizeButton(idx) => {
                            let new_idx = bring_to_front(&mut windows, idx);
                            let pixel_format = screen.pixel_format_raw();
                            if windows[new_idx].mode != WindowMode::Normal {
                                restore_window(&mut windows[new_idx], pixel_format);
                            } else {
                                maximize_window(
                                    &mut windows[new_idx],
                                    screen_w as usize,
                                    screen_h as usize,
                                    pixel_format,
                                );
                            }
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        HitZone::TitleBar(idx) => {
                            let new_idx = bring_to_front(&mut windows, idx);

                            // Double-click detection
                            let now = syscall::clock_nanos();
                            let pid = windows[new_idx].pid;
                            if pid == last_title_click_pid
                                && now.wrapping_sub(last_title_click_time) < DOUBLE_CLICK_NS
                            {
                                let pixel_format = screen.pixel_format_raw();
                                if windows[new_idx].mode != WindowMode::Normal {
                                    restore_window(&mut windows[new_idx], pixel_format);
                                } else {
                                    maximize_window(
                                        &mut windows[new_idx],
                                        screen_w as usize,
                                        screen_h as usize,
                                        pixel_format,
                                    );
                                }
                                last_title_click_pid = 0;
                                last_title_click_time = 0;
                            } else {
                                last_title_click_pid = pid;
                                last_title_click_time = now;
                                // Un-snap/maximize on drag
                                if windows[new_idx].mode != WindowMode::Normal {
                                    let pixel_format = screen.pixel_format_raw();
                                    restore_window(&mut windows[new_idx], pixel_format);
                                    let win = &mut windows[new_idx];
                                    win.content_x = (cursor_x as usize)
                                        .saturating_sub(win.width / 2)
                                        .max(BORDER_WIDTH);
                                    win.content_y = (cursor_y as usize)
                                        .max(BORDER_WIDTH + TITLE_BAR_HEIGHT);
                                }
                                interaction = Interaction::Dragging {
                                    window_idx: new_idx,
                                };
                            }
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        HitZone::ResizeCorner(idx) => {
                            let new_idx = bring_to_front(&mut windows, idx);
                            interaction = Interaction::Resizing {
                                window_idx: new_idx,
                            };
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        HitZone::Content(idx) => {
                            launcher_open = false;
                            let new_idx = bring_to_front(&mut windows, idx);
                            if new_idx != idx {
                                mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                            }
                            let win = &windows[new_idx];
                            let ev = make_mouse_event(win, window::MOUSE_PRESS, 1, 0);
                            message::send(
                                win.pid,
                                Message::new(window::MSG_MOUSE_INPUT, ev),
                            ).ok();
                        }
                        HitZone::TaskbarItem(idx) => {
                            if idx < windows.len() {
                                if windows[idx].minimized {
                                    windows[idx].minimized = false;
                                    bring_to_front(&mut windows, idx);
                                } else if Some(idx) == focused_window_idx(&windows) {
                                    windows[idx].minimized = true;
                                } else {
                                    bring_to_front(&mut windows, idx);
                                }
                                mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                            }
                        }
                        HitZone::TaskbarNew => {
                            launcher_open = !launcher_open;
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        HitZone::LauncherItem(idx) => {
                            Command::new(LAUNCHER_APPS[idx].path).spawn().ok();
                            launcher_open = false;
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        HitZone::Desktop => {
                            if launcher_open {
                                launcher_open = false;
                                mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                            }
                        }
                    }
                }

                // Left button released during this batch
                if release_happened {
                    if let Some(idx) = focused_window_idx(&windows) {
                        let ev = make_mouse_event(&windows[idx], window::MOUSE_RELEASE, 1, 0);
                        message::send(
                            windows[idx].pid,
                            Message::new(window::MSG_MOUSE_INPUT, ev),
                        ).ok();
                    }
                    match interaction {
                        Interaction::Dragging { window_idx } => {
                            // Snap detection on drag release
                            let pixel_format = screen.pixel_format_raw();
                            if cursor_x <= 2 {
                                snap_left(
                                    &mut windows[window_idx],
                                    screen_w as usize,
                                    screen_h as usize,
                                    pixel_format,
                                );
                            } else if cursor_x >= screen_w - 3 {
                                snap_right(
                                    &mut windows[window_idx],
                                    screen_w as usize,
                                    screen_h as usize,
                                    pixel_format,
                                );
                            } else if cursor_y <= 2 {
                                maximize_window(
                                    &mut windows[window_idx],
                                    screen_w as usize,
                                    screen_h as usize,
                                    pixel_format,
                                );
                            }
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        Interaction::Resizing { window_idx } => {
                            let pixel_format = screen.pixel_format_raw();
                            let win = &mut windows[window_idx];
                            let new_w = win.width;
                            let new_h = win.height;
                            resize_window(win, new_w, new_h, pixel_format);
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        Interaction::None => {}
                    }
                    interaction = Interaction::None;
                }

                // Drag / resize with accumulated deltas
                if left {
                    match interaction {
                        Interaction::Dragging { window_idx } => {
                            let old_rect = window_screen_rect(&windows[window_idx]);
                            let win = &mut windows[window_idx];
                            let min_x = BORDER_WIDTH as i32;
                            let min_y = (BORDER_WIDTH + TITLE_BAR_HEIGHT) as i32;
                            win.content_x =
                                (win.content_x as i32 + total_dx).max(min_x) as usize;
                            win.content_y =
                                (win.content_y as i32 + total_dy).max(min_y) as usize;
                            let new_rect = window_screen_rect(&windows[window_idx]);
                            mark_dirty(&mut dirty_rect, old_rect.union(new_rect));
                        }
                        Interaction::Resizing { window_idx } => {
                            let old_rect = window_screen_rect(&windows[window_idx]);
                            let win = &mut windows[window_idx];
                            win.width = (win.width as i32 + total_dx)
                                .max(MIN_CONTENT_WIDTH as i32)
                                as usize;
                            win.height = (win.height as i32 + total_dy)
                                .max(MIN_CONTENT_HEIGHT as i32)
                                as usize;
                            let new_rect = window_screen_rect(&windows[window_idx]);
                            mark_dirty(&mut dirty_rect, old_rect.union(new_rect));
                        }
                        Interaction::None => {
                            // Forward mouse move to focused app for drag selection
                            if let Some(idx) = focused_window_idx(&windows) {
                                let ev = make_mouse_event(
                                    &windows[idx],
                                    window::MOUSE_MOVE,
                                    0,
                                    0,
                                );
                                message::send(
                                    windows[idx].pid,
                                    Message::new(window::MSG_MOUSE_INPUT, ev),
                                ).ok();
                            }
                        }
                    }
                }

                // Scroll with accumulated total
                if total_scroll != 0 {
                    if let Some(idx) = focused_window_idx(&windows) {
                        if let HitZone::Content(_) =
                            hit_test(&windows, cursor_x, cursor_y, screen_h, launcher_open)
                        {
                            let clamped_scroll = total_scroll.clamp(-128, 127) as i8;
                            let ev =
                                make_mouse_event(&windows[idx], window::MOUSE_SCROLL, 0, clamped_scroll);
                            message::send(
                                windows[idx].pid,
                                Message::new(window::MSG_MOUSE_INPUT, ev),
                            ).ok();
                        }
                    }
                }

                prev_buttons = buttons;
            }
        }

        if ready.messages() {
            let msg = message::recv();
            let sender = msg.sender();
            match msg.msg_type() {
                window::MSG_CREATE_WINDOW => {
                    let req: window::CreateWindowRequest = msg.take_payload();
                    let title = if req.title_len > 0 {
                        let len = (req.title_len as usize).min(30);
                        String::from_utf8_lossy(&req.title[..len]).into_owned()
                    } else {
                        String::new()
                    };

                    let screen_w = screen.width();
                    let screen_h = screen.height();

                    let req_w = req.width as usize;
                    let req_h = req.height as usize;

                    let (win_x, win_y, win_w, win_h);
                    if req_w > 0 && req_h > 0 {
                        // App requested a specific content size — compute window size around it
                        let chrome_w = BORDER_WIDTH * 2;
                        let chrome_h = BORDER_WIDTH * 2 + TITLE_BAR_HEIGHT;
                        win_w = req_w + chrome_w;
                        win_h = req_h + chrome_h;
                        // Center on screen
                        win_x = (screen_w.saturating_sub(win_w)) / 2;
                        win_y = (screen_h.saturating_sub(win_h + TASKBAR_HEIGHT)) / 2;
                    } else {
                        let offset = CASCADE_OFFSET * (windows.len() % 10);
                        win_x = INITIAL_MARGIN + offset;
                        win_y = INITIAL_MARGIN + offset;
                        win_w = screen_w - INITIAL_MARGIN * 2;
                        win_h = screen_h - INITIAL_MARGIN * 2 - TASKBAR_HEIGHT;
                    }

                    let content_x = win_x + BORDER_WIDTH;
                    let content_y = win_y + BORDER_WIDTH + TITLE_BAR_HEIGHT;
                    let content_w = win_w - BORDER_WIDTH * 2;
                    let content_h = win_h - BORDER_WIDTH * 2 - TITLE_BAR_HEIGHT;

                    let buf_size = content_w * content_h * 4;
                    let token = syscall::alloc_shared(buf_size);
                    let buffer = syscall::map_shared(token);
                    syscall::grant_shared(token, sender);
                    let pixel_format = screen.pixel_format_raw();

                    let topmost = req.flags & window::WINDOW_FLAG_TOPMOST != 0;
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
                        minimized: false,
                        topmost,
                        mode: WindowMode::Normal,
                        saved_x: 0,
                        saved_y: 0,
                        saved_w: 0,
                        saved_h: 0,
                        presented: false,
                    });

                    message::send(
                        sender,
                        Message::new(
                            window::MSG_WINDOW_CREATED,
                            window::WindowInfo {
                                token,
                                width: content_w as u32,
                                height: content_h as u32,
                                stride: content_w as u32,
                                pixel_format,
                            },
                        ),
                    ).ok();
                    mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w, screen_h));
                }
                window::MSG_PRESENT => {
                    if let Some(win) = windows.iter_mut().find(|w| w.pid == sender) {
                        win.presented = true;
                        mark_dirty(&mut dirty_rect, window_screen_rect(win));
                    }
                }
                window::MSG_DESTROY_WINDOW => {
                    if let Some(idx) = windows.iter().position(|w| w.pid == sender) {
                        windows.remove(idx);
                        mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                    }
                }
                window::MSG_CLIPBOARD_SET => {
                    let bytes = msg.take_bytes();
                    clipboard = String::from_utf8_lossy(&bytes).into_owned();
                }
                _ => {}
            }
        }
        } // end inner drain loop

        // Refresh taskbar once per second for clock + stats
        let now = syscall::clock_nanos();
        if now - last_taskbar_update >= 1_000_000_000 {
            last_taskbar_update = now;

            let mut si = [0u8; 48];
            if syscall::sysinfo(&mut si) >= 48 {
                let total_mem = u64::from_le_bytes(si[0..8].try_into().unwrap());
                let used_mem = u64::from_le_bytes(si[8..16].try_into().unwrap());
                let busy = u64::from_le_bytes(si[32..40].try_into().unwrap());
                let total = u64::from_le_bytes(si[40..48].try_into().unwrap());
                let d_busy = busy.wrapping_sub(prev_busy_ticks);
                let d_total = total.wrapping_sub(prev_total_ticks);
                if d_total > 0 {
                    cpu_pct = d_busy * 100 / d_total;
                }
                prev_busy_ticks = busy;
                prev_total_ticks = total;
                cached_stats = SystemStats {
                    used_mb: used_mem / (1024 * 1024),
                    total_mb: total_mem / (1024 * 1024),
                    cpu_pct,
                };
            }

            let taskbar_dirty = DirtyRect {
                x: 0, y: screen_h as usize - TASKBAR_HEIGHT,
                w: screen_w as usize, h: TASKBAR_HEIGHT,
            };
            mark_dirty(&mut dirty_rect, taskbar_dirty);
        }

        // Composite once per frame
        if let Some(rect) = dirty_rect.take() {
            let rect = rect.clamp(screen_w as usize, screen_h as usize);
            if rect.w > 0 && rect.h > 0 {
                redraw(&screen, &font, &windows, &icons, &wallpaper, launcher_open, &cached_stats, rect);

                // Draw software cursor if no hardware cursor
                if !hw_cursor {
                    let sprite = if current_cursor_is_resize { &cursor_resize } else { &cursor_default };
                    draw_software_cursor(&screen, sprite, cursor_x, cursor_y);
                }

                syscall::gpu_present(rect.x as u32, rect.y as u32, rect.w as u32, rect.h as u32);

                // Send frame callbacks to windows that presented and were composited
                for win in windows.iter_mut() {
                    if win.presented && !win.minimized && rect.overlaps(window_screen_rect(win)) {
                        message::send(win.pid, Message::signal(window::MSG_FRAME)).ok();
                        win.presented = false;
                    }
                }

                // Reap windows whose processes have exited
                let count_before = windows.len();
                windows.retain(|win| {
                    message::send(win.pid, Message::signal(window::MSG_FRAME)).is_ok()
                });
                if windows.len() != count_before {
                    mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                }
            }
        }
    }
}

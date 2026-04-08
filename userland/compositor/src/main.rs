use std::process::Command;
use std::time::{Duration, Instant};

use toyos_abi::shm::SharedMemory;
use toyos_abi::{gpu, ipc, services, system, FramebufferInfo, Fd, OwnedFd};
use toyos_abi::io_uring::*;
use toyos_abi::{device, syscall};
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
const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(400);
const FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667); // ~60fps

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
    LauncherEntry { name: "Terminal", path: "/bin/terminal" },
    LauncherEntry { name: "Files", path: "/bin/files" },
];

const FLAG_HARDWARE_CURSOR: u32 = 1 << 0;

fn read_fb_info(fd: &OwnedFd) -> FramebufferInfo {
    let mut buf = [0u8; std::mem::size_of::<FramebufferInfo>()];
    let n = syscall::read(fd.fd(), &mut buf).expect("failed to read framebuffer info");
    assert_eq!(n, buf.len(), "failed to read framebuffer info");
    unsafe { std::ptr::read(buf.as_ptr() as *const FramebufferInfo) }
}

struct WindowState {
    fd: OwnedFd,
    pid: u32,
    shm: SharedMemory,
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
    cursor_style: u8,
    /// Partial IPC header bytes received so far. Fully non-blocking client I/O:
    /// if a read returns partial data, buffer it here and finish next iteration.
    recv_buf: [u8; 8],
    recv_len: usize,
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
    let old_token = win.shm.token();
    let buf_size = new_w * new_h * 4;
    let new_shm = SharedMemory::allocate(buf_size);
    new_shm.grant(win.pid);
    let token = new_shm.token();
    // Replace the old SharedMemory (drops it, releasing the old mapping)
    win.shm = new_shm;
    win.width = new_w;
    win.height = new_h;
    win.buf_width = new_w;
    win.buf_height = new_h;
    let _ = ipc::send(
        win.fd.fd(),
        window::MSG_WINDOW_RESIZED,
        &window::ResizeInfo {
            token,
            old_token,
            width: new_w as u32,
            height: new_h as u32,
            stride: new_w as u32,
            pixel_format,
        },
    );
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
    DragPending {
        window_idx: usize,
        start_x: i32,
        start_y: i32,
        was_maximized: bool,
    },
    Dragging { window_idx: usize },
    Resizing { window_idx: usize },
}

const DRAG_THRESHOLD: i32 = 5;

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
        let buffer_slice = unsafe { std::slice::from_raw_parts(win.shm.as_ptr(), win.shm.len()) };
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
    let time = system::clock_realtime();
    let hours = time.hours;
    let minutes = time.minutes;

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
        gpu::set_cursor(0, 0);
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
    let listener_fd = services::listen("compositor").expect("compositor already running");

    let kb_fd = device::open_keyboard().expect("failed to claim keyboard");
    let mouse_fd = device::open_mouse().expect("failed to claim mouse");
    let fb_fd = device::open_framebuffer().expect("failed to claim framebuffer");

    let mut fb_info = read_fb_info(&fb_fd);
    let fb_size = fb_info.stride as usize * fb_info.height as usize * 4;
    let mut fb_shm = SharedMemory::map(fb_info.token[0], fb_size);
    let mut screen = Framebuffer::new(
        fb_shm.as_ptr(),
        fb_info.width as usize,
        fb_info.height as usize,
        fb_info.stride as usize,
        fb_info.pixel_format,
    );

    // Set up cursor
    let hw_cursor = fb_info.flags & FLAG_HARDWARE_CURSOR != 0;
    let cursor_shm = SharedMemory::map(fb_info.cursor_token, 64 * 64 * 4);
    let cursor_buf = cursor_shm.as_ptr();
    let cursor_svg = std::fs::read("/share/icons/cursor-bold.svg").expect("failed to read cursor");
    let cursor_default = sprite::Sprite::from_svg_colored(&cursor_svg, 20, [255, 255, 255]);
    let resize_svg =
        std::fs::read("/share/icons/arrow-down-right-bold.svg").expect("failed to read resize cursor");
    let cursor_resize = sprite::Sprite::from_svg_colored(&resize_svg, 20, [255, 255, 255]);
    let crosshair_svg =
        std::fs::read("/share/icons/crosshair-simple-bold.svg").expect("failed to read crosshair cursor");
    let cursor_crosshair = sprite::Sprite::from_svg_colored(&crosshair_svg, 20, [0, 0, 0]);
    upload_cursor(cursor_buf, &cursor_default, hw_cursor);
    let mut current_cursor_style: u8 = window::CURSOR_DEFAULT;

    let font_data = std::fs::read("/share/fonts/JetBrainsMono-Regular-8x16.font").expect("failed to read font");
    let font = font::Font::from_prebuilt(&font_data);

    let wallpaper_raw = std::fs::read("/share/wallpaper.rgb").expect("failed to read wallpaper");
    let wallpaper_w = u32::from_le_bytes(wallpaper_raw[0..4].try_into().unwrap()) as usize;
    let wallpaper_h = u32::from_le_bytes(wallpaper_raw[4..8].try_into().unwrap()) as usize;
    let wallpaper_pixels = &wallpaper_raw[8..];
    eprintln!(
        "compositor: wallpaper {}x{}, scaling to {}x{}",
        wallpaper_w, wallpaper_h, screen.width(), screen.height()
    );
    let mut wallpaper = scale_wallpaper(
        wallpaper_pixels,
        wallpaper_w,
        wallpaper_h,
        screen.width(),
        screen.height(),
        screen.pixel_format_raw() != 0,
    );

    let icons = TitleBarIcons {
        minimize: sprite::Sprite::from_svg_colored(
            &std::fs::read("/share/icons/minus-bold.svg").expect("failed to read minimize icon"),
            14,
            [255, 255, 255],
        ),
        maximize: sprite::Sprite::from_svg_colored(
            &std::fs::read("/share/icons/square-bold.svg").expect("failed to read maximize icon"),
            14,
            [255, 255, 255],
        ),
        close: sprite::Sprite::from_svg_colored(
            &std::fs::read("/share/icons/x-bold.svg").expect("failed to read close icon"),
            14,
            [255, 255, 255],
        ),
    };

    eprintln!("compositor: ready");

    let mut windows: Vec<WindowState> = Vec::new();
    let mut screen_w = screen.width() as i32;
    let mut screen_h = screen.height() as i32;
    let mut cursor_x = screen_w / 2;
    let mut cursor_y = screen_h / 2;
    if hw_cursor {
        gpu::move_cursor(cursor_x as u32, cursor_y as u32);
    }
    let mut dirty_rect: Option<DirtyRect> = Some(DirtyRect::full(screen_w as usize, screen_h as usize));
    let mut prev_buttons: u8 = 0;
    let mut interaction = Interaction::None;
    let mut last_title_click_time = Instant::now();
    let mut last_title_click_fd: Option<Fd> = None;
    let mut clipboard = String::new();
    Command::new("/bin/filepicker").spawn().ok();
    let mut launcher_open = false;
    let mut prev_busy_ticks: u64 = 0;
    let mut prev_total_ticks: u64 = 0;
    let mut cpu_pct: u64 = 0;
    let mut last_taskbar_update = Instant::now();
    let mut cached_stats = SystemStats { used_mb: 0, total_mb: 0, cpu_pct: 0 };

    // io_uring token assignments: system fds use their fd number directly.
    // Client fds also use their fd number. We distinguish by checking if the
    // token matches a known system fd.
    let token_kb = kb_fd.fd().0 as u64;
    let token_mouse = mouse_fd.fd().0 as u64;
    let token_listener = listener_fd.fd().0 as u64;

    // Create io_uring for event multiplexing
    let (raw_ring_fd, ring_shm_token) = syscall::io_uring_setup(256).expect("io_uring_setup failed");
    let ring_fd = OwnedFd::new(raw_ring_fd);
    let ring_base = unsafe { syscall::map_shared(ring_shm_token) };
    let ring_params = unsafe { &*(ring_base as *const IoUringParams) };
    let ring_sq_size = ring_params.sq_ring_size;
    let ring_cq_size = ring_params.cq_ring_size;

    let ring_submit = |fd: i32, token: u64, flags: u32| {
        let sq_hdr = unsafe { &*(ring_base.add(SQ_RING_OFF as usize) as *const IoUringRingHeader) };
        let tail = sq_hdr.tail.load(std::sync::atomic::Ordering::Acquire);
        let idx = tail & (ring_sq_size - 1);
        let sqe = unsafe {
            &mut *(ring_base.add(SQES_OFF as usize + idx as usize * core::mem::size_of::<IoUringSqe>()) as *mut IoUringSqe)
        };
        *sqe = IoUringSqe::default();
        sqe.op = IORING_OP_POLL_ADD;
        sqe.fd = fd;
        sqe.op_flags = flags;
        sqe.user_data = token;
        sq_hdr.tail.store(tail.wrapping_add(1), std::sync::atomic::Ordering::Release);
    };

    // Submit initial POLL_ADDs for system fds (tokens = fd numbers)
    ring_submit(kb_fd.fd().0, token_kb, IORING_POLL_IN);
    ring_submit(mouse_fd.fd().0, token_mouse, IORING_POLL_IN);
    ring_submit(listener_fd.fd().0, token_listener, IORING_POLL_IN);

    loop {
        // Drain all pending events before compositing
        let mut waited = false;
        loop {
            let timeout = if waited { Duration::from_nanos(1) } else { FRAME_INTERVAL };

            // Submit any pending SQEs (initial or re-armed) and wait
            let pending = {
                let sq_hdr = unsafe { &*(ring_base.add(SQ_RING_OFF as usize) as *const IoUringRingHeader) };
                let head = sq_hdr.head.load(std::sync::atomic::Ordering::Acquire);
                let tail = sq_hdr.tail.load(std::sync::atomic::Ordering::Acquire);
                tail.wrapping_sub(head)
            };
            let _ = syscall::io_uring_enter(ring_fd.fd(), pending, 1, timeout.as_nanos() as u64);

            // Drain CQEs into a set of ready tokens (fd numbers)
            let mut ready_tokens: Vec<u64> = Vec::new();
            let cq_hdr = unsafe { &*(ring_base.add(CQ_RING_OFF as usize) as *const IoUringRingHeader) };
            loop {
                let head = cq_hdr.head.load(std::sync::atomic::Ordering::Acquire);
                let tail = cq_hdr.tail.load(std::sync::atomic::Ordering::Acquire);
                if head == tail { break; }
                let idx = head & (ring_cq_size - 1);
                let cqe = unsafe {
                    &*(ring_base.add(CQ_RING_OFF as usize + 16 + idx as usize * core::mem::size_of::<IoUringCqe>()) as *const IoUringCqe)
                };
                if cqe.result > 0 {
                    ready_tokens.push(cqe.user_data);
                }
                cq_hdr.head.store(head.wrapping_add(1), std::sync::atomic::Ordering::Release);
            }

            let kb_ready = ready_tokens.contains(&token_kb);
            let mouse_ready = ready_tokens.contains(&token_mouse);
            let listener_ready = ready_tokens.contains(&token_listener);
            let any_client_ready = windows.iter().any(|w| ready_tokens.contains(&(w.fd.fd().0 as u64)));

            if !kb_ready && !mouse_ready && !listener_ready && !any_client_ready {
                break;
            }
            waited = true;

        if kb_ready {
            let mut events = [window::KeyEvent::EMPTY; 8];
            let buf = unsafe {
                std::slice::from_raw_parts_mut(
                    events.as_mut_ptr() as *mut u8,
                    std::mem::size_of_val(&events),
                )
            };
            let n = syscall::read(kb_fd.fd(), buf).unwrap_or(0);
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
                                let _ = ipc::signal(win.fd.fd(), window::MSG_WINDOW_CLOSE);
                            }
                            0x19 => {
                                // GUI+V: paste clipboard
                                if !clipboard.is_empty() {
                                    if clipboard.len() <= 4096 {
                                        let _ = ipc::send_bytes(
                                            windows[idx].fd.fd(),
                                            window::MSG_CLIPBOARD_PASTE,
                                            clipboard.as_bytes(),
                                        );
                                    } else {
                                        static PASTE_SHM: std::sync::Mutex<Option<SharedMemory>> =
                                            std::sync::Mutex::new(None);
                                        let mut shm = SharedMemory::allocate(clipboard.len());
                                        shm.as_mut_slice()[..clipboard.len()]
                                            .copy_from_slice(clipboard.as_bytes());
                                        let _ = ipc::send(
                                            windows[idx].fd.fd(),
                                            window::MSG_CLIPBOARD_PASTE_SHM,
                                            &window::ClipboardShmMsg {
                                                token: shm.token(),
                                                len: clipboard.len() as u32,
                                            },
                                        );
                                        *PASTE_SHM.lock().unwrap() = Some(shm);
                                    }
                                }
                            }
                            _ => {
                                // Forward other GUI combos to focused app
                                let _ = ipc::send(
                                    windows[idx].fd.fd(),
                                    window::MSG_KEY_INPUT,
                                    event,
                                );
                            }
                        }
                        mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                    }
                } else if event.pressed() && event.ctrl() && event.keycode == 0x11 {
                    // Ctrl+N: spawn terminal
                    Command::new("/bin/terminal").spawn().ok();
                } else {
                    if let Some(idx) = focused_window_idx(&windows) {
                        let _ = ipc::send(windows[idx].fd.fd(), window::MSG_KEY_INPUT, event);
                    }
                }
            }
        }

        if mouse_ready {
            // Drain all pending mouse events in one read
            let mut buf = [0u8; 512];
            let n = syscall::read(mouse_fd.fd(), &mut buf).unwrap_or(0);
            let event_size = 6; // MouseEvent: buttons(1) + scroll(1) + abs_x(2) + abs_y(2)
            let event_count = n / event_size;

            // Track button transitions and last absolute position
            let mut total_scroll: i32 = 0;
            let mut last_abs_x: u16 = 0;
            let mut last_abs_y: u16 = 0;
            let mut buttons = prev_buttons;
            let mut press_happened = false;
            let mut release_happened = false;

            for i in 0..event_count {
                let off = i * event_size;
                let new_buttons = buf[off];
                let scroll = buf[off + 1] as i8;
                last_abs_x = u16::from_le_bytes([buf[off + 2], buf[off + 3]]);
                last_abs_y = u16::from_le_bytes([buf[off + 4], buf[off + 5]]);
                total_scroll += scroll as i32;

                let new_left = new_buttons & 1 != 0;
                let old_left = buttons & 1 != 0;
                if new_left && !old_left { press_happened = true; }
                if !new_left && old_left { release_happened = true; }
                buttons = new_buttons;
            }

            if event_count > 0 {
                let left = buttons & 1 != 0;

                // Convert absolute tablet coordinates (0–32767) to screen coordinates
                let old_cursor_x = cursor_x;
                let old_cursor_y = cursor_y;
                cursor_x = (last_abs_x as i32 * screen_w / 32768).clamp(0, screen_w - 1);
                cursor_y = (last_abs_y as i32 * screen_h / 32768).clamp(0, screen_h - 1);
                if hw_cursor {
                    gpu::move_cursor(cursor_x as u32, cursor_y as u32);
                } else {
                    // Software cursor: mark old and new cursor regions dirty
                    let cw = 20usize;
                    let ch = 20usize;
                    mark_dirty(&mut dirty_rect, DirtyRect { x: old_cursor_x as usize, y: old_cursor_y as usize, w: cw, h: ch });
                    mark_dirty(&mut dirty_rect, DirtyRect { x: cursor_x as usize, y: cursor_y as usize, w: cw, h: ch });
                }

                // Cursor deltas for drag/resize operations
                let cursor_dx = cursor_x - old_cursor_x;
                let cursor_dy = cursor_y - old_cursor_y;

                // Update cursor shape
                let want_resize = match interaction {
                    Interaction::Resizing { .. } => true,
                    _ => matches!(
                        hit_test(&windows, cursor_x, cursor_y, screen_h, launcher_open),
                        HitZone::ResizeCorner(_)
                    ),
                };
                let wanted_cursor = if want_resize {
                    window::CURSOR_RESIZE
                } else if let Some(idx) = focused_window_idx(&windows) {
                    let hz = hit_test(&windows, cursor_x, cursor_y, screen_h, launcher_open);
                    if matches!(hz, HitZone::Content(_)) {
                        windows[idx].cursor_style
                    } else {
                        window::CURSOR_DEFAULT
                    }
                } else {
                    window::CURSOR_DEFAULT
                };
                if wanted_cursor != current_cursor_style {
                    current_cursor_style = wanted_cursor;
                    let sprite = match wanted_cursor {
                        window::CURSOR_CROSSHAIR => &cursor_crosshair,
                        window::CURSOR_RESIZE => &cursor_resize,
                        _ => &cursor_default,
                    };
                    upload_cursor(cursor_buf, sprite, hw_cursor);
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
                            let _ = ipc::signal(win.fd.fd(), window::MSG_WINDOW_CLOSE);
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
                            let now = Instant::now();
                            let win_fd = windows[new_idx].fd.fd();
                            if Some(win_fd) == last_title_click_fd
                                && now.duration_since(last_title_click_time) < DOUBLE_CLICK_TIME
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
                                last_title_click_fd = None;
                                last_title_click_time = now - DOUBLE_CLICK_TIME;
                            } else {
                                last_title_click_fd = Some(win_fd);
                                last_title_click_time = now;
                                if windows[new_idx].mode != WindowMode::Normal {
                                    // Defer unmaximize until drag threshold is exceeded
                                    interaction = Interaction::DragPending {
                                        window_idx: new_idx,
                                        start_x: cursor_x,
                                        start_y: cursor_y,
                                        was_maximized: true,
                                    };
                                } else {
                                    interaction = Interaction::Dragging {
                                        window_idx: new_idx,
                                    };
                                }
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
                            let _ = ipc::send(win.fd.fd(), window::MSG_MOUSE_INPUT, &ev);
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
                        let _ = ipc::send(
                            windows[idx].fd.fd(),
                            window::MSG_MOUSE_INPUT,
                            &ev,
                        );
                    }
                    match interaction {
                        Interaction::DragPending { .. } => {
                            // Click without dragging — just focus, don't unmaximize
                        }
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
                        Interaction::DragPending {
                            window_idx,
                            start_x,
                            start_y,
                            was_maximized,
                        } => {
                            let dx = cursor_x - start_x;
                            let dy = cursor_y - start_y;
                            if dx.abs() > DRAG_THRESHOLD || dy.abs() > DRAG_THRESHOLD {
                                if was_maximized {
                                    let pixel_format = screen.pixel_format_raw();
                                    let win = &mut windows[window_idx];
                                    // Remember old maximized width for proportional cursor placement
                                    let old_width = win.width + 2 * BORDER_WIDTH;
                                    restore_window(win, pixel_format);
                                    let win = &mut windows[window_idx];
                                    let new_width = win.width + 2 * BORDER_WIDTH;
                                    // Place cursor proportionally on the restored title bar
                                    let ratio = (start_x as usize).min(old_width) as f32
                                        / old_width as f32;
                                    win.content_x = (cursor_x as usize)
                                        .saturating_sub((new_width as f32 * ratio) as usize)
                                        .max(BORDER_WIDTH);
                                    win.content_y = (cursor_y as usize)
                                        .max(BORDER_WIDTH + TITLE_BAR_HEIGHT);
                                    mark_dirty(
                                        &mut dirty_rect,
                                        DirtyRect::full(screen_w as usize, screen_h as usize),
                                    );
                                }
                                interaction = Interaction::Dragging { window_idx };
                            }
                        }
                        Interaction::Dragging { window_idx } => {
                            let old_rect = window_screen_rect(&windows[window_idx]);
                            let win = &mut windows[window_idx];
                            let min_x = BORDER_WIDTH as i32;
                            let min_y = (BORDER_WIDTH + TITLE_BAR_HEIGHT) as i32;
                            win.content_x =
                                (win.content_x as i32 + cursor_dx).max(min_x) as usize;
                            win.content_y =
                                (win.content_y as i32 + cursor_dy).max(min_y) as usize;
                            let new_rect = window_screen_rect(&windows[window_idx]);
                            mark_dirty(&mut dirty_rect, old_rect.union(new_rect));
                        }
                        Interaction::Resizing { window_idx } => {
                            let old_rect = window_screen_rect(&windows[window_idx]);
                            let win = &mut windows[window_idx];
                            win.width = (win.width as i32 + cursor_dx)
                                .max(MIN_CONTENT_WIDTH as i32)
                                as usize;
                            win.height = (win.height as i32 + cursor_dy)
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
                                let _ = ipc::send(
                                    windows[idx].fd.fd(),
                                    window::MSG_MOUSE_INPUT,
                                    &ev,
                                );
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
                            let _ = ipc::send(windows[idx].fd.fd(), window::MSG_MOUSE_INPUT, &ev);
                        }
                    }
                }

                prev_buttons = buttons;
            }
        }

        // Collect client messages to process (fd, pid, header).
        // We collect first to avoid borrowing windows while mutating it.
        let mut client_msgs: Vec<(Fd, u32, ipc::IpcHeader)> = Vec::new();

        if listener_ready {
            let conn = services::accept(&listener_fd).expect("accept failed");
            let raw_fd = conn.fd.fd();
            if let Ok(header) = ipc::recv_header(raw_fd) {
                client_msgs.push((raw_fd, conn.client_pid, header));
                conn.fd.into_raw();
            }
        }

        let mut dead_fds: Vec<Fd> = Vec::new();
        for i in 0..windows.len() {
            if ready_tokens.contains(&(windows[i].fd.fd().0 as u64)) {
                let win = &mut windows[i];
                // Non-blocking read into per-client header buffer
                match syscall::read_nonblock(win.fd.fd(), &mut win.recv_buf[win.recv_len..]) {
                    Ok(0) => dead_fds.push(win.fd.fd()),
                    Ok(n) => {
                        win.recv_len += n;
                        if win.recv_len >= 8 {
                            let header = ipc::IpcHeader {
                                msg_type: u32::from_ne_bytes([win.recv_buf[0], win.recv_buf[1], win.recv_buf[2], win.recv_buf[3]]),
                                len: u32::from_ne_bytes([win.recv_buf[4], win.recv_buf[5], win.recv_buf[6], win.recv_buf[7]]),
                            };
                            win.recv_len = 0;
                            client_msgs.push((win.fd.fd(), win.pid, header));
                        }
                        // else: partial header, continue next iteration
                    }
                    Err(_) => {} // WouldBlock — no data yet
                }
            }
        }
        if !dead_fds.is_empty() {
            windows.retain(|w| !dead_fds.contains(&w.fd.fd()));
            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
        }

        for (client_fd, client_pid, header) in client_msgs {
            match header.msg_type {
                window::MSG_CREATE_WINDOW => {
                    let req: window::CreateWindowRequest = ipc::recv_payload(client_fd, &header).unwrap();
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
                        let chrome_w = BORDER_WIDTH * 2;
                        let chrome_h = BORDER_WIDTH * 2 + TITLE_BAR_HEIGHT;
                        win_w = req_w + chrome_w;
                        win_h = req_h + chrome_h;
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
                    let shm = SharedMemory::allocate(buf_size);
                    shm.grant(client_pid);
                    let token = shm.token();
                    let pixel_format = screen.pixel_format_raw();

                    let topmost = req.flags & window::WINDOW_FLAG_TOPMOST != 0;
                    windows.push(WindowState {
                        fd: OwnedFd::new(client_fd),
                        pid: client_pid,
                        shm,
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
                        cursor_style: window::CURSOR_DEFAULT,
                        recv_buf: [0; 8],
                        recv_len: 0,
                    });

                    // Register new client fd in io_uring (token = fd number)
                    ring_submit(client_fd.0, client_fd.0 as u64, IORING_POLL_IN);

                    let _ = ipc::send(
                        client_fd,
                        window::MSG_WINDOW_CREATED,
                        &window::WindowInfo {
                            token,
                            width: content_w as u32,
                            height: content_h as u32,
                            stride: content_w as u32,
                            pixel_format,
                        },
                    );
                    mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w, screen_h));
                }
                window::MSG_PRESENT => {
                    if let Some(win) = windows.iter_mut().find(|w| w.fd.fd() == client_fd) {
                        win.presented = true;
                        mark_dirty(&mut dirty_rect, window_screen_rect(win));
                    }
                }
                window::MSG_DESTROY_WINDOW => {
                    if let Some(idx) = windows.iter().position(|w| w.fd.fd() == client_fd) {
                        windows.remove(idx);
                        mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                    }
                }
                window::MSG_CLIPBOARD_SET => {
                    let mut buf = [0u8; 116];
                    let n = ipc::recv_bytes(client_fd, &header, &mut buf).unwrap();
                    clipboard = String::from_utf8_lossy(&buf[..n]).into_owned();
                }
                window::MSG_CLIPBOARD_SET_SHM => {
                    let info: window::ClipboardShmMsg = ipc::recv_payload(client_fd, &header).unwrap();
                    let shm = SharedMemory::map(info.token, info.len as usize);
                    clipboard = String::from_utf8_lossy(&shm.as_slice()[..info.len as usize]).into_owned();
                }
                window::MSG_SET_CURSOR => {
                    let style: u32 = ipc::recv_payload(client_fd, &header).unwrap();
                    if let Some(win) = windows.iter_mut().find(|w| w.fd.fd() == client_fd) {
                        win.cursor_style = style as u8;
                    }
                }
                window::MSG_SET_RESOLUTION => {
                    let req: window::ResolutionRequest = ipc::recv_payload(client_fd, &header).unwrap();
                    let reply = match gpu::set_resolution(req.width, req.height) {
                        Ok(new_fb_info) => {
                            fb_info = new_fb_info;
                            let new_fb_size = fb_info.stride as usize * fb_info.height as usize * 4;
                            fb_shm = SharedMemory::map(fb_info.token[0], new_fb_size);
                            screen = Framebuffer::new(
                                fb_shm.as_ptr(),
                                fb_info.width as usize,
                                fb_info.height as usize,
                                fb_info.stride as usize,
                                fb_info.pixel_format,
                            );
                            screen_w = screen.width() as i32;
                            screen_h = screen.height() as i32;
                            wallpaper = scale_wallpaper(
                                wallpaper_pixels,
                                wallpaper_w,
                                wallpaper_h,
                                screen.width(),
                                screen.height(),
                                screen.pixel_format_raw() != 0,
                            );

                            let sw = screen_w as usize;
                            let sh = screen_h as usize;
                            let pf = screen.pixel_format_raw();
                            for win in &mut windows {
                                match win.mode {
                                    WindowMode::Maximized => maximize_window(win, sw, sh, pf),
                                    WindowMode::SnappedLeft => snap_left(win, sw, sh, pf),
                                    WindowMode::SnappedRight => snap_right(win, sw, sh, pf),
                                    WindowMode::Normal => {
                                        let win_w = win.width + BORDER_WIDTH * 2;
                                        let win_h = win.height + BORDER_WIDTH * 2 + TITLE_BAR_HEIGHT;
                                        let max_x = sw.saturating_sub(win_w);
                                        let max_y = sh.saturating_sub(win_h + TASKBAR_HEIGHT);
                                        let cx = win.content_x.saturating_sub(BORDER_WIDTH);
                                        let cy = win.content_y.saturating_sub(BORDER_WIDTH + TITLE_BAR_HEIGHT);
                                        win.content_x = cx.min(max_x) + BORDER_WIDTH;
                                        win.content_y = cy.min(max_y) + BORDER_WIDTH + TITLE_BAR_HEIGHT;
                                    }
                                }
                            }

                            cursor_x = cursor_x.min(screen_w - 1);
                            cursor_y = cursor_y.min(screen_h - 1);

                            mark_dirty(&mut dirty_rect, DirtyRect::full(sw, sh));

                            window::ResolutionInfo { width: fb_info.width, height: fb_info.height }
                        }
                        Err(_) => {
                            window::ResolutionInfo { width: fb_info.width, height: fb_info.height }
                        }
                    };
                    let _ = ipc::send(client_fd, window::MSG_RESOLUTION_CHANGED, &reply);
                }
                window::MSG_GET_RESOLUTION => {
                    let reply = window::ResolutionInfo {
                        width: fb_info.width,
                        height: fb_info.height,
                    };
                    let _ = ipc::send(client_fd, window::MSG_RESOLUTION_CHANGED, &reply);
                }
                _ => {}
            }
        }
        // Re-arm one-shot POLL_ADDs for fds that fired
        if kb_ready { ring_submit(kb_fd.fd().0, token_kb, IORING_POLL_IN); }
        if mouse_ready { ring_submit(mouse_fd.fd().0, token_mouse, IORING_POLL_IN); }
        if listener_ready { ring_submit(listener_fd.fd().0, token_listener, IORING_POLL_IN); }
        for win in windows.iter() {
            let token = win.fd.fd().0 as u64;
            if ready_tokens.contains(&token) {
                ring_submit(win.fd.fd().0, token, IORING_POLL_IN);
            }
        }
        } // end inner drain loop

        // Refresh taskbar once per second for clock + stats
        let now = Instant::now();
        if now.duration_since(last_taskbar_update) >= Duration::from_secs(1) {
            last_taskbar_update = now;

            let mut si = [0u8; 48];
            if system::sysinfo(&mut si) >= 48 {
                let total_mem = u64::from_le_bytes(si[0..8].try_into().unwrap());
                let used_mem = u64::from_le_bytes(si[8..16].try_into().unwrap());
                let busy = u64::from_le_bytes(si[32..40].try_into().unwrap());
                let total = u64::from_le_bytes(si[40..48].try_into().unwrap());
                let d_busy = busy.saturating_sub(prev_busy_ticks);
                let d_total = total.saturating_sub(prev_total_ticks);
                if d_total > 0 {
                    cpu_pct = d_busy.saturating_mul(100) / d_total;
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
                    let sprite = match current_cursor_style {
                        window::CURSOR_CROSSHAIR => &cursor_crosshair,
                        window::CURSOR_RESIZE => &cursor_resize,
                        _ => &cursor_default,
                    };
                    draw_software_cursor(&screen, sprite, cursor_x, cursor_y);
                }

                gpu::present(rect.x as u32, rect.y as u32, rect.w as u32, rect.h as u32);

                // Send frame callbacks to windows that presented and were composited
                for win in windows.iter_mut() {
                    if win.presented && !win.minimized && rect.overlaps(window_screen_rect(win)) {
                        let _ = ipc::signal(win.fd.fd(),window::MSG_FRAME);
                        win.presented = false;
                    }
                }

            }
        }
    }
}

//! Turning a compose plan into pixels.
//!
//! **Nothing here touches the scanout.** Everything composes into
//! [`BackBuffer`], one screen of system RAM, and the finished rectangles are
//! handed to the panel whole — which is why nothing on screen is ever
//! half-composed, and why `fill_rect` no longer reads the row it is
//! replicating back out of a mapping firmware made uncached.
//!
//! Every decision this module acts on — what is visible, in what order, clipped
//! to what, which pixels of a client's buffer reach the screen — is
//! `toyos_desktop`'s and host-tested there. What is left here is the drawing.

use toyos_desktop::{content_blit, plan, Desk, Layer, Rect, Stack};
use window::{Color, Framebuffer};

use crate::client::Client;

pub const FOCUSED_TITLE_COLOR: Color = Color { r: 0x3a, g: 0x3a, b: 0x4e };
pub const UNFOCUSED_TITLE_COLOR: Color = Color { r: 0x28, g: 0x28, b: 0x32 };
pub const FOCUSED_BORDER_COLOR: Color = Color { r: 0x58, g: 0x58, b: 0x6e };
pub const UNFOCUSED_BORDER_COLOR: Color = Color { r: 0x38, g: 0x38, b: 0x42 };
pub const FOCUSED_TITLE_TEXT: Color = Color { r: 0xe0, g: 0xe0, b: 0xe8 };
pub const UNFOCUSED_TITLE_TEXT: Color = Color { r: 0x60, g: 0x60, b: 0x70 };
pub const CLOSE_BUTTON_BG: Color = Color { r: 0x50, g: 0x28, b: 0x28 };
pub const TASKBAR_COLOR: Color = Color { r: 0x18, g: 0x18, b: 0x25 };
pub const TASKBAR_ACTIVE_COLOR: Color = Color { r: 0x30, g: 0x30, b: 0x45 };
pub const TASKBAR_TEXT_COLOR: Color = Color { r: 0x80, g: 0x80, b: 0x90 };
pub const TASKBAR_ACTIVE_TEXT: Color = Color { r: 0xe0, g: 0xe0, b: 0xe8 };
pub const TASKBAR_NEW_COLOR: Color = Color { r: 0x40, g: 0x60, b: 0x40 };
pub const TASKBAR_NEW_TEXT: Color = Color { r: 0x80, g: 0xc0, b: 0x80 };
pub const TASKBAR_MINIMIZED_COLOR: Color = Color { r: 0x20, g: 0x20, b: 0x30 };
pub const TASKBAR_MINIMIZED_TEXT: Color = Color { r: 0x50, g: 0x50, b: 0x60 };
pub const LAUNCHER_BG: Color = Color { r: 0x20, g: 0x20, b: 0x30 };
pub const LAUNCHER_TEXT: Color = Color { r: 0xe0, g: 0xe0, b: 0xe8 };

/// Glyph cell height of the prebuilt font, which every vertical centring uses.
const GLYPH_H: i32 = 16;

pub struct TitleBarIcons {
    pub minimize: sprite::Sprite,
    pub maximize: sprite::Sprite,
    pub close: sprite::Sprite,
}

/// The desktop as it will look, one screen of system RAM.
///
/// `surface` points into `pixels`, so the two are replaced together and the
/// vector is never grown.
pub struct BackBuffer {
    pixels: Vec<u8>,
    pub surface: Framebuffer,
}

impl BackBuffer {
    pub fn new(width: usize, height: usize, pixel_format: u32) -> Self {
        let mut pixels = vec![0u8; width * height * 4];
        let surface = Framebuffer::new(pixels.as_mut_ptr(), width, height, width, pixel_format);
        Self { pixels, surface }
    }

    /// The rows of `region`, ready to hand to the panel.
    pub fn region(&self, region: Rect) -> &[u8] {
        &self.pixels[(region.y0 as usize * self.surface.width() + region.x0 as usize) * 4..]
    }
}

pub struct SystemStats {
    pub used_mb: u64,
    pub total_mb: u64,
    pub cpu_pct: u64,
}

/// Everything the renderer needs that is not state.
pub struct Assets<'a> {
    pub font: &'a font::Font,
    pub icons: &'a TitleBarIcons,
    pub wallpaper: &'a [u8],
    pub apps: &'a [(&'static str, &'static str)],
}

fn fill(surface: &Framebuffer, r: Rect, color: Color) {
    if r.is_empty() {
        return;
    }
    surface.fill_rect(r.x0 as usize, r.y0 as usize, r.w() as usize, r.h() as usize, color);
}

/// Compose `region` of the desktop into `back`.
pub fn paint(
    back: &Framebuffer,
    desk: &Desk,
    stack: &Stack<Client>,
    assets: &Assets,
    launcher_open: bool,
    stats: &SystemStats,
    region: Rect,
) {
    let focused = stack.focused();
    for layer in plan::compose(desk, stack, region, launcher_open) {
        match layer {
            Layer::Wallpaper(r) => {
                let offset = (r.y0 as usize * back.width() + r.x0 as usize) * 4;
                back.blit(
                    r.x0 as usize,
                    r.y0 as usize,
                    r.w() as usize,
                    r.h() as usize,
                    back.width(),
                    &assets.wallpaper[offset..],
                );
            }
            Layer::Window { index, clip } => {
                draw_window(back, desk, &stack[index], Some(index) == focused, assets, clip)
            }
            Layer::Taskbar { clip } => draw_taskbar(back, desk, stack, focused, assets, stats, clip),
            Layer::Launcher(r) => draw_launcher(back, desk, stack.len(), assets, r),
        }
    }
}

fn draw_window(
    surface: &Framebuffer,
    desk: &Desk,
    win: &crate::client::Win,
    focused: bool,
    assets: &Assets,
    clip: Rect,
) {
    let chrome = &desk.chrome;
    let border_color = if focused { FOCUSED_BORDER_COLOR } else { UNFOCUSED_BORDER_COLOR };
    let title_color = if focused { FOCUSED_TITLE_COLOR } else { UNFOCUSED_TITLE_COLOR };
    let text_color = if focused { FOCUSED_TITLE_TEXT } else { UNFOCUSED_TITLE_TEXT };

    let frame = win.frame(chrome);
    let strip = chrome.title_strip(frame);
    if clip.overlaps(strip) {
        fill(surface, strip, border_color);
        // The title bar's own colour, inset by the border on three sides. Its
        // bottom edge is the content's top edge: a window's chrome has no rule
        // between the title and what the client draws.
        let bar = Rect::corners(
            strip.x0 + chrome.border,
            strip.y0 + chrome.border,
            strip.x1 - chrome.border,
            strip.y1,
        );
        fill(surface, bar, title_color);

        let title = if win.title.is_empty() { "Window" } else { &win.title };
        assets.font.draw_string(
            surface,
            (bar.x0 + 8) as usize,
            (bar.y0 + (chrome.title_bar - GLYPH_H) / 2) as usize,
            title,
            text_color,
            title_color,
        );

        let [close, maximize, minimize] = chrome.buttons(frame);
        let close_bg = if focused { CLOSE_BUTTON_BG } else { title_color };
        for (rect, bg, icon) in [
            (close, close_bg, &assets.icons.close),
            (maximize, title_color, &assets.icons.maximize),
            (minimize, title_color, &assets.icons.minimize),
        ] {
            fill(surface, rect, bg);
            draw_icon_centered(surface, icon, rect);
        }
    }

    // Side and bottom borders, each clipped: the colour does not depend on the
    // region, so drawing them outside it would be correct and wasted.
    for edge in [
        Rect::corners(frame.x0, win.content.y0, win.content.x0, win.content.y1),
        Rect::corners(win.content.x1, win.content.y0, frame.x1, win.content.y1),
        Rect::corners(frame.x0, win.content.y1, frame.x1, frame.y1),
    ] {
        fill(surface, edge.intersect(clip), border_color);
    }

    if let Some(b) = content_blit(win, clip) {
        let buffer = unsafe {
            std::slice::from_raw_parts(win.client.shm.as_ptr(), win.client.shm.len())
        };
        let offset = (b.src_y as usize * win.buf_w as usize + b.src_x as usize) * 4;
        surface.blit(
            b.dst.x0 as usize,
            b.dst.y0 as usize,
            b.dst.w() as usize,
            b.dst.h() as usize,
            win.buf_w as usize,
            &buffer[offset..],
        );
    }
}

fn draw_icon_centered(surface: &Framebuffer, icon: &sprite::Sprite, area: Rect) {
    let ix = area.x0 as usize + (area.w() as usize).saturating_sub(icon.width()) / 2;
    let iy = area.y0 as usize + (area.h() as usize).saturating_sub(icon.height()) / 2;
    icon.draw(
        surface.ptr(),
        surface.stride(),
        surface.width(),
        surface.height(),
        surface.pixel_format_raw(),
        ix,
        iy,
    );
}

/// Paint the parts of the taskbar `clip` reaches.
///
/// Every element tests `clip` for itself, so the clock's second costs the
/// status box and not a bar's worth of tabs and titles.
fn draw_taskbar(
    back: &Framebuffer,
    desk: &Desk,
    stack: &Stack<Client>,
    focused: Option<usize>,
    assets: &Assets,
    stats: &SystemStats,
    clip: Rect,
) {
    let bar = desk.taskbar(stack.len());
    let text_y = (bar.strip().y0 + (desk.chrome.taskbar - GLYPH_H) / 2) as usize;

    let gap = bar.gap();
    if clip.overlaps(gap) {
        fill(back, gap, TASKBAR_COLOR);
    }

    for (i, win) in stack.iter().enumerate() {
        let tab = bar.tab(i);
        if !clip.overlaps(tab) {
            continue;
        }
        let (bg, fg) = if win.minimized {
            (TASKBAR_MINIMIZED_COLOR, TASKBAR_MINIMIZED_TEXT)
        } else if Some(i) == focused {
            (TASKBAR_ACTIVE_COLOR, TASKBAR_ACTIVE_TEXT)
        } else {
            (TASKBAR_COLOR, TASKBAR_TEXT_COLOR)
        };
        fill(back, tab, TASKBAR_COLOR);
        fill(back, bar.tab_face(i), bg);
        let max_chars = (desk.chrome.taskbar_item as usize - 16) / assets.font.width();
        let title = if win.title.is_empty() { "Window" } else { &win.title };
        let display: String = title.chars().take(max_chars).collect();
        assets.font.draw_string(back, tab.x0 as usize + 8, text_y, &display, fg, bg);
    }

    let plus = bar.new_button();
    if clip.overlaps(plus) {
        fill(back, plus, TASKBAR_COLOR);
        fill(back, bar.new_button_face(), TASKBAR_NEW_COLOR);
        let plus_x = plus.x0 as usize + (desk.chrome.taskbar as usize - 8) / 2;
        assets.font.draw_char(back, plus_x, text_y, '+', TASKBAR_NEW_TEXT, TASKBAR_NEW_COLOR);
    }

    let status = bar.status();
    if clip.overlaps(status) {
        // Dashes and not 00:00 on a machine whose clock never answered: a
        // reader cannot tell a plausible midnight from a missing clock.
        let clock = match toyos::system::clock_realtime() {
            Some(time) => format!("{:02}:{:02}", time.hours, time.minutes),
            None => String::from("--:--"),
        };
        let text: String =
            format!("{}M/{}M  CPU {}%  {clock}", stats.used_mb, stats.total_mb, stats.cpu_pct)
                .chars()
                .take(toyos_desktop::MAX_STATUS_CHARS)
                .collect();
        fill(back, status, TASKBAR_COLOR);
        let text_w = text.chars().count() * assets.font.width();
        let x = (status.x1 as usize)
            .saturating_sub(toyos_desktop::STATUS_MARGIN as usize + text_w);
        assets.font.draw_string(back, x, text_y, &text, TASKBAR_ACTIVE_TEXT, TASKBAR_COLOR);
    }
}

fn draw_launcher(
    surface: &Framebuffer,
    desk: &Desk,
    windows: usize,
    assets: &Assets,
    area: Rect,
) {
    fill(surface, area, LAUNCHER_BG);
    let bar = desk.taskbar(windows);
    for (i, (name, _)) in assets.apps.iter().enumerate() {
        let row = bar.launcher_item(i);
        assets.font.draw_string(
            surface,
            row.x0 as usize + 12,
            (row.y0 + (row.h() - GLYPH_H) / 2) as usize,
            name,
            LAUNCHER_TEXT,
            LAUNCHER_BG,
        );
    }
}

/// Scale an RGB image to the screen and convert to its pixel format.
pub fn scale_wallpaper(
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

/// Render the cursor sprite (RGBA) into a 64x64 BGRA hardware cursor buffer.
pub fn upload_cursor(cursor_buf: *mut u8, sprite: &sprite::Sprite, hw_cursor: bool) {
    let data = sprite.data();
    let w = sprite.width();
    let h = sprite.height();
    unsafe {
        core::ptr::write_bytes(cursor_buf, 0, 64 * 64 * 4);
    }
    for y in 0..h.min(64) {
        for x in 0..w.min(64) {
            let si = (y * w + x) * 4;
            let di = (y * 64 + x) * 4;
            unsafe {
                let dst = cursor_buf.add(di);
                *dst = data[si + 2];
                *dst.add(1) = data[si + 1];
                *dst.add(2) = data[si];
                *dst.add(3) = data[si + 3];
            }
        }
    }
    if hw_cursor {
        toyos::gpu::set_cursor(0, 0).expect("compositor owns the framebuffer");
    }
}

/// Draw the cursor sprite into the composed frame (software cursor fallback).
///
/// It blends, so it reads the pixel under every partly transparent one — which
/// is why it draws into the back buffer and not the panel.
pub fn draw_software_cursor(surface: &Framebuffer, sprite: &sprite::Sprite, at: toyos_desktop::Point) {
    let data = sprite.data();
    let sw = sprite.width();
    let sh = sprite.height();
    let width = surface.width();
    let height = surface.height();

    for sy in 0..sh {
        let py = at.y as usize + sy;
        if py >= height {
            break;
        }
        for sx in 0..sw {
            let px = at.x as usize + sx;
            if px >= width {
                break;
            }
            let si = (sy * sw + sx) * 4;
            let alpha = data[si + 3] as u32;
            if alpha == 0 {
                continue;
            }
            let sr = data[si] as u32;
            let sg = data[si + 1] as u32;
            let sb = data[si + 2] as u32;
            if alpha == 255 {
                surface.put_pixel(px, py, Color { r: sr as u8, g: sg as u8, b: sb as u8 });
            } else {
                let bg = surface.get_pixel(px, py);
                let inv = 255 - alpha;
                let r = ((sr * alpha + bg.r as u32 * inv) / 255) as u8;
                let g = ((sg * alpha + bg.g as u32 * inv) / 255) as u8;
                let b = ((sb * alpha + bg.b as u32 * inv) / 255) as u8;
                surface.put_pixel(px, py, Color { r, g, b });
            }
        }
    }
}

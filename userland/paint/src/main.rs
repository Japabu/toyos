use filepicker_api::PickerMode;
use font::Font;
use std::collections::VecDeque;
use window::{Color, Event, Framebuffer, KeyEvent, MouseEvent, Window};

// Layout
const TOOLBAR_HEIGHT: usize = 32;
const PALETTE_WIDTH: usize = 40;
const STATUS_HEIGHT: usize = 20;
const TOOL_BTN_W: usize = 40;
const TOOL_BTN_H: usize = 28;
const TOOL_BTN_GAP: usize = 2;
const TOOL_BTN_Y: usize = 2;
const BRUSH_BTN_SIZE: usize = 28;
const SWATCH_SIZE: usize = 16;
const SWATCH_GAP: usize = 2;
const PALETTE_COLS: usize = 2;
const MAX_UNDO: usize = 20;

// Colors
const TOOLBAR_BG: Color = Color { r: 0x2B, g: 0x2B, b: 0x35 };
const TOOLBAR_BTN: Color = Color { r: 0x3C, g: 0x3C, b: 0x4A };
const TOOLBAR_BTN_SEL: Color = Color { r: 0x5A, g: 0x5A, b: 0x70 };
const TOOLBAR_FG: Color = Color { r: 0xDD, g: 0xDD, b: 0xEE };
const PALETTE_BG: Color = Color { r: 0x2B, g: 0x2B, b: 0x35 };
const CANVAS_BG: Color = Color { r: 0xFF, g: 0xFF, b: 0xFF };
const SELECTED_BORDER: Color = Color { r: 0xFF, g: 0xFF, b: 0xFF };
const STATUS_BG: Color = Color { r: 0x22, g: 0x22, b: 0x2A };
const STATUS_FG: Color = Color { r: 0xAA, g: 0xAA, b: 0xBB };

// HID keycodes
const KEY_DELETE: u8 = 0x4C;

const TOOLS: &[(Tool, &str, char)] = &[
    (Tool::Pencil, "Pen", 'p'),
    (Tool::Line, "Line", 'l'),
    (Tool::Rectangle, "Rect", 'r'),
    (Tool::Ellipse, "Elli", 'e'),
    (Tool::Fill, "Fill", 'f'),
    (Tool::Eraser, "Eras", 'x'),
    (Tool::Spray, "Spry", 's'),
    (Tool::Eyedropper, "Pick", 'i'),
];

const BRUSH_SIZES: [usize; 3] = [1, 4, 8];

const PALETTE: [Color; 28] = [
    Color { r: 0x00, g: 0x00, b: 0x00 },
    Color { r: 0xFF, g: 0xFF, b: 0xFF },
    Color { r: 0x80, g: 0x80, b: 0x80 },
    Color { r: 0xC0, g: 0xC0, b: 0xC0 },
    Color { r: 0xFF, g: 0x00, b: 0x00 },
    Color { r: 0x80, g: 0x00, b: 0x00 },
    Color { r: 0xFF, g: 0x80, b: 0x00 },
    Color { r: 0xFF, g: 0xA5, b: 0x00 },
    Color { r: 0xFF, g: 0xFF, b: 0x00 },
    Color { r: 0x80, g: 0x80, b: 0x00 },
    Color { r: 0x00, g: 0xFF, b: 0x00 },
    Color { r: 0x00, g: 0x80, b: 0x00 },
    Color { r: 0x00, g: 0xFF, b: 0xFF },
    Color { r: 0x00, g: 0x80, b: 0x80 },
    Color { r: 0x00, g: 0x00, b: 0xFF },
    Color { r: 0x00, g: 0x00, b: 0x80 },
    Color { r: 0xFF, g: 0x00, b: 0xFF },
    Color { r: 0x80, g: 0x00, b: 0x80 },
    Color { r: 0xFF, g: 0x69, b: 0xB4 },
    Color { r: 0xFF, g: 0xC0, b: 0xCB },
    Color { r: 0x8B, g: 0x45, b: 0x13 },
    Color { r: 0xD2, g: 0xB4, b: 0x8C },
    Color { r: 0xA0, g: 0x52, b: 0x2D },
    Color { r: 0xFF, g: 0xDE, b: 0xAD },
    Color { r: 0x40, g: 0xE0, b: 0xD0 },
    Color { r: 0x7B, g: 0x68, b: 0xEE },
    Color { r: 0xFF, g: 0x63, b: 0x47 },
    Color { r: 0x32, g: 0xCD, b: 0x32 },
];

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Pencil,
    Line,
    Rectangle,
    Ellipse,
    Fill,
    Eraser,
    Spray,
    Eyedropper,
}

impl Tool {
    fn is_shape(self) -> bool {
        matches!(self, Tool::Line | Tool::Rectangle | Tool::Ellipse)
    }

    fn name(self) -> &'static str {
        match self {
            Tool::Pencil => "Pencil",
            Tool::Line => "Line",
            Tool::Rectangle => "Rectangle",
            Tool::Ellipse => "Ellipse",
            Tool::Fill => "Fill",
            Tool::Eraser => "Eraser",
            Tool::Spray => "Spray",
            Tool::Eyedropper => "Eyedropper",
        }
    }
}

enum HitZone {
    ToolButton(usize),
    BrushButton(usize),
    FilledToggle,
    PaletteSwatch(usize),
    Canvas(usize, usize),
    None,
}

// Simple LCG random for spray
struct Rng(u32);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1103515245).wrapping_add(12345);
        self.0
    }
    fn next_isize(&mut self, range: isize) -> isize {
        if range == 0 { return 0; }
        (self.next() as isize).rem_euclid(range)
    }
}

struct PaintApp {
    window: Window,
    font: Font,

    canvas_buf: Vec<u8>,
    canvas_w: usize,
    canvas_h: usize,
    pixel_format: u32,

    undo_stack: Vec<Vec<u8>>,
    redo_stack: Vec<Vec<u8>>,

    tool: Tool,
    color: Color,
    brush_index: usize,
    filled: bool,

    drawing: bool,
    drag_start: (isize, isize),
    last_draw: (isize, isize),

    // Cursor tracking (canvas coordinates, -1 = not on canvas)
    cursor_x: isize,
    cursor_y: isize,

    save_path: Option<String>,
    dirty: bool,
    rng: Rng,
}

impl PaintApp {
    fn new() -> Self {
        let window = Window::create_with_title(0, 0, "Paint");
        window.set_cursor(window::CURSOR_CROSSHAIR);
        let fb = window.framebuffer();
        let canvas_w = fb.width().saturating_sub(PALETTE_WIDTH);
        let canvas_h = fb.height().saturating_sub(TOOLBAR_HEIGHT + STATUS_HEIGHT);
        let pixel_format = fb.pixel_format_raw();
        let canvas_buf = Self::make_canvas_buf(canvas_w, canvas_h, pixel_format);

        let font_bytes = std::fs::read("/share/fonts/JetBrainsMono-Regular-8x16.font")
            .expect("failed to read font");
        let font = Font::from_prebuilt(&font_bytes);

        Self {
            window,
            font,
            canvas_buf,
            canvas_w,
            canvas_h,
            pixel_format,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            tool: Tool::Pencil,
            color: Color { r: 0, g: 0, b: 0 },
            brush_index: 0,
            filled: false,
            drawing: false,
            drag_start: (0, 0),
            last_draw: (0, 0),
            cursor_x: -1,
            cursor_y: -1,
            save_path: None,
            dirty: true,
            rng: Rng(12345),
        }
    }

    fn make_canvas_buf(w: usize, h: usize, pixel_format: u32) -> Vec<u8> {
        let pixel = Self::encode_color(CANVAS_BG, pixel_format);
        let mut buf = vec![0u8; w * h * 4];
        for chunk in buf.chunks_exact_mut(4) {
            chunk.copy_from_slice(&pixel);
        }
        buf
    }

    #[inline]
    fn encode_color(color: Color, pixel_format: u32) -> [u8; 4] {
        if pixel_format == 0 {
            [color.r, color.g, color.b, 0]
        } else {
            [color.b, color.g, color.r, 0]
        }
    }

    #[inline]
    fn decode_color(bytes: &[u8], pixel_format: u32) -> Color {
        if pixel_format == 0 {
            Color { r: bytes[0], g: bytes[1], b: bytes[2] }
        } else {
            Color { r: bytes[2], g: bytes[1], b: bytes[0] }
        }
    }

    fn brush_radius(&self) -> usize {
        BRUSH_SIZES[self.brush_index]
    }

    fn draw_color(&self) -> Color {
        match self.tool {
            Tool::Eraser => CANVAS_BG,
            _ => self.color,
        }
    }

    fn canvas_view_w(&self) -> usize {
        self.window.width() as usize - PALETTE_WIDTH
    }

    // --- Canvas pixel access ---

    fn get_pixel(&self, x: usize, y: usize) -> Color {
        if x < self.canvas_w && y < self.canvas_h {
            let off = (y * self.canvas_w + x) * 4;
            Self::decode_color(&self.canvas_buf[off..off + 4], self.pixel_format)
        } else {
            CANVAS_BG
        }
    }

    fn set_pixel(&mut self, x: usize, y: usize, color: Color) {
        if x < self.canvas_w && y < self.canvas_h {
            let off = (y * self.canvas_w + x) * 4;
            let encoded = Self::encode_color(color, self.pixel_format);
            self.canvas_buf[off..off + 4].copy_from_slice(&encoded);
        }
    }

    // --- Drawing primitives ---

    fn stamp_circle(&mut self, cx: isize, cy: isize, radius: usize, color: Color) {
        let r = radius as isize;
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r * r {
                    let px = cx + dx;
                    let py = cy + dy;
                    if px >= 0 && py >= 0 {
                        self.set_pixel(px as usize, py as usize, color);
                    }
                }
            }
        }
    }

    fn stamp_circle_on_fb(fb: &Framebuffer, cx: isize, cy: isize, radius: usize, color: Color, ox: usize, oy: usize) {
        let r = radius as isize;
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r * r {
                    let px = cx + dx + ox as isize;
                    let py = cy + dy + oy as isize;
                    if px >= 0 && py >= 0 {
                        fb.put_pixel(px as usize, py as usize, color);
                    }
                }
            }
        }
    }

    fn draw_line_on_canvas(&mut self, x0: isize, y0: isize, x1: isize, y1: isize, color: Color, radius: usize) {
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx: isize = if x0 < x1 { 1 } else { -1 };
        let sy: isize = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let mut cx = x0;
        let mut cy = y0;
        loop {
            self.stamp_circle(cx, cy, radius, color);
            if cx == x1 && cy == y1 { break; }
            let e2 = 2 * err;
            if e2 >= dy { err += dy; cx += sx; }
            if e2 <= dx { err += dx; cy += sy; }
        }
    }

    fn draw_line_on_fb(fb: &Framebuffer, x0: isize, y0: isize, x1: isize, y1: isize, color: Color, radius: usize, ox: usize, oy: usize) {
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx: isize = if x0 < x1 { 1 } else { -1 };
        let sy: isize = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let mut cx = x0;
        let mut cy = y0;
        loop {
            Self::stamp_circle_on_fb(fb, cx, cy, radius, color, ox, oy);
            if cx == x1 && cy == y1 { break; }
            let e2 = 2 * err;
            if e2 >= dy { err += dy; cx += sx; }
            if e2 <= dx { err += dx; cy += sy; }
        }
    }

    fn draw_rect_on_canvas(&mut self, x0: isize, y0: isize, x1: isize, y1: isize, color: Color, radius: usize) {
        let lx = x0.min(x1);
        let rx = x0.max(x1);
        let ty = y0.min(y1);
        let by = y0.max(y1);
        self.draw_line_on_canvas(lx, ty, rx, ty, color, radius);
        self.draw_line_on_canvas(rx, ty, rx, by, color, radius);
        self.draw_line_on_canvas(rx, by, lx, by, color, radius);
        self.draw_line_on_canvas(lx, by, lx, ty, color, radius);
    }

    fn draw_rect_on_fb(fb: &Framebuffer, x0: isize, y0: isize, x1: isize, y1: isize, color: Color, radius: usize, ox: usize, oy: usize) {
        let lx = x0.min(x1);
        let rx = x0.max(x1);
        let ty = y0.min(y1);
        let by = y0.max(y1);
        Self::draw_line_on_fb(fb, lx, ty, rx, ty, color, radius, ox, oy);
        Self::draw_line_on_fb(fb, rx, ty, rx, by, color, radius, ox, oy);
        Self::draw_line_on_fb(fb, rx, by, lx, by, color, radius, ox, oy);
        Self::draw_line_on_fb(fb, lx, by, lx, ty, color, radius, ox, oy);
    }

    fn fill_rect_on_canvas(&mut self, x0: isize, y0: isize, x1: isize, y1: isize, color: Color) {
        let lx = x0.min(x1).max(0) as usize;
        let rx = x0.max(x1).max(0) as usize;
        let ty = y0.min(y1).max(0) as usize;
        let by = y0.max(y1).max(0) as usize;
        for y in ty..=by.min(self.canvas_h.saturating_sub(1)) {
            for x in lx..=rx.min(self.canvas_w.saturating_sub(1)) {
                self.set_pixel(x, y, color);
            }
        }
    }

    fn fill_rect_on_fb(fb: &Framebuffer, x0: isize, y0: isize, x1: isize, y1: isize, color: Color, ox: usize, oy: usize) {
        let lx = (x0.min(x1) + ox as isize).max(0) as usize;
        let ty = (y0.min(y1) + oy as isize).max(0) as usize;
        let w = (x0 - x1).unsigned_abs() + 1;
        let h = (y0 - y1).unsigned_abs() + 1;
        fb.fill_rect(lx, ty, w, h, color);
    }

    fn draw_ellipse_on_canvas(&mut self, cx: isize, cy: isize, rx: isize, ry: isize, color: Color, radius: usize) {
        if rx <= 0 || ry <= 0 {
            self.stamp_circle(cx, cy, radius, color);
            return;
        }
        self.draw_ellipse_points(cx, cy, rx, ry, |s, px, py| {
            s.stamp_circle(px, py, radius, color);
        });
    }

    fn fill_ellipse_on_canvas(&mut self, cx: isize, cy: isize, rx: isize, ry: isize, color: Color) {
        if rx <= 0 || ry <= 0 {
            self.set_pixel(cx.max(0) as usize, cy.max(0) as usize, color);
            return;
        }
        let rx2 = (rx as i64) * (rx as i64);
        let ry2 = (ry as i64) * (ry as i64);
        for dy in -ry..=ry {
            let dy2 = (dy as i64) * (dy as i64);
            let max_x = (((rx2 * (ry2 - dy2)) as f64 / ry2 as f64).sqrt()) as isize;
            for dx in -max_x..=max_x {
                let px = cx + dx;
                let py = cy + dy;
                if px >= 0 && py >= 0 {
                    self.set_pixel(px as usize, py as usize, color);
                }
            }
        }
    }

    fn draw_ellipse_on_fb(fb: &Framebuffer, cx: isize, cy: isize, rx: isize, ry: isize, color: Color, radius: usize, ox: usize, oy: usize) {
        if rx <= 0 || ry <= 0 {
            Self::stamp_circle_on_fb(fb, cx, cy, radius, color, ox, oy);
            return;
        }
        let rx2 = rx * rx;
        let ry2 = ry * ry;
        let mut x: isize = 0;
        let mut y: isize = ry;
        let mut px: isize = 0;
        let mut py: isize = 2 * rx2 * y;

        let plot = |px: isize, py: isize| {
            Self::stamp_circle_on_fb(fb, px, py, radius, color, ox, oy);
        };

        plot(cx + x, cy + y);
        plot(cx - x, cy + y);
        plot(cx + x, cy - y);
        plot(cx - x, cy - y);

        let mut p = ry2 - rx2 * ry + rx2 / 4;
        while px < py {
            x += 1;
            px += 2 * ry2;
            if p < 0 { p += ry2 + px; } else { y -= 1; py -= 2 * rx2; p += ry2 + px - py; }
            plot(cx + x, cy + y); plot(cx - x, cy + y);
            plot(cx + x, cy - y); plot(cx - x, cy - y);
        }

        p = ry2 * (x * 2 + 1) * (x * 2 + 1) / 4 + rx2 * (y - 1) * (y - 1) - rx2 * ry2;
        while y > 0 {
            y -= 1;
            py -= 2 * rx2;
            if p > 0 { p += rx2 - py; } else { x += 1; px += 2 * ry2; p += rx2 - py + px; }
            plot(cx + x, cy + y); plot(cx - x, cy + y);
            plot(cx + x, cy - y); plot(cx - x, cy - y);
        }
    }

    fn fill_ellipse_on_fb(fb: &Framebuffer, cx: isize, cy: isize, rx: isize, ry: isize, color: Color, ox: usize, oy: usize) {
        if rx <= 0 || ry <= 0 { return; }
        let rx2 = (rx as i64) * (rx as i64);
        let ry2 = (ry as i64) * (ry as i64);
        for dy in -ry..=ry {
            let dy2 = (dy as i64) * (dy as i64);
            let max_x = (((rx2 * (ry2 - dy2)) as f64 / ry2 as f64).sqrt()) as isize;
            let y = cy + dy + oy as isize;
            let x_start = cx - max_x + ox as isize;
            if y >= 0 && x_start >= 0 {
                fb.fill_rect(x_start as usize, y as usize, (max_x * 2 + 1) as usize, 1, color);
            }
        }
    }

    fn draw_ellipse_points(&mut self, cx: isize, cy: isize, rx: isize, ry: isize, mut plot: impl FnMut(&mut Self, isize, isize)) {
        let rx2 = rx * rx;
        let ry2 = ry * ry;
        let mut x: isize = 0;
        let mut y: isize = ry;
        let mut px: isize = 0;
        let mut py: isize = 2 * rx2 * y;

        plot(self, cx + x, cy + y); plot(self, cx - x, cy + y);
        plot(self, cx + x, cy - y); plot(self, cx - x, cy - y);

        let mut p = ry2 - rx2 * ry + rx2 / 4;
        while px < py {
            x += 1; px += 2 * ry2;
            if p < 0 { p += ry2 + px; } else { y -= 1; py -= 2 * rx2; p += ry2 + px - py; }
            plot(self, cx + x, cy + y); plot(self, cx - x, cy + y);
            plot(self, cx + x, cy - y); plot(self, cx - x, cy - y);
        }

        p = ry2 * (x * 2 + 1) * (x * 2 + 1) / 4 + rx2 * (y - 1) * (y - 1) - rx2 * ry2;
        while y > 0 {
            y -= 1; py -= 2 * rx2;
            if p > 0 { p += rx2 - py; } else { x += 1; px += 2 * ry2; p += rx2 - py + px; }
            plot(self, cx + x, cy + y); plot(self, cx - x, cy + y);
            plot(self, cx + x, cy - y); plot(self, cx - x, cy - y);
        }
    }

    fn spray(&mut self, cx: isize, cy: isize, radius: usize, color: Color) {
        let r = radius as isize * 3;
        for _ in 0..20 {
            let dx = self.rng.next_isize(r * 2 + 1) - r;
            let dy = self.rng.next_isize(r * 2 + 1) - r;
            if dx * dx + dy * dy <= r * r {
                let px = cx + dx;
                let py = cy + dy;
                if px >= 0 && py >= 0 {
                    self.set_pixel(px as usize, py as usize, color);
                }
            }
        }
    }

    fn flood_fill(&mut self, sx: usize, sy: usize, fill_color: Color) {
        if sx >= self.canvas_w || sy >= self.canvas_h { return; }
        let target = self.get_pixel(sx, sy);
        if target == fill_color { return; }
        let mut queue = VecDeque::new();
        queue.push_back((sx, sy));
        while let Some((x, y)) = queue.pop_front() {
            if x >= self.canvas_w || y >= self.canvas_h { continue; }
            if self.get_pixel(x, y) != target { continue; }
            self.set_pixel(x, y, fill_color);
            if x > 0 { queue.push_back((x - 1, y)); }
            if y > 0 { queue.push_back((x, y - 1)); }
            queue.push_back((x + 1, y));
            queue.push_back((x, y + 1));
        }
    }

    // --- Undo ---

    fn push_undo(&mut self) {
        if self.undo_stack.len() >= MAX_UNDO {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(self.canvas_buf.clone());
        self.redo_stack.clear();
    }

    fn pop_undo(&mut self) {
        if let Some(snapshot) = self.undo_stack.pop() {
            self.redo_stack.push(self.canvas_buf.clone());
            self.canvas_buf = snapshot;
            self.dirty = true;
        }
    }

    fn pop_redo(&mut self) {
        if let Some(snapshot) = self.redo_stack.pop() {
            self.undo_stack.push(self.canvas_buf.clone());
            self.canvas_buf = snapshot;
            self.dirty = true;
        }
    }

    // --- Save ---

    fn save(&self) {
        let path = match &self.save_path {
            Some(p) => p,
            None => return,
        };
        // Write PPM (P6 binary RGB)
        let header = format!("P6\n{} {}\n255\n", self.canvas_w, self.canvas_h);
        let mut data = Vec::with_capacity(header.len() + self.canvas_w * self.canvas_h * 3);
        data.extend_from_slice(header.as_bytes());
        for pixel in self.canvas_buf.chunks_exact(4) {
            let c = Self::decode_color(pixel, self.pixel_format);
            data.push(c.r);
            data.push(c.g);
            data.push(c.b);
        }
        if let Err(e) = std::fs::write(path, &data) {
            eprintln!("Save error: {}", e);
        }
    }

    fn load_ppm(&mut self, path: &str) {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to open {}: {}", path, e);
                return;
            }
        };
        // Parse PPM P6 header: "P6\n<width> <height>\n<maxval>\n"
        let mut pos = 0;
        let next_token = |p: &mut usize| -> Option<String> {
            // Skip whitespace and comments
            while *p < data.len() {
                if data[*p] == b'#' {
                    while *p < data.len() && data[*p] != b'\n' { *p += 1; }
                } else if data[*p].is_ascii_whitespace() {
                    *p += 1;
                } else {
                    break;
                }
            }
            let start = *p;
            while *p < data.len() && !data[*p].is_ascii_whitespace() { *p += 1; }
            if start == *p { return None; }
            String::from_utf8(data[start..*p].to_vec()).ok()
        };
        let magic = next_token(&mut pos).unwrap_or_default();
        if magic != "P6" {
            eprintln!("Not a PPM P6 file: {}", path);
            return;
        }
        let w: usize = next_token(&mut pos).and_then(|s| s.parse().ok()).unwrap_or(0);
        let h: usize = next_token(&mut pos).and_then(|s| s.parse().ok()).unwrap_or(0);
        let _maxval = next_token(&mut pos);
        // Skip exactly one whitespace byte after maxval
        pos += 1;
        if w == 0 || h == 0 || pos + w * h * 3 > data.len() {
            eprintln!("Invalid PPM dimensions in {}", path);
            return;
        }
        // Blit into canvas (clipped to canvas size)
        let blit_w = w.min(self.canvas_w);
        let blit_h = h.min(self.canvas_h);
        for y in 0..blit_h {
            for x in 0..blit_w {
                let src = pos + (y * w + x) * 3;
                let c = Color { r: data[src], g: data[src + 1], b: data[src + 2] };
                let encoded = Self::encode_color(c, self.pixel_format);
                let dst = (y * self.canvas_w + x) * 4;
                self.canvas_buf[dst..dst + 4].copy_from_slice(&encoded);
            }
        }
        self.save_path = Some(path.to_string());
        self.dirty = true;
    }

    // --- Hit testing ---

    fn toolbar_tool_x(idx: usize) -> usize {
        4 + idx * (TOOL_BTN_W + TOOL_BTN_GAP)
    }

    fn toolbar_brush_x(idx: usize) -> usize {
        Self::toolbar_tool_x(TOOLS.len()) + 12 + idx * (BRUSH_BTN_SIZE + TOOL_BTN_GAP)
    }

    fn toolbar_filled_x() -> usize {
        Self::toolbar_brush_x(BRUSH_SIZES.len()) + 12
    }

    fn toolbar_color_x() -> usize {
        Self::toolbar_filled_x() + BRUSH_BTN_SIZE + 8
    }

    fn hit_test(&self, mx: usize, my: usize) -> HitZone {
        let fw = self.window.width() as usize;

        if my < TOOLBAR_HEIGHT {
            for i in 0..TOOLS.len() {
                let bx = Self::toolbar_tool_x(i);
                if mx >= bx && mx < bx + TOOL_BTN_W && my >= TOOL_BTN_Y && my < TOOL_BTN_Y + TOOL_BTN_H {
                    return HitZone::ToolButton(i);
                }
            }
            for i in 0..BRUSH_SIZES.len() {
                let bx = Self::toolbar_brush_x(i);
                if mx >= bx && mx < bx + BRUSH_BTN_SIZE && my >= TOOL_BTN_Y && my < TOOL_BTN_Y + BRUSH_BTN_SIZE {
                    return HitZone::BrushButton(i);
                }
            }
            let fx = Self::toolbar_filled_x();
            if mx >= fx && mx < fx + BRUSH_BTN_SIZE && my >= TOOL_BTN_Y && my < TOOL_BTN_Y + BRUSH_BTN_SIZE {
                return HitZone::FilledToggle;
            }
            return HitZone::None;
        }

        // Status bar
        if my >= self.window.height() as usize - STATUS_HEIGHT {
            return HitZone::None;
        }

        let palette_x = fw.saturating_sub(PALETTE_WIDTH);
        if mx >= palette_x && my >= TOOLBAR_HEIGHT {
            let local_x = mx - palette_x;
            let local_y = my - TOOLBAR_HEIGHT;
            let pad = (PALETTE_WIDTH - PALETTE_COLS * (SWATCH_SIZE + SWATCH_GAP) + SWATCH_GAP) / 2;
            let col = local_x.saturating_sub(pad) / (SWATCH_SIZE + SWATCH_GAP);
            let row = local_y.saturating_sub(4) / (SWATCH_SIZE + SWATCH_GAP);
            if col < PALETTE_COLS {
                let idx = row * PALETTE_COLS + col;
                if idx < PALETTE.len() {
                    return HitZone::PaletteSwatch(idx);
                }
            }
            return HitZone::None;
        }

        if my >= TOOLBAR_HEIGHT && mx < fw.saturating_sub(PALETTE_WIDTH) {
            let cx = mx;
            let cy = my - TOOLBAR_HEIGHT;
            if cx < self.canvas_w && cy < self.canvas_h {
                return HitZone::Canvas(cx, cy);
            }
        }

        HitZone::None
    }

    // --- Rendering ---

    fn render_all(&self) {
        let fb = self.window.framebuffer();
        let fw = fb.width();
        let fh = fb.height();

        // Toolbar background
        fb.fill_rect(0, 0, fw, TOOLBAR_HEIGHT, TOOLBAR_BG);

        // Tool buttons
        for (i, (tool, label, _)) in TOOLS.iter().enumerate() {
            let bx = Self::toolbar_tool_x(i);
            let bg = if *tool == self.tool { TOOLBAR_BTN_SEL } else { TOOLBAR_BTN };
            fb.fill_rect(bx, TOOL_BTN_Y, TOOL_BTN_W, TOOL_BTN_H, bg);
            let text_x = bx + (TOOL_BTN_W - label.len() * self.font.width()) / 2;
            let text_y = TOOL_BTN_Y + (TOOL_BTN_H - self.font.height()) / 2;
            self.font.draw_string(&fb, text_x, text_y, label, TOOLBAR_FG, bg);
        }

        // Brush size buttons
        for i in 0..BRUSH_SIZES.len() {
            let bx = Self::toolbar_brush_x(i);
            let bg = if i == self.brush_index { TOOLBAR_BTN_SEL } else { TOOLBAR_BTN };
            fb.fill_rect(bx, TOOL_BTN_Y, BRUSH_BTN_SIZE, BRUSH_BTN_SIZE, bg);
            let dot_r = BRUSH_SIZES[i].min(10);
            let center_x = (bx + BRUSH_BTN_SIZE / 2) as isize;
            let center_y = (TOOL_BTN_Y + BRUSH_BTN_SIZE / 2) as isize;
            let r = dot_r as isize;
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx * dx + dy * dy <= r * r {
                        fb.put_pixel((center_x + dx) as usize, (center_y + dy) as usize, TOOLBAR_FG);
                    }
                }
            }
        }

        // Filled toggle button
        let fx = Self::toolbar_filled_x();
        let fbg = if self.filled { TOOLBAR_BTN_SEL } else { TOOLBAR_BTN };
        fb.fill_rect(fx, TOOL_BTN_Y, BRUSH_BTN_SIZE, BRUSH_BTN_SIZE, fbg);
        // Draw a small filled/outline square icon
        let icon_x = fx + 6;
        let icon_y = TOOL_BTN_Y + 6;
        let icon_s = BRUSH_BTN_SIZE - 12;
        if self.filled {
            fb.fill_rect(icon_x, icon_y, icon_s, icon_s, TOOLBAR_FG);
        } else {
            fb.fill_rect(icon_x, icon_y, icon_s, 2, TOOLBAR_FG);
            fb.fill_rect(icon_x, icon_y + icon_s - 2, icon_s, 2, TOOLBAR_FG);
            fb.fill_rect(icon_x, icon_y, 2, icon_s, TOOLBAR_FG);
            fb.fill_rect(icon_x + icon_s - 2, icon_y, 2, icon_s, TOOLBAR_FG);
        }

        // Current color indicator
        let cx = Self::toolbar_color_x();
        fb.fill_rect(cx, TOOL_BTN_Y, BRUSH_BTN_SIZE, BRUSH_BTN_SIZE, self.color);
        for i in 0..BRUSH_BTN_SIZE {
            fb.put_pixel(cx + i, TOOL_BTN_Y, SELECTED_BORDER);
            fb.put_pixel(cx + i, TOOL_BTN_Y + BRUSH_BTN_SIZE - 1, SELECTED_BORDER);
            fb.put_pixel(cx, TOOL_BTN_Y + i, SELECTED_BORDER);
            fb.put_pixel(cx + BRUSH_BTN_SIZE - 1, TOOL_BTN_Y + i, SELECTED_BORDER);
        }

        // Palette sidebar
        let palette_x = fw.saturating_sub(PALETTE_WIDTH);
        fb.fill_rect(palette_x, TOOLBAR_HEIGHT, PALETTE_WIDTH, fh.saturating_sub(TOOLBAR_HEIGHT), PALETTE_BG);
        let pad = (PALETTE_WIDTH - PALETTE_COLS * (SWATCH_SIZE + SWATCH_GAP) + SWATCH_GAP) / 2;
        for (i, &c) in PALETTE.iter().enumerate() {
            let col = i % PALETTE_COLS;
            let row = i / PALETTE_COLS;
            let sx = palette_x + pad + col * (SWATCH_SIZE + SWATCH_GAP);
            let sy = TOOLBAR_HEIGHT + 4 + row * (SWATCH_SIZE + SWATCH_GAP);
            fb.fill_rect(sx, sy, SWATCH_SIZE, SWATCH_SIZE, c);
            if c == self.color {
                for j in 0..SWATCH_SIZE {
                    fb.put_pixel(sx + j, sy, SELECTED_BORDER);
                    fb.put_pixel(sx + j, sy + SWATCH_SIZE - 1, SELECTED_BORDER);
                    fb.put_pixel(sx, sy + j, SELECTED_BORDER);
                    fb.put_pixel(sx + SWATCH_SIZE - 1, sy + j, SELECTED_BORDER);
                }
            }
        }

        // Canvas
        self.render_canvas(&fb, fw);

        // Status bar
        self.render_status(&fb, fw, fh);

    }

    fn render_canvas(&self, fb: &Framebuffer, fw: usize) {
        let view_w = fw.saturating_sub(PALETTE_WIDTH).min(self.canvas_w);
        let view_h = fb.height().saturating_sub(TOOLBAR_HEIGHT + STATUS_HEIGHT).min(self.canvas_h);
        fb.blit(0, TOOLBAR_HEIGHT, view_w, view_h, self.canvas_w, &self.canvas_buf);
    }

    fn render_status(&self, fb: &Framebuffer, fw: usize, fh: usize) {
        let sy = fh.saturating_sub(STATUS_HEIGHT);
        fb.fill_rect(0, sy, fw.saturating_sub(PALETTE_WIDTH), STATUS_HEIGHT, STATUS_BG);

        let mut status = String::new();
        status.push_str(self.tool.name());
        if self.tool.is_shape() {
            status.push_str(if self.filled { " (filled)" } else { " (outline)" });
        }
        if self.cursor_x >= 0 && self.cursor_y >= 0 {
            status.push_str(&format!("  |  {}, {}", self.cursor_x, self.cursor_y));
        }
        status.push_str(&format!("  |  Brush: {}px", self.brush_radius()));

        let text_y = sy + (STATUS_HEIGHT - self.font.height()) / 2;
        self.font.draw_string(fb, 4, text_y, &status, STATUS_FG, STATUS_BG);
    }

    fn render_canvas_and_preview(&self, x0: isize, y0: isize, x1: isize, y1: isize) {
        let fb = self.window.framebuffer();
        let fw = fb.width();
        self.render_canvas(&fb, fw);

        let color = self.draw_color();
        let radius = self.brush_radius();
        match self.tool {
            Tool::Line => {
                Self::draw_line_on_fb(&fb, x0, y0, x1, y1, color, radius, 0, TOOLBAR_HEIGHT);
            }
            Tool::Rectangle => {
                if self.filled {
                    Self::fill_rect_on_fb(&fb, x0, y0, x1, y1, color, 0, TOOLBAR_HEIGHT);
                } else {
                    Self::draw_rect_on_fb(&fb, x0, y0, x1, y1, color, radius, 0, TOOLBAR_HEIGHT);
                }
            }
            Tool::Ellipse => {
                let ecx = (x0 + x1) / 2;
                let ecy = (y0 + y1) / 2;
                let erx = (x1 - x0).abs() / 2;
                let ery = (y1 - y0).abs() / 2;
                if self.filled {
                    Self::fill_ellipse_on_fb(&fb, ecx, ecy, erx, ery, color, 0, TOOLBAR_HEIGHT);
                } else {
                    Self::draw_ellipse_on_fb(&fb, ecx, ecy, erx, ery, color, radius, 0, TOOLBAR_HEIGHT);
                }
            }
            _ => {}
        }
    }

    // --- Event handling ---

    fn to_canvas_coords(&self, mx: usize, my: usize) -> (isize, isize) {
        let cvw = self.canvas_view_w();
        let cx = mx.min(cvw.saturating_sub(1)) as isize;
        let cy = my.saturating_sub(TOOLBAR_HEIGHT) as isize;
        (cx, cy)
    }

    fn handle_mouse(&mut self, ev: MouseEvent) {
        let mx = ev.x as usize;
        let my = ev.y as usize;

        // Update cursor position
        let fw = self.window.width() as usize;
        let fh = self.window.height() as usize;
        let palette_x = fw.saturating_sub(PALETTE_WIDTH);
        if my >= TOOLBAR_HEIGHT && my < fh.saturating_sub(STATUS_HEIGHT) && mx < palette_x {
            self.cursor_x = mx as isize;
            self.cursor_y = (my - TOOLBAR_HEIGHT) as isize;
        } else {
            self.cursor_x = -1;
            self.cursor_y = -1;
        }

        match ev.event_type {
            // Right-click = eyedropper (pick color from canvas)
            window::MOUSE_PRESS if ev.changed & 2 != 0 => {
                if let HitZone::Canvas(cx, cy) = self.hit_test(mx, my) {
                    self.color = self.get_pixel(cx, cy);
                    self.dirty = true;
                }
            }
            window::MOUSE_PRESS if ev.changed & 1 != 0 => {
                match self.hit_test(mx, my) {
                    HitZone::ToolButton(i) => {
                        self.tool = TOOLS[i].0;
                        self.dirty = true;
                    }
                    HitZone::BrushButton(i) => {
                        self.brush_index = i;
                        self.dirty = true;
                    }
                    HitZone::FilledToggle => {
                        self.filled = !self.filled;
                        self.dirty = true;
                    }
                    HitZone::PaletteSwatch(i) => {
                        self.color = PALETTE[i];
                        self.dirty = true;
                    }
                    HitZone::Canvas(cx, cy) => {
                        let color = self.draw_color();
                        let radius = self.brush_radius();
                        match self.tool {
                            Tool::Eyedropper => {
                                self.color = self.get_pixel(cx, cy);
                                self.dirty = true;
                            }
                            Tool::Fill => {
                                self.push_undo();
                                self.flood_fill(cx, cy, color);
                                self.dirty = true;
                            }
                            Tool::Pencil | Tool::Eraser => {
                                self.push_undo();
                                self.drawing = true;
                                self.drag_start = (cx as isize, cy as isize);
                                self.last_draw = (cx as isize, cy as isize);
                                self.stamp_circle(cx as isize, cy as isize, radius, color);
                                let fb = self.window.framebuffer();
                                Self::stamp_circle_on_fb(&fb, cx as isize, cy as isize, radius, color, 0, TOOLBAR_HEIGHT);
                                self.window.present();
                            }
                            Tool::Spray => {
                                self.push_undo();
                                self.drawing = true;
                                self.last_draw = (cx as isize, cy as isize);
                                self.spray(cx as isize, cy as isize, radius, color);
                                self.dirty = true;
                            }
                            Tool::Line | Tool::Rectangle | Tool::Ellipse => {
                                self.push_undo();
                                self.drawing = true;
                                self.drag_start = (cx as isize, cy as isize);
                                self.last_draw = (cx as isize, cy as isize);
                            }
                        }
                    }
                    HitZone::None => {}
                }
            }
            window::MOUSE_MOVE => {
                if self.drawing {
                    let (cxi, cyi) = self.to_canvas_coords(mx, my);
                    match self.tool {
                        Tool::Pencil | Tool::Eraser => {
                            let color = self.draw_color();
                            let radius = self.brush_radius();
                            let (lx, ly) = self.last_draw;
                            self.draw_line_on_canvas(lx, ly, cxi, cyi, color, radius);
                            self.last_draw = (cxi, cyi);
                            let fb = self.window.framebuffer();
                            Self::draw_line_on_fb(&fb, lx, ly, cxi, cyi, color, radius, 0, TOOLBAR_HEIGHT);
                            self.window.present();
                        }
                        Tool::Spray => {
                            let color = self.draw_color();
                            let radius = self.brush_radius();
                            self.spray(cxi, cyi, radius, color);
                            self.last_draw = (cxi, cyi);
                            self.dirty = true;
                        }
                        Tool::Line | Tool::Rectangle | Tool::Ellipse => {
                            let (sx, sy) = self.drag_start;
                            self.render_canvas_and_preview(sx, sy, cxi, cyi);
                            self.window.present();
                        }
                        _ => {}
                    }
                } else {
                    // Not drawing — just update cursor
                    self.dirty = true;
                }
            }
            window::MOUSE_RELEASE if ev.changed & 1 != 0 && self.drawing => {
                let (cxi, cyi) = self.to_canvas_coords(mx, my);
                let color = self.draw_color();
                let radius = self.brush_radius();
                let (sx, sy) = self.drag_start;

                match self.tool {
                    Tool::Line => {
                        self.draw_line_on_canvas(sx, sy, cxi, cyi, color, radius);
                    }
                    Tool::Rectangle => {
                        if self.filled {
                            self.fill_rect_on_canvas(sx, sy, cxi, cyi, color);
                        } else {
                            self.draw_rect_on_canvas(sx, sy, cxi, cyi, color, radius);
                        }
                    }
                    Tool::Ellipse => {
                        let ecx = (sx + cxi) / 2;
                        let ecy = (sy + cyi) / 2;
                        let erx = (cxi - sx).abs() / 2;
                        let ery = (cyi - sy).abs() / 2;
                        if self.filled {
                            self.fill_ellipse_on_canvas(ecx, ecy, erx, ery, color);
                        } else {
                            self.draw_ellipse_on_canvas(ecx, ecy, erx, ery, color, radius);
                        }
                    }
                    _ => {}
                }
                self.drawing = false;
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, ev: KeyEvent) {
        if ev.released() { return; }

        let cmd = ev.ctrl() || ev.gui();
        let ch = if ev.len > 0 {
            Some(ev.translated[0] as char)
        } else {
            None
        };

        if cmd {
            match ch.map(|c| c.to_ascii_lowercase()) {
                Some('z') => { self.pop_undo(); return; }
                Some('y') => { self.pop_redo(); return; }
                Some('s') => {
                    if self.save_path.is_none() {
                        if let Some(path) = filepicker_api::pick_file(PickerMode::Save, "/") {
                            self.save_path = Some(path);
                        }
                    }
                    self.save();
                    return;
                }
                Some('o') => {
                    if let Some(path) = filepicker_api::pick_file(PickerMode::Open, "/") {
                        self.load_ppm(&path);
                    }
                    return;
                }
                _ => {}
            }
            if ev.keycode == KEY_DELETE {
                self.push_undo();
                let pixel = Self::encode_color(CANVAS_BG, self.pixel_format);
                for chunk in self.canvas_buf.chunks_exact_mut(4) {
                    chunk.copy_from_slice(&pixel);
                }
                self.dirty = true;
                return;
            }
        }

        if let Some(ch) = ch {
            for (i, (_, _, shortcut)) in TOOLS.iter().enumerate() {
                if ch == *shortcut {
                    self.tool = TOOLS[i].0;
                    self.dirty = true;
                    return;
                }
            }
            match ch {
                '1' => { self.brush_index = 0; self.dirty = true; }
                '2' => { self.brush_index = 1; self.dirty = true; }
                '3' => { self.brush_index = 2; self.dirty = true; }
                'g' => { self.filled = !self.filled; self.dirty = true; }
                _ => {}
            }
        }
    }

    fn handle_resize(&mut self) {
        let fb = self.window.framebuffer();
        let new_w = fb.width().saturating_sub(PALETTE_WIDTH);
        let new_h = fb.height().saturating_sub(TOOLBAR_HEIGHT + STATUS_HEIGHT);
        let new_format = fb.pixel_format_raw();

        if new_w > self.canvas_w || new_h > self.canvas_h {
            let grow_w = new_w.max(self.canvas_w);
            let grow_h = new_h.max(self.canvas_h);
            let mut new_buf = Self::make_canvas_buf(grow_w, grow_h, new_format);
            let row_bytes = self.canvas_w * 4;
            for y in 0..self.canvas_h {
                let src_start = y * row_bytes;
                let dst_start = y * grow_w * 4;
                new_buf[dst_start..dst_start + row_bytes]
                    .copy_from_slice(&self.canvas_buf[src_start..src_start + row_bytes]);
            }
            self.canvas_buf = new_buf;
            self.canvas_w = grow_w;
            self.canvas_h = grow_h;
        }
        self.pixel_format = new_format;
        self.dirty = true;
    }

    // --- Main loop ---

    fn run(&mut self) {
        self.render_all();
        self.window.present();
        self.dirty = false;

        loop {
            match self.window.poll_event(50_000_000) {
                Some(Event::MouseInput(ev)) => self.handle_mouse(ev),
                Some(Event::KeyInput(ev)) => self.handle_key(ev),
                Some(Event::Resized) => self.handle_resize(),
                Some(Event::Close) => break,
                _ => {}
            }
            if self.dirty {
                self.render_all();
                self.window.present();
                self.dirty = false;
            }
        }
    }
}

fn main() {
    let mut app = PaintApp::new();
    if let Some(path) = std::env::args().nth(1) {
        app.load_ppm(&path);
    }
    app.run();
}

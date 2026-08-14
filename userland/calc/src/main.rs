//! The window: a strip of keys under a display, laid out from whatever size the
//! surface happens to be.
//!
//! Snake's shape, for the same reason snake has it — one program that runs on
//! the development host and on ToyOS with nothing in it that knows the
//! difference. winit gives it a window and its events, softbuffer gives it a
//! wall of pixels, and everything the calculator actually decides lives in the
//! library beside this file.
//!
//! **Nothing here is a fixed pixel.** [`Layout`] is computed from the surface
//! every frame, so the window resizes: the eight columns and four rows divide
//! what there is and carry the remainder a pixel at a time, and the type is
//! chosen per region from the four cell sizes `build.rs` bakes.

use std::num::NonZeroU32;
use std::sync::Arc;

use calc::app::{enabled, Action, Button, Calc, Mode};
use calc::num::APPROX;
use calc::prog;
use font::Font;
use softbuffer::{Context, Pixel, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowAttributes, WindowId};

/// The size the window opens at.
const OPEN_W: u32 = 600;
const OPEN_H: u32 = 440;

/// Below this the keys stop being keys. Asked of the window as a minimum, and
/// applied to the layout as a floor as well, because a compositor is free to
/// ignore the request.
const MIN_W: i32 = 460;
const MIN_H: i32 = 360;

const COLS: usize = 8;
const ROWS: usize = 4;
/// Columns in the scientific block; the pad is the rest.
const LEFT_COLS: usize = 3;

/// The longest key face, in characters — what the key type has to fit.
const KEY_CHARS: i32 = 3;

// --- the palette, snake's ---

const BG: Pixel = Pixel::new_rgb(0x1a, 0x1a, 0x2e);
const PANEL: Pixel = Pixel::new_rgb(0x22, 0x22, 0x38);
const SUNKEN: Pixel = Pixel::new_rgb(0x18, 0x18, 0x2a);
const KEY_DIGIT: Pixel = Pixel::new_rgb(0x2e, 0x2e, 0x48);
const KEY_OP: Pixel = Pixel::new_rgb(0x34, 0x34, 0x5c);
const KEY_FN: Pixel = Pixel::new_rgb(0x28, 0x28, 0x40);
const KEY_CLEAR: Pixel = Pixel::new_rgb(0x4a, 0x2c, 0x38);
const KEY_EQUALS: Pixel = Pixel::new_rgb(0x2e, 0x6a, 0x3a);
const KEY_ACTIVE: Pixel = Pixel::new_rgb(0x40, 0xb0, 0x40);
const HOVER: Pixel = Pixel::new_rgb(0x12, 0x12, 0x18);
const PRESSED: Pixel = Pixel::new_rgb(0x24, 0x24, 0x30);
const TEXT: font::Color = font::Color { r: 0xe0, g: 0xe0, b: 0xe8 };
const DIM: font::Color = font::Color { r: 0x70, g: 0x70, b: 0x80 };
const OFF: font::Color = font::Color { r: 0x4a, g: 0x4a, b: 0x58 };
const ERROR: font::Color = font::Color { r: 0xe0, g: 0x50, b: 0x50 };

fn as_font_color(p: Pixel) -> font::Color {
    font::Color { r: p.r, g: p.g, b: p.b }
}

fn as_pixel(c: font::Color) -> Pixel {
    Pixel::new_rgb(c.r, c.g, c.b)
}

/// Lighten or darken a key, which is what hovering and pressing it look like.
fn shade(base: Pixel, by: Pixel, up: bool) -> Pixel {
    let mix = |a: u8, b: u8| if up { a.saturating_add(b) } else { a.saturating_sub(b) };
    Pixel::new_rgb(mix(base.r, by.r), mix(base.g, by.g), mix(base.b, by.b))
}

/// Half-open, like every other rectangle in this repository.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    fn inset(&self, by: i32) -> Rect {
        Rect { x: self.x + by, y: self.y + by, w: self.w - 2 * by, h: self.h - 2 * by }
    }
}

/// Divide `total` into `gaps.len() + 1` tracks separated by those gaps.
///
/// The remainder is handed out a pixel at a time to the leading tracks rather
/// than left at one end, so the last track ends exactly where the space does at
/// every window size — which is the whole of what "the keys still line up"
/// means once nothing is a constant.
fn tracks(start: i32, total: i32, gaps: &[i32]) -> Vec<(i32, i32)> {
    let n = gaps.len() as i32 + 1;
    let total = total.max(n);
    let mut gaps: Vec<i32> = gaps.to_vec();
    let mut gap_sum: i32 = gaps.iter().sum();
    // **Gaps give way before tracks do.** A one-pixel key is still a key; a gap
    // that pushes the last key past the end of the row is not a gap. Without
    // this the eight columns overflowed their strip on any window narrow enough
    // that the seams cost more than the keys.
    if gap_sum > total - n {
        let each = (total - n).max(0) / gaps.len().max(1) as i32;
        for g in gaps.iter_mut() {
            *g = (*g).min(each);
        }
        gap_sum = gaps.iter().sum();
    }
    let avail = total - gap_sum;
    let size = avail / n;
    let extra = avail % n;
    let mut out = Vec::with_capacity(n as usize);
    let mut at = start;
    for i in 0..n {
        let w = size + i32::from(i < extra);
        out.push((at, w));
        at += w + gaps.get(i as usize).copied().unwrap_or(0);
    }
    out
}

fn clamp(v: i32, lo: i32, hi: i32) -> i32 {
    v.max(lo).min(hi)
}

/// Where everything goes, for one surface size.
struct Layout {
    tabs: [Rect; 2],
    /// The right end of the tab strip, where the mode's own note sits.
    strip: Rect,
    display: Rect,
    message: Rect,
    panel: Rect,
    keys: [Rect; COLS * ROWS],
    pad: i32,
}

impl Layout {
    fn new(width: i32, height: i32) -> Layout {
        let w = width.max(MIN_W);
        let h = height.max(MIN_H);
        let margin = clamp(w.min(h) / 28, 8, 22);
        let cw = w - 2 * margin;
        let ch = h - 2 * margin;

        let vgap = clamp(ch / 45, 4, 12);
        let tab_h = clamp(ch * 10 / 100, 24, 44);
        let msg_h = clamp(ch * 7 / 100, 14, 26);
        let rest = (ch - tab_h - msg_h - 3 * vgap).max(4 * ROWS as i32);
        let grid_h = rest * 3 / 5;
        let display_h = rest - grid_h;

        let tab_y = margin;
        let display_y = tab_y + tab_h + vgap;
        let message_y = display_y + display_h + vgap;
        let grid_y = message_y + msg_h + vgap;

        let tab_w = clamp(cw / 7, 52, 96);
        let tab_gap = clamp(cw / 80, 4, 10);
        let tabs = [
            Rect { x: margin, y: tab_y, w: tab_w, h: tab_h },
            Rect { x: margin + tab_w + tab_gap, y: tab_y, w: tab_w, h: tab_h },
        ];

        let gap = clamp(cw / 80, 4, 9);
        // The scientific block and the pad read as two, so the seam between
        // them is wider than the seams inside either.
        let block = gap * 3;
        let col_gaps: Vec<i32> =
            (0..COLS - 1).map(|i| if i == LEFT_COLS - 1 { block } else { gap }).collect();
        let cols = tracks(margin, cw, &col_gaps);
        let rows = tracks(grid_y, grid_h, &vec![gap; ROWS - 1]);

        let mut keys = [Rect { x: 0, y: 0, w: 0, h: 0 }; COLS * ROWS];
        for (i, key) in keys.iter_mut().enumerate() {
            let (x, kw) = cols[i % COLS];
            let (y, kh) = rows[i / COLS];
            *key = Rect { x, y, w: kw, h: kh };
        }

        // The keys sit on a panel that stands a little proud of them. Never
        // more than half the vertical gap, or on a short window the panel
        // reaches up into the message line.
        let outset = clamp(vgap / 2, 2, gap);
        Layout {
            tabs,
            strip: Rect { x: margin, y: tab_y, w: cw, h: tab_h },
            display: Rect { x: margin, y: display_y, w: cw, h: display_h },
            message: Rect { x: margin, y: message_y, w: cw, h: msg_h },
            panel: Rect {
                x: margin - outset,
                y: grid_y - outset,
                w: cw + 2 * outset,
                h: grid_h + 2 * outset,
            },
            keys,
            pad: clamp(cw / 60, 6, 14),
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<Target> {
        for (i, mode) in [Mode::Calc, Mode::Prog].into_iter().enumerate() {
            if self.tabs[i].contains(x, y) {
                return Some(Target::Tab(mode));
            }
        }
        self.keys.iter().position(|k| k.contains(x, y)).map(Target::Key)
    }
}

/// What the pointer is over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    Tab(Mode),
    Key(usize),
}

/// The pixel buffer, as something that can be drawn on.
///
/// `font::Canvas::put_pixel` takes `&self`, so the buffer is reached through a
/// raw pointer — snake does the same, for the same reason.
struct Canvas {
    ptr: *mut Pixel,
    width: usize,
    height: usize,
}

impl Canvas {
    fn new(pixels: &mut [Pixel], width: usize, height: usize) -> Canvas {
        Canvas { ptr: pixels.as_mut_ptr(), width, height }
    }

    fn set(&self, x: i32, y: i32, color: Pixel) {
        if x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height {
            unsafe { *self.ptr.add(y as usize * self.width + x as usize) = color };
        }
    }

    fn fill(&self, r: Rect, color: Pixel) {
        for row in 0..r.h {
            for col in 0..r.w {
                self.set(r.x + col, r.y + row, color);
            }
        }
    }

    /// A one-pixel outline, which is how a pressed key says so.
    fn outline(&self, r: Rect, color: Pixel) {
        self.fill(Rect { h: 1, ..r }, color);
        self.fill(Rect { y: r.y + r.h - 1, h: 1, ..r }, color);
        self.fill(Rect { w: 1, ..r }, color);
        self.fill(Rect { x: r.x + r.w - 1, w: 1, ..r }, color);
    }

    fn text(&self, f: &Font, x: i32, y: i32, s: &str, fg: font::Color, bg: Pixel) {
        if x < 0 || y < 0 {
            return;
        }
        f.draw_string(self, x as usize, y as usize, s, fg, as_font_color(bg));
    }

    fn text_centred(&self, f: &Font, r: Rect, s: &str, fg: font::Color, bg: Pixel) {
        let chars = s.chars().count() as i32;
        let x = r.x + (r.w - chars * f.width() as i32) / 2;
        let y = r.y + (r.h - f.height() as i32) / 2;
        self.text(f, x, y, s, fg, bg);
    }
}

impl font::Canvas for Canvas {
    fn put_pixel(&self, x: usize, y: usize, color: font::Color) {
        self.set(x as i32, y as i32, Pixel::new_rgb(color.r, color.g, color.b));
    }
}

/// The four cell sizes, largest first. A result that will not fit at one is
/// drawn at the next; nothing is ever cut to make it fit.
struct Fonts {
    scaled: [Font; 4],
}

impl Fonts {
    fn load() -> Fonts {
        Fonts {
            scaled: [
                Font::from_prebuilt(include_bytes!(concat!(
                    env!("OUT_DIR"),
                    "/JetBrainsMono-Regular-12x24.font"
                ))),
                Font::from_prebuilt(include_bytes!(concat!(
                    env!("OUT_DIR"),
                    "/JetBrainsMono-Regular-10x20.font"
                ))),
                Font::from_prebuilt(include_bytes!(concat!(
                    env!("OUT_DIR"),
                    "/JetBrainsMono-Regular-8x16.font"
                ))),
                Font::from_prebuilt(include_bytes!(concat!(
                    env!("OUT_DIR"),
                    "/JetBrainsMono-Regular-6x12.font"
                ))),
            ],
        }
    }

    fn smallest(&self) -> &Font {
        &self.scaled[3]
    }

    /// The largest cell that puts `chars` characters inside `w` by `h`.
    fn fitting(&self, chars: i32, w: i32, h: i32) -> &Font {
        self.scaled
            .iter()
            .find(|f| chars * f.width() as i32 <= w && f.height() as i32 <= h)
            .unwrap_or_else(|| self.smallest())
    }

    /// The largest cell that draws `text` whole in as few rows as it can, up to
    /// `lines`. Shrinking comes first and wrapping second, and if the smallest
    /// cell still needs more rows than that it gets them: the alternative is
    /// cutting digits off a number, which this never does.
    fn fit(&self, text: &str, width: i32, lines: usize) -> (&Font, Vec<String>) {
        let count = text.chars().count();
        for allowed in 1..=lines {
            for f in &self.scaled {
                let per = (width / f.width() as i32).max(1) as usize;
                if count.div_ceil(per) <= allowed {
                    return (f, wrap(text, per));
                }
            }
        }
        let f = self.smallest();
        let per = (width / f.width() as i32).max(1) as usize;
        (f, wrap(text, per))
    }
}

fn wrap(text: &str, per: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(per).map(|c| c.iter().collect()).collect()
}

struct App {
    context: Context<OwnedDisplayHandle>,
    ui: Option<Ui>,
}

struct Ui {
    window: Arc<dyn Window>,
    surface: Surface<OwnedDisplayHandle, Arc<dyn Window>>,
    fonts: Fonts,
    calc: Calc,
    width: u32,
    height: u32,
    hover: Option<Target>,
    pressed: Option<Target>,
}

impl Ui {
    fn new(elwt: &dyn ActiveEventLoop, context: &Context<OwnedDisplayHandle>) -> Ui {
        let attrs = WindowAttributes::default()
            .with_title("Calculator")
            .with_surface_size(PhysicalSize::new(OPEN_W, OPEN_H))
            .with_min_surface_size(PhysicalSize::new(MIN_W as u32, MIN_H as u32));
        let window: Arc<dyn Window> = elwt.create_window(attrs).unwrap().into();
        let size = window.surface_size();
        let mut surface = Surface::new(context, window.clone()).unwrap();
        let (w, h) = (size.width.max(1), size.height.max(1));
        surface.resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap()).unwrap();
        Ui {
            window,
            surface,
            fonts: Fonts::load(),
            calc: Calc::new(),
            width: w,
            height: h,
            hover: None,
            pressed: None,
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        let (w, h) = (width.max(1), height.max(1));
        self.width = w;
        self.height = h;
        self.surface.resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap()).unwrap();
    }

    fn layout(&self) -> Layout {
        Layout::new(self.width as i32, self.height as i32)
    }

    fn key(&mut self, key: &Key) {
        match key {
            Key::Named(NamedKey::Enter) => self.calc.act(Action::Equals),
            Key::Named(NamedKey::Escape) => self.calc.act(Action::Clear),
            Key::Named(NamedKey::Backspace) => self.calc.act(Action::Backspace),
            Key::Named(NamedKey::Delete) => self.calc.act(Action::Delete),
            Key::Named(NamedKey::ArrowLeft) => self.calc.act(Action::Left),
            Key::Named(NamedKey::ArrowRight) => self.calc.act(Action::Right),
            Key::Named(NamedKey::Home) => self.calc.act(Action::Home),
            Key::Named(NamedKey::End) => self.calc.act(Action::End),
            Key::Named(NamedKey::Tab) => {
                let other = match self.calc.mode() {
                    Mode::Calc => Mode::Prog,
                    Mode::Prog => Mode::Calc,
                };
                self.calc.act(Action::SetMode(other));
            }
            Key::Character(s) => {
                for c in s.chars() {
                    if c == '=' {
                        self.calc.act(Action::Equals);
                    } else {
                        self.calc.type_char(c);
                    }
                }
            }
            _ => {}
        }
    }

    fn redraw(&mut self) {
        let layout = self.layout();
        let (w, h) = (self.width as usize, self.height as usize);
        // The scene borrows the fields the drawing reads; the buffer borrows
        // the surface. Two disjoint halves of one `Ui`.
        let scene = Scene {
            calc: &self.calc,
            fonts: &self.fonts,
            layout: &layout,
            hover: self.hover,
            pressed: self.pressed,
        };
        let mut buffer = self.surface.next_buffer().unwrap();
        let pixels = buffer.pixels();
        let canvas = Canvas::new(pixels, w, h);
        canvas.fill(Rect { x: 0, y: 0, w: w as i32, h: h as i32 }, BG);

        scene.draw_tabs(&canvas);
        scene.draw_display(&canvas);
        scene.draw_message(&canvas);
        scene.draw_keys(&canvas);

        buffer.present().unwrap();
    }
}

/// Everything one frame is drawn from, and nothing that can change while it is.
struct Scene<'a> {
    calc: &'a Calc,
    fonts: &'a Fonts,
    layout: &'a Layout,
    hover: Option<Target>,
    pressed: Option<Target>,
}

impl Scene<'_> {
    fn draw_tabs(&self, canvas: &Canvas) {
        for (i, mode) in [Mode::Calc, Mode::Prog].into_iter().enumerate() {
            let rect = self.layout.tabs[i];
            let active = self.calc.mode() == mode;
            let mut base = if active { KEY_EQUALS } else { KEY_FN };
            if self.hover == Some(Target::Tab(mode)) {
                base = shade(base, HOVER, true);
            }
            if self.pressed == Some(Target::Tab(mode)) {
                base = shade(base, PRESSED, false);
            }
            canvas.fill(rect, base);
            if active {
                canvas.outline(rect, KEY_ACTIVE);
            }
            let label = match mode {
                Mode::Calc => "Calc",
                Mode::Prog => "Prog",
            };
            let f = self.fonts.fitting(label.len() as i32, rect.w - 8, rect.h - 8);
            canvas.text_centred(f, rect, label, if active { TEXT } else { DIM }, base);
        }

        // What the layout is standing on, right-aligned in the same strip.
        let note = match self.calc.mode() {
            Mode::Calc => self.calc.angle_label(),
            Mode::Prog => self.calc.base().label(),
        };
        let strip = self.layout.strip;
        let f = self.fonts.fitting(KEY_CHARS, strip.w / 4, strip.h - 8);
        let x = strip.x + strip.w - note.chars().count() as i32 * f.width() as i32;
        canvas.text(f, x, strip.y + (strip.h - f.height() as i32) / 2, note, DIM, BG);
    }

    fn draw_display(&self, canvas: &Canvas) {
        canvas.fill(self.layout.display, SUNKEN);
        let inner = self.layout.display.inset(self.layout.pad);
        match self.calc.mode() {
            Mode::Calc => self.draw_calc_display(canvas, inner),
            Mode::Prog => self.draw_prog_display(canvas, inner),
        }
    }

    /// The entry line, scrolled so the caret is always on it, and the caret.
    /// Returns the height it took.
    fn draw_entry(&self, canvas: &Canvas, inner: Rect, f: &Font) -> i32 {
        let expr = self.calc.expr();
        let per = (inner.w / f.width() as i32).max(1) as usize;
        let caret_at = expr[..self.calc.caret()].chars().count();
        let scroll = caret_at.saturating_sub(per.saturating_sub(1));
        let shown: String = expr.chars().skip(scroll).take(per).collect();
        canvas.text(f, inner.x, inner.y, &shown, TEXT, SUNKEN);
        let caret_x = inner.x + (caret_at - scroll) as i32 * f.width() as i32;
        canvas.fill(
            Rect { x: caret_x, y: inner.y - 2, w: 2, h: f.height() as i32 + 4 },
            as_pixel(TEXT),
        );
        f.height() as i32
    }

    fn draw_calc_display(&self, canvas: &Canvas, inner: Rect) {
        let (f, _) = self.fonts.fit(self.calc.expr(), inner.w, 1);
        self.draw_entry(canvas, inner, f);

        // The result, as large as it fits and wrapped rather than cut.
        let Some(text) = self.calc.preview() else { return };
        let (rf, lines) = self.fonts.fit(&text, inner.w, 2);
        let colour = if text.starts_with(APPROX) { DIM } else { TEXT };
        let top = inner.y + inner.h - lines.len() as i32 * rf.height() as i32;
        for (i, line) in lines.iter().enumerate() {
            let width = line.chars().count() as i32 * rf.width() as i32;
            canvas.text(
                rf,
                inner.x + inner.w - width,
                top + i as i32 * rf.height() as i32,
                line,
                colour,
                SUNKEN,
            );
        }
    }

    fn draw_prog_display(&self, canvas: &Canvas, inner: Rect) {
        // Four rows of panes under the entry line, all in one cell size: the
        // binary pane is 35 characters and the widest thing here, so it decides.
        let f = self.fonts.fitting(40, inner.w, inner.h / 6);
        let cell = f.height() as i32;
        self.draw_entry(canvas, inner, f);

        let value = self.calc.value();
        let (high, low) = prog::pane_bin(value);
        let rows: [(&str, &str); 4] = [
            ("HEX", &prog::pane_hex(value)),
            ("DEC", &prog::pane_dec(value)),
            ("BIN", &high),
            ("", &low),
        ];
        let label_w = 4 * f.width() as i32;
        let step = ((inner.h - cell) / rows.len() as i32).max(cell);
        for (i, (label, text)) in rows.iter().enumerate() {
            let y = inner.y + cell + 4 + i as i32 * step;
            let active = self.calc.base().label() == *label;
            canvas.text(f, inner.x, y, label, if active { TEXT } else { DIM }, SUNKEN);
            let width = text.chars().count() as i32 * f.width() as i32;
            canvas.text(f, inner.x + (inner.w - width).max(label_w), y, text, TEXT, SUNKEN);
        }
    }

    fn draw_message(&self, canvas: &Canvas) {
        let Some(message) = self.calc.message() else { return };
        let r = self.layout.message;
        let (f, lines) = self.fonts.fit(message, r.w, 1);
        canvas.text(f, r.x, r.y + (r.h - f.height() as i32) / 2, &lines[0], ERROR, BG);
    }

    fn draw_keys(&self, canvas: &Canvas) {
        canvas.fill(self.layout.panel, PANEL);
        let first = self.layout.keys[0];
        let f = self.fonts.fitting(KEY_CHARS, first.w - 8, first.h - 8);
        for (i, button) in self.calc.buttons().iter().enumerate() {
            let rect = self.layout.keys[i];
            let live = enabled(button, self.calc.mode(), self.calc.base());
            let on = is_on(button, self.calc);
            let mut colour = if on { KEY_EQUALS } else { key_colour(button) };
            if !live {
                colour = KEY_FN;
            } else if self.hover == Some(Target::Key(i)) {
                colour = shade(colour, HOVER, true);
            }
            if self.pressed == Some(Target::Key(i)) && live {
                colour = shade(colour, PRESSED, false);
            }
            canvas.fill(rect, colour);
            if self.pressed == Some(Target::Key(i)) && live {
                canvas.outline(rect, KEY_ACTIVE);
            }
            let label = match button.action {
                Action::ToggleAngle => self.calc.angle_label(),
                _ => button.label,
            };
            canvas.text_centred(f, rect, label, if live { TEXT } else { OFF }, colour);
        }
    }
}

/// Whether this button shows the state it selects, rather than an action.
fn is_on(button: &Button, calc: &Calc) -> bool {
    match button.action {
        Action::SetBase(base) => calc.mode() == Mode::Prog && calc.base() == base,
        _ => false,
    }
}

fn key_colour(button: &Button) -> Pixel {
    match button.action {
        Action::Equals => KEY_EQUALS,
        Action::Clear | Action::Backspace => KEY_CLEAR,
        Action::SetBase(_) | Action::ToggleAngle => KEY_FN,
        // A value looks like a value whether it is a digit or a constant, and a
        // function looks like a function whether it is spelled or drawn.
        Action::Insert("π") => KEY_DIGIT,
        Action::Insert("√") => KEY_FN,
        Action::Insert(text) if text.ends_with('(') => KEY_FN,
        Action::Insert(text) => {
            let value = text.chars().count() == 1
                && text.chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '.');
            if value {
                KEY_DIGIT
            } else {
                KEY_OP
            }
        }
        _ => KEY_OP,
    }
}

impl ApplicationHandler for App {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        if self.ui.is_none() {
            self.ui = Some(Ui::new(event_loop, &self.context));
        }
    }

    fn window_event(&mut self, event_loop: &dyn ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(ui) = self.ui.as_mut() else { return };
        let mut dirty = false;
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
                return;
            }
            WindowEvent::SurfaceResized(size) => {
                ui.resize(size.width, size.height);
                // Whatever the pointer was over is somewhere else now.
                ui.hover = None;
                ui.pressed = None;
                dirty = true;
            }
            WindowEvent::RedrawRequested => {
                ui.redraw();
                return;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    ui.key(&event.logical_key);
                    dirty = true;
                }
            }
            WindowEvent::PointerMoved { position, .. } => {
                let over = ui.layout().hit(position.x as i32, position.y as i32);
                if over != ui.hover {
                    ui.hover = over;
                    dirty = true;
                }
            }
            WindowEvent::PointerLeft { .. } => {
                if ui.hover.is_some() || ui.pressed.is_some() {
                    ui.hover = None;
                    ui.pressed = None;
                    dirty = true;
                }
            }
            WindowEvent::PointerButton { state, position, button, .. } => {
                if button.mouse_button() != Some(MouseButton::Left) {
                    return;
                }
                let over = ui.layout().hit(position.x as i32, position.y as i32);
                ui.hover = over;
                match state {
                    ElementState::Pressed => ui.pressed = over,
                    ElementState::Released => {
                        // A press only counts where it started and ended on the
                        // same key.
                        if let (Some(down), Some(up)) = (ui.pressed, over) {
                            if down == up {
                                match up {
                                    Target::Tab(mode) => ui.calc.act(Action::SetMode(mode)),
                                    Target::Key(i) => {
                                        let button = &ui.calc.buttons()[i];
                                        if enabled(button, ui.calc.mode(), ui.calc.base()) {
                                            let action = button.action;
                                            ui.calc.act(action);
                                        }
                                    }
                                }
                            }
                        }
                        ui.pressed = None;
                    }
                }
                dirty = true;
            }
            _ => {}
        }
        if dirty {
            ui.window.request_redraw();
        }
    }
}

fn main() {
    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    let context = Context::new(event_loop.owned_display_handle()).unwrap();
    event_loop.run_app(App { context, ui: None }).unwrap();
}

/// Every character outside ASCII that the panel can put on the screen.
///
/// The same set `build.rs` bakes beyond Latin-1, plus the Latin-1 signs the
/// font always carries. A glyph nothing baked draws as `?`, which is a button
/// with a wrong face on it and nothing that fails — so the test below is what
/// makes the two lists agree.
#[cfg(test)]
const DRAWABLE_NON_ASCII: &[char] = &['\u{00B1}', '\u{00D7}', '\u{00F7}', 'π', '←', '−', '√', '≈'];

#[cfg(test)]
mod tests {
    use super::*;
    use calc::app::{CALC_BUTTONS, PROG_BUTTONS};
    use calc::error::EvalError;
    use calc::prog::Base;

    /// Sizes the layout has to hold: the one it opens at, its own floor, one
    /// below that floor, a tall narrow one, a wide short one, and a screen.
    const SIZES: &[(i32, i32)] = &[
        (OPEN_W as i32, OPEN_H as i32),
        (MIN_W, MIN_H),
        (320, 240),
        (480, 900),
        (1400, 400),
        (1920, 1080),
    ];

    /// At every size the keys tile their strip: inside the panel, no two
    /// overlapping, and the last column ending exactly where the space does.
    #[test]
    fn the_keys_tile_the_panel_at_every_size() {
        for &(w, h) in SIZES {
            let l = Layout::new(w, h);
            let (fw, fh) = (w.max(MIN_W), h.max(MIN_H));
            for (i, a) in l.keys.iter().enumerate() {
                assert!(a.w > 0 && a.h > 0, "key {i} is empty at {w}x{h}");
                assert!(a.x >= l.panel.x, "key {i} left of the panel at {w}x{h}");
                assert!(a.x + a.w <= l.panel.x + l.panel.w, "key {i} past the panel at {w}x{h}");
                assert!(a.y + a.h <= fh, "key {i} past the bottom at {w}x{h}");
                for (j, b) in l.keys.iter().enumerate().skip(i + 1) {
                    let apart = a.x + a.w <= b.x
                        || b.x + b.w <= a.x
                        || a.y + a.h <= b.y
                        || b.y + b.h <= a.y;
                    assert!(apart, "keys {i} and {j} overlap at {w}x{h}");
                }
            }
            // The row spans the content exactly, remainder pixels and all.
            let first = l.keys[0];
            let last = l.keys[COLS - 1];
            assert_eq!(first.x, l.display.x, "the grid and the display disagree at {w}x{h}");
            assert_eq!(
                last.x + last.w,
                l.display.x + l.display.w,
                "the grid stops short of the display at {w}x{h}"
            );
            let bottom = l.keys[COLS * ROWS - 1];
            assert!(bottom.y + bottom.h <= fh, "the grid runs off the bottom at {w}x{h}");
            // And the strips above it are in order and do not overlap.
            assert!(l.strip.y + l.strip.h <= l.display.y, "tabs into the display at {w}x{h}");
            assert!(l.display.y + l.display.h <= l.message.y, "display into the message at {w}x{h}");
            assert!(l.message.y + l.message.h <= l.panel.y, "message into the keys at {w}x{h}");
            assert!(l.panel.x >= 0 && l.panel.x + l.panel.w <= fw, "the panel escapes at {w}x{h}");
        }
    }

    #[test]
    fn hit_testing_follows_the_layout_at_every_size() {
        for &(w, h) in SIZES {
            let l = Layout::new(w, h);
            for i in 0..COLS * ROWS {
                let k = l.keys[i];
                assert_eq!(l.hit(k.x + k.w / 2, k.y + k.h / 2), Some(Target::Key(i)));
            }
            assert_eq!(l.hit(l.tabs[0].x + 2, l.tabs[0].y + 2), Some(Target::Tab(Mode::Calc)));
            assert_eq!(l.hit(l.tabs[1].x + 2, l.tabs[1].y + 2), Some(Target::Tab(Mode::Prog)));
            assert_eq!(l.hit(l.display.x, l.display.y + 2), None);
            assert_eq!(l.hit(-1, -1), None);
            // The seam between the two blocks belongs to neither.
            let seam = l.keys[LEFT_COLS - 1];
            assert_eq!(l.hit(seam.x + seam.w + 1, seam.y + seam.h / 2), None);
        }
    }

    /// A forty-digit answer is drawn whole at some cell size, which is the
    /// whole point of carrying four of them.
    #[test]
    fn the_longest_answer_is_never_cut() {
        let fonts = Fonts::load();
        let longest = format!("{APPROX}-1.{}e-100", "9".repeat(39));
        for &(w, h) in SIZES {
            let inner = Layout::new(w, h).display.inset(Layout::new(w, h).pad);
            let (_, lines) = fonts.fit(&longest, inner.w, 2);
            assert_eq!(lines.concat(), longest, "digits went missing at {w}x{h}");
            assert!(lines.len() <= 2, "the answer needed {} lines at {w}x{h}", lines.len());
        }
        // One that genuinely cannot fit is wrapped rather than shortened.
        let absurd = "8".repeat(400);
        let (_, lines) = fonts.fit(&absurd, 200, 2);
        assert_eq!(lines.concat(), absurd);
    }

    /// Nothing the panel can draw names a glyph the font does not carry.
    #[test]
    fn every_face_the_panel_shows_was_baked() {
        let mut faces: Vec<String> = Vec::new();
        for layout in [&CALC_BUTTONS, &PROG_BUTTONS] {
            faces.extend(layout.iter().map(|b| b.label.to_string()));
        }
        faces.extend(["Calc", "Prog", "RAD", "DEG"].map(String::from));
        faces.extend([Base::Hex, Base::Dec, Base::Bin].map(|b| b.label().to_string()));
        faces.push(APPROX.to_string());
        for error in [
            EvalError::Parse("× needs a value before it".into()),
            EvalError::DivisionByZero,
            EvalError::NegativeRoot,
            EvalError::LogOfNonPositive,
            EvalError::ZeroToNonPositivePower,
            EvalError::NegativeBaseFractionalExponent,
            EvalError::Overflow,
            EvalError::ArgumentTooLarge,
            EvalError::NotAnInteger,
            EvalError::OutOfRange,
            EvalError::NegativeShift,
            EvalError::TooDeep,
            EvalError::TooLong,
        ] {
            faces.push(error.message());
        }
        for face in &faces {
            for ch in face.chars() {
                let baked = ch.is_ascii_graphic() || ch == ' ' || DRAWABLE_NON_ASCII.contains(&ch);
                assert!(baked, "{ch:?} (U+{:04X}) in {face:?} is not in the baked font", ch as u32);
            }
        }
        // The message strip is one row tall and draws the first line it is
        // given, so a refusal too long for the narrowest window would be a
        // sentence cut in half. Every one of them fits.
        let strip = Layout::new(MIN_W, MIN_H).message;
        let cell = Fonts::load().smallest().width() as i32;
        for face in faces.iter().filter(|f| f.contains(' ')) {
            let width = face.chars().count() as i32 * cell;
            assert!(width <= strip.w, "{face:?} is {width}px in a {}px message strip", strip.w);
        }
        // And the set is not idle: every character in it is on a face, so a
        // codepoint that stops being drawn stops being baked.
        for &ch in DRAWABLE_NON_ASCII {
            assert!(faces.iter().any(|f| f.contains(ch)), "{ch:?} is baked and nothing draws it");
        }
    }

    /// No key face is wider than the cell it is drawn in, at any size.
    #[test]
    fn every_key_face_fits_its_key() {
        let fonts = Fonts::load();
        let widest = CALC_BUTTONS
            .iter()
            .chain(PROG_BUTTONS.iter())
            .map(|b| b.label.chars().count())
            .max()
            .expect("both layouts have keys");
        assert!(widest as i32 <= KEY_CHARS, "a key face is {widest} characters wide");
        for &(w, h) in SIZES {
            let key = Layout::new(w, h).keys[0];
            let f = fonts.fitting(KEY_CHARS, key.w - 8, key.h - 8);
            assert!(
                KEY_CHARS * f.width() as i32 <= key.w,
                "a three-character face does not fit a {}x{} key at {w}x{h}",
                key.w,
                key.h
            );
        }
    }

    /// The tracks a row divides into span it exactly, whatever the remainder.
    #[test]
    fn tracks_never_lose_a_pixel() {
        for total in 40..400 {
            for gap in [0, 1, 4, 9] {
                let gaps = vec![gap; COLS - 1];
                let out = tracks(7, total, &gaps);
                assert_eq!(out.len(), COLS);
                let last = out[COLS - 1];
                assert_eq!(last.0 + last.1, 7 + total, "total={total} gap={gap}");
                let spread = out.iter().map(|t| t.1).max().unwrap()
                    - out.iter().map(|t| t.1).min().unwrap();
                assert!(spread <= 1, "tracks differ by {spread} at total={total} gap={gap}");
            }
        }
    }
}

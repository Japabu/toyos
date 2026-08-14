//! The window: a fixed-size panel of buttons over a display strip.
//!
//! Snake's shape, for the same reason snake has it — one program that runs on
//! the development host and on ToyOS with nothing in it that knows the
//! difference. winit gives it a window and its events, softbuffer gives it a
//! wall of pixels, and everything the calculator actually decides lives in the
//! library beside this file.

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

// --- the panel, in pixels. Every number below is a fixed layout: the window
// does not resize, and where a compositor hands over a different surface the
// panel is centred in it rather than stretched.

const PANEL_W: i32 = 600;
const PANEL_H: i32 = 434;
const MARGIN: i32 = 20;

const TAB_Y: i32 = 10;
const TAB_H: i32 = 34;
const TAB_W: i32 = 76;
const TAB_GAP: i32 = 8;

const DISPLAY_Y: i32 = 52;
const DISPLAY_H: i32 = 132;
const DISPLAY_PAD: i32 = 10;

const MSG_Y: i32 = 188;
const MSG_H: i32 = 20;

const GRID_Y: i32 = 216;
const BTN_W: i32 = 62;
const BTN_H: i32 = 44;
const BTN_GAP: i32 = 7;
/// The gap between the scientific block and the pad, wider than the rest so the
/// two read as two.
const BLOCK_GAP: i32 = 22;
const LEFT_COLS: i32 = 3;
const COLS: i32 = 8;

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

/// Lighten or darken a key, which is what hovering and pressing it look like.
fn shade(base: Pixel, by: Pixel, up: bool) -> Pixel {
    let mix = |a: u8, b: u8| if up { a.saturating_add(b) } else { a.saturating_sub(b) };
    Pixel::new_rgb(mix(base.r, by.r), mix(base.g, by.g), mix(base.b, by.b))
}

/// Where a button sits, from its index in the row-major thirty-two.
fn button_rect(index: usize) -> (i32, i32, i32, i32) {
    let col = index as i32 % COLS;
    let row = index as i32 / COLS;
    let block = if col >= LEFT_COLS { BLOCK_GAP - BTN_GAP } else { 0 };
    let x = MARGIN + col * (BTN_W + BTN_GAP) + block;
    let y = GRID_Y + row * (BTN_H + BTN_GAP);
    (x, y, BTN_W, BTN_H)
}

fn tab_rect(index: i32) -> (i32, i32, i32, i32) {
    (MARGIN + index * (TAB_W + TAB_GAP), TAB_Y, TAB_W, TAB_H)
}

fn inside(rect: (i32, i32, i32, i32), x: i32, y: i32) -> bool {
    x >= rect.0 && x < rect.0 + rect.2 && y >= rect.1 && y < rect.1 + rect.3
}

/// What the pointer is over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    Tab(Mode),
    Key(usize),
}

fn hit(calc: &Calc, x: i32, y: i32) -> Option<Target> {
    for (i, mode) in [Mode::Calc, Mode::Prog].into_iter().enumerate() {
        if inside(tab_rect(i as i32), x, y) {
            return Some(Target::Tab(mode));
        }
    }
    for i in 0..calc.buttons().len() {
        if inside(button_rect(i), x, y) {
            return Some(Target::Key(i));
        }
    }
    None
}

/// The pixel buffer, as something that can be drawn on.
///
/// `font::Canvas::put_pixel` takes `&self`, so the buffer is reached through a
/// raw pointer — snake does the same, for the same reason.
struct Canvas {
    ptr: *mut Pixel,
    width: usize,
    height: usize,
    /// Where the panel's origin sits inside the surface.
    ox: i32,
    oy: i32,
}

impl Canvas {
    fn new(pixels: &mut [Pixel], width: usize, height: usize, ox: i32, oy: i32) -> Canvas {
        Canvas { ptr: pixels.as_mut_ptr(), width, height, ox, oy }
    }

    fn set(&self, x: i32, y: i32, color: Pixel) {
        if x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height {
            unsafe { *self.ptr.add(y as usize * self.width + x as usize) = color };
        }
    }

    fn fill(&self, x: i32, y: i32, w: i32, h: i32, color: Pixel) {
        for row in 0..h {
            for col in 0..w {
                self.set(self.ox + x + col, self.oy + y + row, color);
            }
        }
    }

    /// A one-pixel outline, which is how a pressed key says so.
    fn outline(&self, x: i32, y: i32, w: i32, h: i32, color: Pixel) {
        self.fill(x, y, w, 1, color);
        self.fill(x, y + h - 1, w, 1, color);
        self.fill(x, y, 1, h, color);
        self.fill(x + w - 1, y, 1, h, color);
    }

    fn text(&self, f: &Font, x: i32, y: i32, s: &str, fg: font::Color, bg: Pixel) {
        let px = self.ox + x;
        let py = self.oy + y;
        if px < 0 || py < 0 {
            return;
        }
        f.draw_string(self, px as usize, py as usize, s, fg, as_font_color(bg));
    }

    fn text_centred(&self, f: &Font, rect: (i32, i32, i32, i32), s: &str, fg: font::Color, bg: Pixel) {
        let chars = s.chars().count() as i32;
        let x = rect.0 + (rect.2 - chars * f.width() as i32) / 2;
        let y = rect.1 + (rect.3 - f.height() as i32) / 2;
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

    /// The one every button and label is drawn in.
    fn ui(&self) -> &Font {
        &self.scaled[2]
    }

    fn smallest(&self) -> &Font {
        &self.scaled[3]
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
            .with_surface_size(PhysicalSize::new(PANEL_W as u32, PANEL_H as u32))
            .with_resizable(false);
        let window: Arc<dyn Window> = elwt.create_window(attrs).unwrap().into();
        let size = window.surface_size();
        let mut surface = Surface::new(context, window.clone()).unwrap();
        let (w, h) = (size.width.max(1), size.height.max(1));
        surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .unwrap();
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
        self.surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .unwrap();
    }

    /// The panel's origin inside the surface, so a surface that is not the size
    /// asked for shows a centred panel rather than a stretched one.
    fn origin(&self) -> (i32, i32) {
        (
            (self.width as i32 - PANEL_W).max(0) / 2,
            (self.height as i32 - PANEL_H).max(0) / 2,
        )
    }

    fn point(&self, x: f64, y: f64) -> (i32, i32) {
        let (ox, oy) = self.origin();
        (x as i32 - ox, y as i32 - oy)
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
        let (ox, oy) = self.origin();
        let (w, h) = (self.width as usize, self.height as usize);
        // The scene borrows the fields the drawing reads; the buffer borrows
        // the surface. Two disjoint halves of one `Ui`.
        let scene = Scene {
            calc: &self.calc,
            fonts: &self.fonts,
            hover: self.hover,
            pressed: self.pressed,
        };
        let mut buffer = self.surface.next_buffer().unwrap();
        let pixels = buffer.pixels();
        let canvas = Canvas::new(pixels, w, h, ox, oy);
        canvas.fill(-ox, -oy, w as i32, h as i32, BG);

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
    hover: Option<Target>,
    pressed: Option<Target>,
}

impl Scene<'_> {
    fn draw_tabs(&self, canvas: &Canvas) {
        for (i, mode) in [Mode::Calc, Mode::Prog].into_iter().enumerate() {
            let rect = tab_rect(i as i32);
            let active = self.calc.mode() == mode;
            let mut base = if active { KEY_EQUALS } else { KEY_FN };
            if self.hover == Some(Target::Tab(mode)) {
                base = shade(base, HOVER, true);
            }
            if self.pressed == Some(Target::Tab(mode)) {
                base = shade(base, PRESSED, false);
            }
            canvas.fill(rect.0, rect.1, rect.2, rect.3, base);
            if active {
                canvas.outline(rect.0, rect.1, rect.2, rect.3, KEY_ACTIVE);
            }
            let label = match mode {
                Mode::Calc => "Calc",
                Mode::Prog => "Prog",
            };
            canvas.text_centred(self.fonts.ui(), rect, label, if active { TEXT } else { DIM }, base);
        }

        // What the layout is standing on, right-aligned in the same strip.
        let note = match self.calc.mode() {
            Mode::Calc => self.calc.angle_label(),
            Mode::Prog => self.calc.base().label(),
        };
        let f = self.fonts.ui();
        let x = PANEL_W - MARGIN - note.chars().count() as i32 * f.width() as i32;
        canvas.text(f, x, TAB_Y + (TAB_H - f.height() as i32) / 2, note, DIM, BG);
    }

    fn draw_display(&self, canvas: &Canvas) {
        canvas.fill(MARGIN, DISPLAY_Y, PANEL_W - 2 * MARGIN, DISPLAY_H, SUNKEN);
        let x0 = MARGIN + DISPLAY_PAD;
        let inner = PANEL_W - 2 * MARGIN - 2 * DISPLAY_PAD;
        match self.calc.mode() {
            Mode::Calc => self.draw_calc_display(canvas, x0, inner),
            Mode::Prog => self.draw_prog_display(canvas, x0, inner),
        }
    }

    fn draw_calc_display(&self, canvas: &Canvas, x0: i32, inner: i32) {
        // The expression, on one line, scrolled so the caret is always on it.
        let expr = self.calc.expr();
        let (f, _) = self.fonts.fit(expr, inner, 1);
        let per = (inner / f.width() as i32).max(1) as usize;
        let caret_at = expr[..self.calc.caret()].chars().count();
        let scroll = caret_at.saturating_sub(per.saturating_sub(1));
        let shown: String = expr.chars().skip(scroll).take(per).collect();
        let y = DISPLAY_Y + 12;
        canvas.text(f, x0, y, &shown, TEXT, SUNKEN);
        let caret_x = x0 + (caret_at - scroll) as i32 * f.width() as i32;
        canvas.fill(caret_x, y - 2, 2, f.height() as i32 + 4, as_pixel(TEXT));

        // The result, as large as it fits and wrapped rather than cut.
        match self.calc.preview() {
            None => {}
            Some(text) => {
                let (rf, lines) = self.fonts.fit(&text, inner, 2);
                let colour = if text.starts_with(APPROX) { DIM } else { TEXT };
                let top = DISPLAY_Y + DISPLAY_H - DISPLAY_PAD - lines.len() as i32 * rf.height() as i32;
                for (i, line) in lines.iter().enumerate() {
                    let width = line.chars().count() as i32 * rf.width() as i32;
                    canvas.text(
                        rf,
                        x0 + inner - width,
                        top + i as i32 * rf.height() as i32,
                        line,
                        colour,
                        SUNKEN,
                    );
                }
            }
        }
    }

    fn draw_prog_display(&self, canvas: &Canvas, x0: i32, inner: i32) {
        let f = self.fonts.ui();
        let cell = f.height() as i32;
        let expr = self.calc.expr();
        let per = (inner / f.width() as i32).max(1) as usize;
        let caret_at = expr[..self.calc.caret()].chars().count();
        let scroll = caret_at.saturating_sub(per.saturating_sub(1));
        let shown: String = expr.chars().skip(scroll).take(per).collect();
        let y = DISPLAY_Y + 8;
        canvas.text(f, x0, y, &shown, TEXT, SUNKEN);
        let caret_x = x0 + (caret_at - scroll) as i32 * f.width() as i32;
        canvas.fill(caret_x, y - 2, 2, cell + 4, as_pixel(TEXT));

        // Three panes over the same sixty-four bits.
        let value = self.calc.value();
        let (high, low) = prog::pane_bin(value);
        let rows: [(&str, &str); 4] = [
            ("HEX", &prog::pane_hex(value)),
            ("DEC", &prog::pane_dec(value)),
            ("BIN", &high),
            ("", &low),
        ];
        let label_w = 4 * f.width() as i32;
        for (i, (label, text)) in rows.iter().enumerate() {
            let ry = DISPLAY_Y + 34 + i as i32 * (cell + 6);
            let active = self.calc.base().label() == *label;
            canvas.text(f, x0, ry, label, if active { TEXT } else { DIM }, SUNKEN);
            let width = text.chars().count() as i32 * f.width() as i32;
            canvas.text(f, x0 + label_w + (inner - label_w - width).max(0), ry, text, TEXT, SUNKEN);
        }
    }

    fn draw_message(&self, canvas: &Canvas) {
        let f = self.fonts.ui();
        let y = MSG_Y + (MSG_H - f.height() as i32) / 2;
        if let Some(message) = self.calc.message() {
            canvas.text(f, MARGIN, y, message, ERROR, BG);
        }
    }

    fn draw_keys(&self, canvas: &Canvas) {
        canvas.fill(
            MARGIN - 6,
            GRID_Y - 6,
            PANEL_W - 2 * MARGIN + 12,
            PANEL_H - GRID_Y - MARGIN + 12,
            PANEL,
        );
        let f = self.fonts.ui();
        for (i, button) in self.calc.buttons().iter().enumerate() {
            let rect = button_rect(i);
            let live = enabled(button, self.calc.mode(), self.calc.base());
            let on = is_on(button, &self.calc);
            let mut colour = if on { KEY_EQUALS } else { key_colour(button) };
            if !live {
                colour = KEY_FN;
            } else if self.hover == Some(Target::Key(i)) {
                colour = shade(colour, HOVER, true);
            }
            if self.pressed == Some(Target::Key(i)) && live {
                colour = shade(colour, PRESSED, false);
            }
            canvas.fill(rect.0, rect.1, rect.2, rect.3, colour);
            if self.pressed == Some(Target::Key(i)) && live {
                canvas.outline(rect.0, rect.1, rect.2, rect.3, KEY_ACTIVE);
            }
            let label = match button.action {
                Action::ToggleAngle => self.calc.angle_label(),
                _ => button.label,
            };
            let fg = if live { TEXT } else { OFF };
            canvas.text_centred(f, rect, label, fg, colour);
        }
    }
}

fn as_pixel(c: font::Color) -> Pixel {
    Pixel::new_rgb(c.r, c.g, c.b)
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
        Action::Insert(text) => {
            let digit = text.len() == 1
                && text.chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '.');
            if digit {
                KEY_DIGIT
            } else if text.ends_with('(') {
                KEY_FN
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
                let (x, y) = ui.point(position.x, position.y);
                let over = hit(&ui.calc, x, y);
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
                let (x, y) = ui.point(position.x, position.y);
                let over = hit(&ui.calc, x, y);
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
        // And the set is not idle: every character in it is on a face, so a
        // codepoint that stops being drawn stops being baked.
        for &ch in DRAWABLE_NON_ASCII {
            assert!(
                faces.iter().any(|f| f.contains(ch)),
                "{ch:?} is baked and nothing draws it"
            );
        }
    }

    /// The thirty-two keys tile the two blocks without overlapping and without
    /// leaving the panel.
    #[test]
    fn the_keys_tile_the_panel() {
        let rects: Vec<(i32, i32, i32, i32)> = (0..32).map(button_rect).collect();
        for (i, a) in rects.iter().enumerate() {
            assert!(a.0 >= MARGIN, "key {i} starts left of the margin");
            assert!(a.0 + a.2 <= PANEL_W - MARGIN, "key {i} runs past the right margin");
            assert!(a.1 + a.3 <= PANEL_H - MARGIN, "key {i} runs past the bottom");
            assert!(a.1 >= GRID_Y);
            for (j, b) in rects.iter().enumerate().skip(i + 1) {
                let apart = a.0 + a.2 <= b.0 || b.0 + b.2 <= a.0 || a.1 + a.3 <= b.1 || b.1 + b.3 <= a.1;
                assert!(apart, "keys {i} and {j} overlap");
            }
        }
        // The right block is flush with the right margin.
        let last = button_rect(7);
        assert_eq!(last.0 + last.2, PANEL_W - MARGIN);
    }

    #[test]
    fn hit_testing_answers_where_the_keys_are() {
        let calc = Calc::new();
        for i in 0..32 {
            let (x, y, w, h) = button_rect(i);
            assert_eq!(hit(&calc, x + w / 2, y + h / 2), Some(Target::Key(i)));
            // The gap between two keys belongs to neither.
            assert_eq!(hit(&calc, x - 3, y + h / 2), None, "the gap left of key {i}");
        }
        assert_eq!(hit(&calc, MARGIN + 2, TAB_Y + 2), Some(Target::Tab(Mode::Calc)));
        assert_eq!(hit(&calc, MARGIN + TAB_W + TAB_GAP + 2, TAB_Y + 2), Some(Target::Tab(Mode::Prog)));
        assert_eq!(hit(&calc, PANEL_W - 2, TAB_Y + 2), None);
        assert_eq!(hit(&calc, 0, DISPLAY_Y + 2), None);
    }

    /// A forty-digit answer is drawn whole at some cell size, which is the
    /// whole point of carrying four of them.
    #[test]
    fn the_longest_answer_fits_on_one_line() {
        let fonts = Fonts::load();
        let inner = PANEL_W - 2 * MARGIN - 2 * DISPLAY_PAD;
        let longest = format!("{APPROX}-1.{}e-100", "9".repeat(39));
        let (_, lines) = fonts.fit(&longest, inner, 2);
        assert_eq!(lines.len(), 1, "an answer wrapped that had no need to");
        assert_eq!(lines[0], longest);
        // And one that genuinely cannot fit is wrapped rather than shortened.
        let absurd = "8".repeat(400);
        let (_, lines) = fonts.fit(&absurd, inner, 2);
        assert_eq!(lines.concat(), absurd);
    }
}

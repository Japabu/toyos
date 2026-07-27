use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::Rng;
use softbuffer::{Context, Pixel, Surface};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowAttributes, WindowId};

const CELL: usize = 16;
const HEADER: usize = 24;
const TICK: Duration = Duration::from_millis(75);

const BG: Pixel = Pixel::new_rgb(0x1a, 0x1a, 0x2e);
const GRID_BG: Pixel = Pixel::new_rgb(0x22, 0x22, 0x38);
const GRID_LINE: Pixel = Pixel::new_rgb(0x28, 0x28, 0x40);
const SNAKE_HEAD: Pixel = Pixel::new_rgb(0x50, 0xe0, 0x50);
const SNAKE_BODY: Pixel = Pixel::new_rgb(0x40, 0xb0, 0x40);
const FOOD: Pixel = Pixel::new_rgb(0xe0, 0x40, 0x40);
const TEXT: font::Color = font::Color { r: 0xe0, g: 0xe0, b: 0xe8 };
const DIM: font::Color = font::Color { r: 0x70, g: 0x70, b: 0x80 };

fn pixel_to_font_color(p: Pixel) -> font::Color {
    font::Color { r: p.r, g: p.g, b: p.b }
}

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    fn opposite(self) -> Self {
        match self {
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
            Dir::Left => Dir::Right,
            Dir::Right => Dir::Left,
        }
    }

    fn delta(self) -> (isize, isize) {
        match self {
            Dir::Up => (0, -1),
            Dir::Down => (0, 1),
            Dir::Left => (-1, 0),
            Dir::Right => (1, 0),
        }
    }
}

/// Adapter so `font::Font::draw_string` can write into a softbuffer pixel slice.
///
/// Uses a raw pointer because `Canvas::put_pixel` takes `&self` but we need mutation.
struct PixelCanvas {
    ptr: *mut Pixel,
    width: usize,
    height: usize,
}

impl PixelCanvas {
    fn new(pixels: &mut [Pixel], width: usize, height: usize) -> Self {
        Self { ptr: pixels.as_mut_ptr(), width, height }
    }
}

impl font::Canvas for PixelCanvas {
    fn put_pixel(&self, x: usize, y: usize, color: font::Color) {
        if x < self.width && y < self.height {
            unsafe { *self.ptr.add(y * self.width + x) = Pixel::new_rgb(color.r, color.g, color.b) };
        }
    }
}

struct Game {
    window: Arc<dyn Window>,
    surface: Surface<OwnedDisplayHandle, Arc<dyn Window>>,
    font: font::Font,
    width: u32,
    height: u32,
    cols: usize,
    rows: usize,
    snake: VecDeque<(usize, usize)>,
    dir: Dir,
    next_dir: Dir,
    food: (usize, usize),
    score: usize,
    game_over: bool,
    next_tick: Instant,
}

impl Game {
    fn new(elwt: &dyn ActiveEventLoop, context: &Context<OwnedDisplayHandle>) -> Self {
        let attrs = WindowAttributes::default().with_title("Snake");
        let window: Arc<dyn Window> = elwt.create_window(attrs).unwrap().into();
        let size = window.surface_size();
        let mut surface = Surface::new(context, window.clone()).unwrap();
        surface
            .resize(
                NonZeroU32::new(size.width).unwrap(),
                NonZeroU32::new(size.height).unwrap(),
            )
            .unwrap();

        let font = font::Font::from_prebuilt(include_bytes!(concat!(env!("OUT_DIR"), "/JetBrainsMono-Regular-8x16.font")));

        let cols = size.width as usize / CELL;
        let rows = (size.height as usize).saturating_sub(HEADER) / CELL;

        let mut game = Game {
            window,
            surface,
            font,
            width: size.width,
            height: size.height,
            cols,
            rows,
            snake: VecDeque::new(),
            dir: Dir::Right,
            next_dir: Dir::Right,
            food: (0, 0),
            score: 0,
            game_over: false,
            next_tick: Instant::now(),
        };
        game.reset();
        game
    }

    fn reset(&mut self) {
        self.snake.clear();
        self.dir = Dir::Right;
        self.next_dir = Dir::Right;
        self.score = 0;
        self.game_over = false;
        self.next_tick = Instant::now() + TICK;

        let cx = self.cols / 2;
        let cy = self.rows / 2;
        self.snake.push_back((cx, cy));
        self.snake.push_back((cx - 1, cy));
        self.snake.push_back((cx - 2, cy));

        self.place_food();
    }

    fn place_food(&mut self) {
        let mut rng = rand::thread_rng();
        loop {
            let x = rng.gen_range(0..self.cols);
            let y = rng.gen_range(0..self.rows);
            if !self.snake.contains(&(x, y)) {
                self.food = (x, y);
                return;
            }
        }
    }

    fn step(&mut self) {
        self.dir = self.next_dir;

        let (hx, hy) = self.snake[0];
        let (dx, dy) = self.dir.delta();
        let nx = hx as isize + dx;
        let ny = hy as isize + dy;

        if nx < 0 || ny < 0 || nx >= self.cols as isize || ny >= self.rows as isize {
            self.game_over = true;
            return;
        }

        let new_head = (nx as usize, ny as usize);

        if self.snake.contains(&new_head) {
            self.game_over = true;
            return;
        }

        self.snake.push_front(new_head);

        if new_head == self.food {
            self.score += 1;
            self.place_food();
        } else {
            self.snake.pop_back();
        }
    }

    fn redraw(&mut self) {
        let mut buffer = self.surface.next_buffer().unwrap();
        let w = self.width as usize;
        let h = self.height as usize;
        let pixels = buffer.pixels();

        fill_rect(pixels, w, h, 0, 0, w, h, BG);

        // Header
        {
            let score_str = format!("Score: {}", self.score);
            let canvas = PixelCanvas::new(pixels, w, h);
            let bg_color = pixel_to_font_color(BG);
            self.font.draw_string(&canvas, 8, 4, &score_str, TEXT, bg_color);
        }

        // Grid
        let grid_y = HEADER;
        let grid_w = self.cols * CELL;
        let grid_h = self.rows * CELL;
        fill_rect(pixels, w, h, 0, grid_y, grid_w, grid_h, GRID_BG);

        // Grid lines
        for col in 0..=self.cols {
            let x = col * CELL;
            if x < w {
                fill_rect(pixels, w, h, x, grid_y, 1, grid_h, GRID_LINE);
            }
        }
        for row in 0..=self.rows {
            let y = grid_y + row * CELL;
            if y < h {
                fill_rect(pixels, w, h, 0, y, grid_w, 1, GRID_LINE);
            }
        }

        // Food
        let (fx, fy) = self.food;
        fill_rect(
            pixels, w, h,
            fx * CELL + 2, grid_y + fy * CELL + 2,
            CELL - 4, CELL - 4, FOOD,
        );

        // Snake
        for (i, &(sx, sy)) in self.snake.iter().enumerate() {
            let color = if i == 0 { SNAKE_HEAD } else { SNAKE_BODY };
            let inset = if i == 0 { 1 } else { 2 };
            fill_rect(
                pixels, w, h,
                sx * CELL + inset, grid_y + sy * CELL + inset,
                CELL - inset * 2, CELL - inset * 2, color,
            );
        }

        // Game over overlay
        if self.game_over {
            let overlay_w = 200;
            let overlay_h = 60;
            let ox = grid_w.saturating_sub(overlay_w) / 2;
            let oy = grid_y + grid_h.saturating_sub(overlay_h) / 2;
            fill_rect(pixels, w, h, ox, oy, overlay_w, overlay_h, BG);

            let bg_color = pixel_to_font_color(BG);
            let food_color = pixel_to_font_color(FOOD);
            let canvas = PixelCanvas::new(pixels, w, h);

            let msg = "GAME OVER";
            let msg_x = ox + overlay_w.saturating_sub(msg.len() * self.font.width()) / 2;
            self.font.draw_string(&canvas, msg_x, oy + 8, msg, food_color, bg_color);

            let score_msg = format!("Score: {}", self.score);
            let sx = ox + overlay_w.saturating_sub(score_msg.len() * self.font.width()) / 2;
            self.font.draw_string(&canvas, sx, oy + 24, &score_msg, TEXT, bg_color);

            let restart = "Space to restart";
            let rx = ox + overlay_w.saturating_sub(restart.len() * self.font.width()) / 2;
            self.font.draw_string(&canvas, rx, oy + 40, restart, DIM, bg_color);
        }

        buffer.present().unwrap();
    }

    fn handle_resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        if let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) {
            self.surface.resize(w, h).unwrap();
        }
        self.cols = width as usize / CELL;
        self.rows = (height as usize).saturating_sub(HEADER) / CELL;
        let snake_oob = self.snake.iter().any(|&(x, y)| x >= self.cols || y >= self.rows);
        if snake_oob {
            self.reset();
        } else {
            let (fx, fy) = self.food;
            if fx >= self.cols || fy >= self.rows {
                self.place_food();
            }
        }
    }

    fn handle_key(&mut self, key: KeyCode, state: ElementState) {
        if state != ElementState::Pressed {
            return;
        }

        if self.game_over {
            if matches!(key, KeyCode::Space | KeyCode::Enter) {
                self.reset();
            }
            return;
        }

        let new_dir = match key {
            KeyCode::ArrowUp => Some(Dir::Up),
            KeyCode::ArrowDown => Some(Dir::Down),
            KeyCode::ArrowLeft => Some(Dir::Left),
            KeyCode::ArrowRight => Some(Dir::Right),
            _ => None,
        };

        if let Some(d) = new_dir {
            if d != self.dir.opposite() {
                self.next_dir = d;
            }
        }
    }
}

fn fill_rect(pixels: &mut [Pixel], buf_w: usize, buf_h: usize, x: usize, y: usize, w: usize, h: usize, color: Pixel) {
    let x_end = (x + w).min(buf_w);
    let y_end = (y + h).min(buf_h);
    for row in y..y_end {
        let start = row * buf_w + x;
        let end = row * buf_w + x_end;
        pixels[start..end].fill(color);
    }
}

struct App {
    context: Context<OwnedDisplayHandle>,
    game: Option<Game>,
}

impl ApplicationHandler for App {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        if self.game.is_none() {
            self.game = Some(Game::new(event_loop, &self.context));
        }
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        let game = self.game.as_mut().unwrap();
        if game.game_over {
            event_loop.set_control_flow(ControlFlow::Wait);
        } else {
            let now = Instant::now();
            if now >= game.next_tick {
                game.step();
                game.next_tick = now + TICK;
                game.window.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(game.next_tick));
        }
    }

    fn window_event(&mut self, event_loop: &dyn ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let game = self.game.as_mut().unwrap();
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::SurfaceResized(size) => {
                game.handle_resize(size.width, size.height);
                game.window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(key) = event.physical_key {
                    game.handle_key(key, event.state);
                    if game.game_over || matches!(key, KeyCode::ArrowUp | KeyCode::ArrowDown | KeyCode::ArrowLeft | KeyCode::ArrowRight) {
                        game.window.request_redraw();
                    }
                }
            }
            WindowEvent::RedrawRequested => game.redraw(),
            _ => {}
        }
    }
}

fn main() {
    let event_loop = EventLoop::new().unwrap();
    let context = Context::new(event_loop.owned_display_handle()).unwrap();
    let app = App { context, game: None };
    event_loop.run_app(app).unwrap();
}

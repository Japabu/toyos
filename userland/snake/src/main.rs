use std::collections::VecDeque;
use std::fs;
use std::os::toyos::io;

use window::{Color, Framebuffer, Window};

const CELL: usize = 16;
const HEADER: usize = 24;
const TICK_MS: u64 = 150;
const TICK_NANOS: u64 = TICK_MS * 1_000_000;

const BG: Color = Color { r: 0x1a, g: 0x1a, b: 0x2e };
const GRID_BG: Color = Color { r: 0x22, g: 0x22, b: 0x38 };
const GRID_LINE: Color = Color { r: 0x28, g: 0x28, b: 0x40 };
const SNAKE_HEAD: Color = Color { r: 0x50, g: 0xe0, b: 0x50 };
const SNAKE_BODY: Color = Color { r: 0x40, g: 0xb0, b: 0x40 };
const FOOD: Color = Color { r: 0xe0, g: 0x40, b: 0x40 };
const TEXT: Color = Color { r: 0xe0, g: 0xe0, b: 0xe8 };
const DIM: Color = Color { r: 0x70, g: 0x70, b: 0x80 };

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

struct Rng(u64);

impl Rng {
    fn new() -> Self {
        let mut seed = io::clock_nanos();
        if seed == 0 {
            seed = 0xdeadbeef;
        }
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn range(&mut self, max: usize) -> usize {
        (self.next() % max as u64) as usize
    }
}

struct Game {
    window: Window,
    fb: Framebuffer,
    font: font::Font,
    rng: Rng,
    cols: usize,
    rows: usize,
    snake: VecDeque<(usize, usize)>,
    dir: Dir,
    next_dir: Dir,
    food: (usize, usize),
    score: usize,
    game_over: bool,
    next_tick: u64,
}

impl Game {
    fn new() -> Self {
        let window = Window::create_with_title(0, 0, "Snake");
        let fb = window.framebuffer();
        let font_data = fs::read("/initrd/JetBrainsMono-8x16.font").expect("font");
        let font = font::Font::from_prebuilt(&font_data);
        let rng = Rng::new();

        let cols = fb.width() / CELL;
        let rows = (fb.height() - HEADER) / CELL;

        let mut game = Game {
            window,
            fb,
            font,
            rng,
            cols,
            rows,
            snake: VecDeque::new(),
            dir: Dir::Right,
            next_dir: Dir::Right,
            food: (0, 0),
            score: 0,
            game_over: false,
            next_tick: 0,
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
        self.next_tick = io::clock_nanos() + TICK_NANOS;

        let cx = self.cols / 2;
        let cy = self.rows / 2;
        self.snake.push_back((cx, cy));
        self.snake.push_back((cx - 1, cy));
        self.snake.push_back((cx - 2, cy));

        self.place_food();
        self.redraw();
        self.window.present();
    }

    fn place_food(&mut self) {
        loop {
            let x = self.rng.range(self.cols);
            let y = self.rng.range(self.rows);
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

        // Wall collision
        if nx < 0 || ny < 0 || nx >= self.cols as isize || ny >= self.rows as isize {
            self.game_over = true;
            return;
        }

        let new_head = (nx as usize, ny as usize);

        // Self collision
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

    fn redraw(&self) {
        let w = self.fb.width();
        let h = self.fb.height();

        // Header
        self.fb.fill_rect(0, 0, w, HEADER, BG);
        let score_str = format!("Score: {}", self.score);
        self.font.draw_string(&self.fb, 8, 4, &score_str, TEXT, BG);

        // Grid background
        let grid_y = HEADER;
        let grid_w = self.cols * CELL;
        let grid_h = self.rows * CELL;
        self.fb.fill_rect(0, grid_y, w, h - grid_y, BG);
        self.fb.fill_rect(0, grid_y, grid_w, grid_h, GRID_BG);

        // Grid lines
        for col in 0..=self.cols {
            let x = col * CELL;
            if x < w {
                self.fb.fill_rect(x, grid_y, 1, grid_h, GRID_LINE);
            }
        }
        for row in 0..=self.rows {
            let y = grid_y + row * CELL;
            if y < h {
                self.fb.fill_rect(0, y, grid_w, 1, GRID_LINE);
            }
        }

        // Food
        let (fx, fy) = self.food;
        self.fb.fill_rect(fx * CELL + 2, grid_y + fy * CELL + 2, CELL - 4, CELL - 4, FOOD);

        // Snake
        for (i, &(sx, sy)) in self.snake.iter().enumerate() {
            let color = if i == 0 { SNAKE_HEAD } else { SNAKE_BODY };
            let inset = if i == 0 { 1 } else { 2 };
            self.fb.fill_rect(
                sx * CELL + inset,
                grid_y + sy * CELL + inset,
                CELL - inset * 2,
                CELL - inset * 2,
                color,
            );
        }

        // Game over overlay
        if self.game_over {
            let overlay_w = 200;
            let overlay_h = 60;
            let ox = (grid_w.saturating_sub(overlay_w)) / 2;
            let oy = grid_y + (grid_h.saturating_sub(overlay_h)) / 2;
            self.fb.fill_rect(ox, oy, overlay_w, overlay_h, BG);

            let msg = "GAME OVER";
            let msg_x = ox + (overlay_w.saturating_sub(msg.len() * self.font.width())) / 2;
            self.font.draw_string(&self.fb, msg_x, oy + 8, msg, FOOD, BG);

            let score_msg = format!("Score: {}", self.score);
            let sx = ox + (overlay_w.saturating_sub(score_msg.len() * self.font.width())) / 2;
            self.font.draw_string(&self.fb, sx, oy + 24, &score_msg, TEXT, BG);

            let restart = "Space to restart";
            let rx = ox + (overlay_w.saturating_sub(restart.len() * self.font.width())) / 2;
            self.font.draw_string(&self.fb, rx, oy + 40, restart, DIM, BG);
        }
    }

    fn handle_resize(&mut self) {
        self.fb = self.window.framebuffer();
        self.cols = self.fb.width() / CELL;
        self.rows = (self.fb.height() - HEADER) / CELL;
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

    fn handle_key(&mut self, ev: window::KeyEvent) {
        if self.game_over {
            if ev.keycode == 0x2C || ev.keycode == 0x28 {
                self.reset();
            }
            return;
        }

        let new_dir = match ev.keycode {
            0x52 => Some(Dir::Up),
            0x51 => Some(Dir::Down),
            0x50 => Some(Dir::Left),
            0x4F => Some(Dir::Right),
            _ => None,
        };

        if let Some(d) = new_dir {
            if d != self.next_dir.opposite() {
                self.next_dir = d;
            }
        }
    }

    fn run(&mut self) {
        loop {
            let now = io::clock_nanos();
            // timeout=0 means block forever, so use max(1) when game is running
            let timeout = if self.game_over {
                0
            } else {
                self.next_tick.saturating_sub(now).max(1)
            };

            match self.window.poll_event(timeout) {
                Some(window::Event::KeyInput(ev)) => self.handle_key(ev),
                Some(window::Event::Resized) => self.handle_resize(),
                Some(window::Event::Close) => break,
                _ => {}
            }

            if !self.game_over && io::clock_nanos() >= self.next_tick {
                self.step();
                self.next_tick = io::clock_nanos() + TICK_NANOS;
            }

            self.redraw();
            self.window.present();
        }
    }
}

fn main() {
    let mut game = Game::new();
    game.run();
}

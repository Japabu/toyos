use alloc::vec::Vec;
use core::cell::UnsafeCell;

use crate::font::{self, Font};
use crate::drivers::framebuffer::{Color, Framebuffer};

const DEFAULT_FG: Color = Color { r: 255, g: 255, b: 255 };
const DEFAULT_BG: Color = Color { r: 0, g: 0, b: 0 };

fn ansi_color(index: usize) -> Color {
    match index {
        0 => Color { r: 0, g: 0, b: 0 },
        1 => Color { r: 205, g: 49, b: 49 },
        2 => Color { r: 13, g: 188, b: 121 },
        3 => Color { r: 229, g: 229, b: 16 },
        4 => Color { r: 36, g: 114, b: 200 },
        5 => Color { r: 188, g: 63, b: 188 },
        6 => Color { r: 17, g: 168, b: 205 },
        7 => Color { r: 229, g: 229, b: 229 },
        _ => DEFAULT_FG,
    }
}

fn ansi_bright_color(index: usize) -> Color {
    match index {
        0 => Color { r: 102, g: 102, b: 102 },
        1 => Color { r: 241, g: 76, b: 76 },
        2 => Color { r: 35, g: 209, b: 139 },
        3 => Color { r: 245, g: 245, b: 67 },
        4 => Color { r: 59, g: 142, b: 234 },
        5 => Color { r: 214, g: 112, b: 214 },
        6 => Color { r: 41, g: 184, b: 219 },
        7 => Color { r: 255, g: 255, b: 255 },
        _ => DEFAULT_FG,
    }
}

fn color256(n: usize) -> Color {
    match n {
        0..=7 => ansi_color(n),
        8..=15 => ansi_bright_color(n - 8),
        16..=231 => {
            let n = n - 16;
            Color {
                r: ((n / 36) * 51) as u8,
                g: (((n / 6) % 6) * 51) as u8,
                b: ((n % 6) * 51) as u8,
            }
        }
        232..=255 => {
            let v = (8 + (n - 232) * 10) as u8;
            Color { r: v, g: v, b: v }
        }
        _ => DEFAULT_FG,
    }
}

// ANSI escape sequence parser states
#[derive(Clone, Copy)]
enum AnsiState {
    Normal,
    Escape,       // saw \x1b
    Bracket,      // saw \x1b[
    QuestionMark, // saw \x1b[?
}

struct Console {
    fb: Framebuffer,
    font: Font,
    cols: usize,
    rows: usize,
    cursor_col: usize,
    cursor_row: usize,
    fg: Color,
    bg: Color,
    // ANSI state machine
    ansi_state: AnsiState,
    ansi_buf: [u8; 16],
    ansi_len: usize,
    reverse_video: bool,
}

impl Console {
    fn new(fb: Framebuffer, font_data: Vec<u8>) -> Self {
        let cols = fb.width() / font::WIDTH;
        let rows = fb.height() / font::HEIGHT;

        let console = Self {
            fb,
            font: Font::new(font_data),
            cols,
            rows,
            cursor_col: 0,
            cursor_row: 0,
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            ansi_state: AnsiState::Normal,
            ansi_buf: [0; 16],
            ansi_len: 0,
            reverse_video: false,
        };

        console.fb.clear(console.bg);
        console
    }

    fn draw_char(&self, col: usize, row: usize, ch: u8) {
        let px = col * font::WIDTH;
        let py = row * font::HEIGHT;
        let (fg, bg) = if self.reverse_video {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        };
        self.font.draw_char(&self.fb, px, py, ch, fg, bg);
    }

    fn scroll(&mut self) {
        self.fb.scroll_up(font::HEIGHT, self.bg);
        self.cursor_row = self.rows - 1;
        self.cursor_col = 0;
    }

    fn newline(&mut self) {
        self.cursor_col = 0;
        self.cursor_row += 1;
        if self.cursor_row >= self.rows {
            self.scroll();
        }
    }

    fn write_byte(&mut self, byte: u8) {
        match self.ansi_state {
            AnsiState::Normal => match byte {
                0x1B => self.ansi_state = AnsiState::Escape,
                b'\n' => self.newline(),
                b'\r' => self.cursor_col = 0,
                byte => {
                    if self.cursor_col >= self.cols {
                        self.newline();
                    }
                    self.draw_char(self.cursor_col, self.cursor_row, byte);
                    self.cursor_col += 1;
                }
            },
            AnsiState::Escape => match byte {
                b'[' => {
                    self.ansi_state = AnsiState::Bracket;
                    self.ansi_len = 0;
                }
                _ => self.ansi_state = AnsiState::Normal, // invalid, reset
            },
            AnsiState::Bracket => {
                if byte == b'?' {
                    self.ansi_state = AnsiState::QuestionMark;
                    self.ansi_len = 0;
                } else if byte.is_ascii_digit() || byte == b';' {
                    if self.ansi_len < self.ansi_buf.len() {
                        self.ansi_buf[self.ansi_len] = byte;
                        self.ansi_len += 1;
                    }
                } else {
                    // Terminal byte — execute the command
                    self.execute_ansi(byte);
                    self.ansi_state = AnsiState::Normal;
                }
            }
            AnsiState::QuestionMark => {
                if byte.is_ascii_digit() {
                    if self.ansi_len < self.ansi_buf.len() {
                        self.ansi_buf[self.ansi_len] = byte;
                        self.ansi_len += 1;
                    }
                } else {
                    self.execute_ansi_private(byte);
                    self.ansi_state = AnsiState::Normal;
                }
            }
        }
    }

    fn parse_params(&self) -> ([usize; 8], usize) {
        let buf = &self.ansi_buf[..self.ansi_len];
        let mut params = [0usize; 8];
        let mut count = 0;
        let mut val: usize = 0;
        let mut has_digit = false;
        for &b in buf {
            if b == b';' {
                if count < 8 {
                    params[count] = val;
                    count += 1;
                }
                val = 0;
                has_digit = false;
            } else {
                val = val * 10 + (b - b'0') as usize;
                has_digit = true;
            }
        }
        if has_digit && count < 8 {
            params[count] = val;
            count += 1;
        }
        (params, count)
    }

    fn execute_ansi(&mut self, cmd: u8) {
        let (params, count) = self.parse_params();
        let p1 = if count > 0 { params[0] } else { 0 };
        let p2 = if count > 1 { params[1] } else { 0 };
        match cmd {
            b'H' | b'f' => {
                let row = if p1 == 0 { 0 } else { p1 - 1 };
                let col = if p2 == 0 { 0 } else { p2 - 1 };
                self.cursor_row = row.min(self.rows - 1);
                self.cursor_col = col.min(self.cols - 1);
            }
            b'J' => {
                if p1 == 2 || p1 == 3 {
                    self.fb.clear(self.bg);
                    self.cursor_col = 0;
                    self.cursor_row = 0;
                }
            }
            b'K' => {
                if p1 == 0 {
                    for col in self.cursor_col..self.cols {
                        self.draw_char(col, self.cursor_row, b' ');
                    }
                }
            }
            b'm' => self.execute_sgr(&params[..count]),
            b'A' => {
                let n = if p1 == 0 { 1 } else { p1 };
                self.cursor_row = self.cursor_row.saturating_sub(n);
            }
            b'B' => {
                let n = if p1 == 0 { 1 } else { p1 };
                self.cursor_row = (self.cursor_row + n).min(self.rows - 1);
            }
            b'C' => {
                let n = if p1 == 0 { 1 } else { p1 };
                self.cursor_col = (self.cursor_col + n).min(self.cols - 1);
            }
            b'D' => {
                let n = if p1 == 0 { 1 } else { p1 };
                self.cursor_col = self.cursor_col.saturating_sub(n);
            }
            _ => {}
        }
    }

    fn execute_sgr(&mut self, params: &[usize]) {
        // \x1b[m with no params is equivalent to \x1b[0m
        if params.is_empty() {
            self.fg = DEFAULT_FG;
            self.bg = DEFAULT_BG;
            self.reverse_video = false;
            return;
        }
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => {
                    self.fg = DEFAULT_FG;
                    self.bg = DEFAULT_BG;
                    self.reverse_video = false;
                }
                7 => self.reverse_video = true,
                27 => self.reverse_video = false,
                30..=37 => self.fg = ansi_color(params[i] - 30),
                38 => {
                    // Extended foreground: 38;5;N (256-color)
                    if i + 2 < params.len() && params[i + 1] == 5 {
                        self.fg = color256(params[i + 2]);
                        i += 2;
                    }
                }
                39 => self.fg = DEFAULT_FG,
                40..=47 => self.bg = ansi_color(params[i] - 40),
                48 => {
                    // Extended background: 48;5;N (256-color)
                    if i + 2 < params.len() && params[i + 1] == 5 {
                        self.bg = color256(params[i + 2]);
                        i += 2;
                    }
                }
                49 => self.bg = DEFAULT_BG,
                90..=97 => self.fg = ansi_bright_color(params[i] - 90),
                100..=107 => self.bg = ansi_bright_color(params[i] - 100),
                _ => {}
            }
            i += 1;
        }
    }

    fn execute_ansi_private(&mut self, cmd: u8) {
        let (params, count) = self.parse_params();
        let p1 = if count > 0 { params[0] } else { 0 };
        match (p1, cmd) {
            (25, b'l') => {} // hide cursor (no-op for now)
            (25, b'h') => {} // show cursor (no-op for now)
            (1049, b'h') => { // alternate screen buffer: just clear
                self.fb.clear(self.bg);
                self.cursor_col = 0;
                self.cursor_row = 0;
            }
            (1049, b'l') => {} // restore main screen (no-op)
            _ => {}
        }
    }

    fn write_str(&mut self, s: &str) {
        for byte in s.bytes() {
            self.write_byte(byte);
        }
    }
}

// Global singleton

struct GlobalConsole {
    inner: UnsafeCell<Option<Console>>,
}

unsafe impl Sync for GlobalConsole {}

static CONSOLE: GlobalConsole = GlobalConsole {
    inner: UnsafeCell::new(None),
};

pub fn init(fb: Framebuffer, font_data: Vec<u8>) {
    unsafe {
        *CONSOLE.inner.get() = Some(Console::new(fb, font_data));
    }
}

pub fn println(s: &str) {
    unsafe {
        if let Some(console) = &mut *CONSOLE.inner.get() {
            console.write_str(s);
            console.write_byte(b'\n');
        }
    }
}

pub fn write_str(s: &str) {
    unsafe {
        if let Some(console) = &mut *CONSOLE.inner.get() {
            console.write_str(s);
        }
    }
}

pub fn putchar(b: u8) {
    unsafe {
        if let Some(console) = &mut *CONSOLE.inner.get() {
            console.write_byte(b);
        }
    }
}

pub fn clear() {
    unsafe {
        if let Some(console) = &mut *CONSOLE.inner.get() {
            console.fb.clear(console.bg);
            console.cursor_col = 0;
            console.cursor_row = 0;
        }
    }
}

pub fn screen_size() -> (usize, usize) {
    unsafe {
        if let Some(console) = &*CONSOLE.inner.get() {
            (console.cols, console.rows)
        } else {
            (80, 24)
        }
    }
}

pub fn backspace() {
    unsafe {
        if let Some(console) = &mut *CONSOLE.inner.get() {
            if console.cursor_col > 0 {
                console.cursor_col -= 1;
                console.draw_char(console.cursor_col, console.cursor_row, b' ');
            }
        }
    }
}

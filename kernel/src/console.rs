use alloc::vec::Vec;
use core::cell::UnsafeCell;

use crate::font::{self, Font};
use crate::framebuffer::{Color, Framebuffer};

struct Console {
    fb: Framebuffer,
    font: Font,
    cols: usize,
    rows: usize,
    cursor_col: usize,
    cursor_row: usize,
    fg: Color,
    bg: Color,
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
            fg: Color::WHITE,
            bg: Color::BLACK,
        };

        console.fb.clear(console.bg);
        console
    }

    fn draw_char(&self, col: usize, row: usize, ch: u8) {
        let px = col * font::WIDTH;
        let py = row * font::HEIGHT;
        self.font.draw_char(&self.fb, px, py, ch, self.fg, self.bg);
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
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.cursor_col = 0,
            byte => {
                if self.cursor_col >= self.cols {
                    self.newline();
                }
                self.draw_char(self.cursor_col, self.cursor_row, byte);
                self.cursor_col += 1;
            }
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

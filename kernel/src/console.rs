use core::cell::UnsafeCell;

use crate::framebuffer::{Color, Framebuffer};

const FONT_WIDTH: usize = 8;
const FONT_HEIGHT: usize = 16;
const FONT_GLYPHS: usize = 256;
const FONT_BYTES: usize = FONT_GLYPHS * FONT_HEIGHT;

struct Console {
    fb: Framebuffer,
    font: [u8; FONT_BYTES],
    cols: usize,
    rows: usize,
    cursor_col: usize,
    cursor_row: usize,
    fg: Color,
    bg: Color,
}

impl Console {
    fn new(fb: Framebuffer, font_data: &[u8]) -> Self {
        assert!(font_data.len() >= FONT_BYTES, "Font data too small");
        let mut font = [0u8; FONT_BYTES];
        font.copy_from_slice(&font_data[..FONT_BYTES]);

        let cols = fb.width() / FONT_WIDTH;
        let rows = fb.height() / FONT_HEIGHT;

        let console = Self {
            fb,
            font,
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
        let glyph_offset = (ch as usize) * FONT_HEIGHT;
        let px = col * FONT_WIDTH;
        let py = row * FONT_HEIGHT;

        for glyph_row in 0..FONT_HEIGHT {
            let bitmap_byte = self.font[glyph_offset + glyph_row];
            for bit in 0..FONT_WIDTH {
                let color = if bitmap_byte & (0x80 >> bit) != 0 {
                    self.fg
                } else {
                    self.bg
                };
                self.fb.put_pixel(px + bit, py + glyph_row, color);
            }
        }
    }

    fn scroll(&mut self) {
        self.fb.scroll_up(FONT_HEIGHT, self.bg);
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

pub fn init(fb: Framebuffer, font_data: &[u8]) {
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

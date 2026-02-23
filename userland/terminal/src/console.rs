use crate::font::{self, Font};
use crate::framebuffer::{Color, Framebuffer};

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

#[derive(Clone, Copy)]
enum AnsiState {
    Normal,
    Escape,
    Bracket,
    QuestionMark,
}

struct SavedScreen {
    char_buf: Vec<char>,
    rendered: Vec<u64>,
    cursor_col: usize,
    cursor_row: usize,
}

/// Pack codepoint + fg + bg into a u64 for fast equality checks.
fn cell_key(ch: char, fg: Color, bg: Color) -> u64 {
    (ch as u64 & 0x1F_FFFF)
        | ((fg.r as u64 >> 1) << 21)
        | ((fg.g as u64 >> 1) << 28)
        | ((fg.b as u64 >> 1) << 35)
        | ((bg.r as u64 >> 1) << 42)
        | ((bg.g as u64 >> 1) << 49)
        | ((bg.b as u64 >> 1) << 56)
}

pub struct Console {
    fb: Framebuffer,
    font: Font,
    cols: usize,
    rows: usize,
    cursor_col: usize,
    cursor_row: usize,
    fg: Color,
    bg: Color,
    char_buf: Vec<char>,
    rendered: Vec<u64>,
    ansi_state: AnsiState,
    ansi_buf: [u8; 16],
    ansi_len: usize,
    reverse_video: bool,
    cursor_visible: bool,
    cursor_enabled: bool,
    saved_screen: Option<SavedScreen>,
    utf8_buf: [u8; 4],
    utf8_len: usize,
    utf8_needed: usize,
}

impl Console {
    pub fn new(fb: Framebuffer, font: Font) -> Self {
        let cols = fb.width() / font::WIDTH;
        let rows = fb.height() / font::HEIGHT;

        let mut console = Self {
            fb,
            font,
            cols,
            rows,
            cursor_col: 0,
            cursor_row: 0,
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            char_buf: vec![' '; cols * rows],
            rendered: vec![cell_key(' ', DEFAULT_FG, DEFAULT_BG); cols * rows],
            ansi_state: AnsiState::Normal,
            ansi_buf: [0; 16],
            ansi_len: 0,
            reverse_video: false,
            cursor_visible: false,
            cursor_enabled: true,
            saved_screen: None,
            utf8_buf: [0; 4],
            utf8_len: 0,
            utf8_needed: 0,
        };

        console.fb.clear(DEFAULT_BG);
        console.draw_cursor();
        console
    }

    fn put_char(&mut self, col: usize, row: usize, ch: char) {
        let idx = row * self.cols + col;
        self.char_buf[idx] = ch;
        let (fg, bg) = if self.reverse_video {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        };
        let key = cell_key(ch, fg, bg);
        if self.rendered[idx] == key {
            return;
        }
        self.rendered[idx] = key;
        let px = col * font::WIDTH;
        let py = row * font::HEIGHT;
        self.font.draw_char(&self.fb, px, py, ch, fg, bg);
    }

    fn draw_cursor(&mut self) {
        if !self.cursor_enabled {
            return;
        }
        if self.cursor_col < self.cols && self.cursor_row < self.rows {
            let idx = self.cursor_row * self.cols + self.cursor_col;
            let ch = self.char_buf[idx];
            let px = self.cursor_col * font::WIDTH;
            let py = self.cursor_row * font::HEIGHT;
            self.rendered[idx] = 0;
            self.font.draw_char(&self.fb, px, py, ch, self.bg, self.fg);
        }
        self.cursor_visible = true;
    }

    fn erase_cursor(&mut self) {
        if !self.cursor_visible {
            return;
        }
        if self.cursor_col < self.cols && self.cursor_row < self.rows {
            let idx = self.cursor_row * self.cols + self.cursor_col;
            let ch = self.char_buf[idx];
            let px = self.cursor_col * font::WIDTH;
            let py = self.cursor_row * font::HEIGHT;
            self.rendered[idx] = 0;
            self.font.draw_char(&self.fb, px, py, ch, self.fg, self.bg);
        }
        self.cursor_visible = false;
    }

    fn scroll(&mut self) {
        self.fb.scroll_up(font::HEIGHT, self.bg);
        let row_size = self.cols;
        self.char_buf.copy_within(row_size.., 0);
        self.rendered.copy_within(row_size.., 0);
        let last_row = (self.rows - 1) * row_size;
        for i in last_row..last_row + row_size {
            self.char_buf[i] = ' ';
            self.rendered[i] = 0;
        }
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

    fn clear_screen(&mut self) {
        self.fb.clear(self.bg);
        self.char_buf.fill(' ');
        let blank = cell_key(' ', DEFAULT_FG, DEFAULT_BG);
        self.rendered.fill(blank);
        self.cursor_col = 0;
        self.cursor_row = 0;
    }

    fn redraw_all(&mut self) {
        self.fb.clear(self.bg);
        self.rendered.fill(0);
        for row in 0..self.rows {
            for col in 0..self.cols {
                let idx = row * self.cols + col;
                let ch = self.char_buf[idx];
                if ch != ' ' {
                    let px = col * font::WIDTH;
                    let py = row * font::HEIGHT;
                    self.rendered[idx] = cell_key(ch, self.fg, self.bg);
                    self.font.draw_char(&self.fb, px, py, ch, self.fg, self.bg);
                }
            }
        }
    }

    fn emit_char(&mut self, ch: char) {
        if self.cursor_col >= self.cols {
            self.newline();
        }
        self.put_char(self.cursor_col, self.cursor_row, ch);
        self.cursor_col += 1;
    }

    fn flush_utf8(&mut self) {
        if let Ok(s) = core::str::from_utf8(&self.utf8_buf[..self.utf8_len]) {
            if let Some(ch) = s.chars().next() {
                self.emit_char(ch);
            }
        }
        self.utf8_needed = 0;
    }

    fn write_byte(&mut self, byte: u8) {
        if self.utf8_needed > 0 {
            if byte & 0xC0 == 0x80 {
                self.utf8_buf[self.utf8_len] = byte;
                self.utf8_len += 1;
                if self.utf8_len == self.utf8_needed {
                    self.flush_utf8();
                }
                return;
            }
            self.utf8_needed = 0;
        }

        match self.ansi_state {
            AnsiState::Normal => match byte {
                0x1B => self.ansi_state = AnsiState::Escape,
                b'\n' => self.newline(),
                b'\r' => self.cursor_col = 0,
                0x08 | 0x7F => {
                    if self.cursor_col > 0 {
                        self.cursor_col -= 1;
                    }
                }
                b if b & 0xE0 == 0xC0 => {
                    self.utf8_buf[0] = b;
                    self.utf8_len = 1;
                    self.utf8_needed = 2;
                }
                b if b & 0xF0 == 0xE0 => {
                    self.utf8_buf[0] = b;
                    self.utf8_len = 1;
                    self.utf8_needed = 3;
                }
                b if b & 0xF8 == 0xF0 => {
                    self.utf8_buf[0] = b;
                    self.utf8_len = 1;
                    self.utf8_needed = 4;
                }
                byte if byte >= 0x20 => self.emit_char(byte as char),
                _ => {}
            },
            AnsiState::Escape => match byte {
                b'[' => {
                    self.ansi_state = AnsiState::Bracket;
                    self.ansi_len = 0;
                }
                _ => self.ansi_state = AnsiState::Normal,
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
                    self.clear_screen();
                }
            }
            b'K' => {
                if p1 == 0 {
                    for col in self.cursor_col..self.cols {
                        self.put_char(col, self.cursor_row, ' ');
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
                    if i + 2 < params.len() && params[i + 1] == 5 {
                        self.fg = color256(params[i + 2]);
                        i += 2;
                    }
                }
                39 => self.fg = DEFAULT_FG,
                40..=47 => self.bg = ansi_color(params[i] - 40),
                48 => {
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
            (25, b'l') => {
                self.cursor_enabled = false;
                self.erase_cursor();
            }
            (25, b'h') => {
                self.cursor_enabled = true;
            }
            (1049, b'h') => {
                let n = self.cols * self.rows;
                self.saved_screen = Some(SavedScreen {
                    char_buf: core::mem::replace(&mut self.char_buf, vec![' '; n]),
                    rendered: core::mem::replace(&mut self.rendered, vec![0; n]),
                    cursor_col: self.cursor_col,
                    cursor_row: self.cursor_row,
                });
                self.cursor_col = 0;
                self.cursor_row = 0;
                self.fb.clear(self.bg);
            }
            (1049, b'l') => {
                if let Some(saved) = self.saved_screen.take() {
                    self.char_buf = saved.char_buf;
                    self.rendered = saved.rendered;
                    self.cursor_col = saved.cursor_col;
                    self.cursor_row = saved.cursor_row;
                    self.redraw_all();
                }
            }
            _ => {}
        }
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.erase_cursor();
        for &byte in bytes {
            self.write_byte(byte);
        }
        self.draw_cursor();
    }
}

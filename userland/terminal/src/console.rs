use std::collections::VecDeque;

use font::Font;
use window::{Color, Framebuffer, Screen};

const DEFAULT_FG: Color = Color { r: 255, g: 255, b: 255 };
const DEFAULT_BG: Color = Color { r: 0, g: 0, b: 0 };
const SEL_FG: Color = Color { r: 255, g: 255, b: 255 };
const SEL_BG: Color = Color { r: 58, g: 110, b: 165 };
/// Public because a caller that pre-fills the scrollback has to bound what it
/// writes, and the honest bound is this number rather than one of its own.
pub const SCROLLBACK_ROWS: usize = 1000;
const SCROLLBAR_WIDTH: usize = 6;
const SCROLLBAR_TRACK: Color = Color { r: 0x20, g: 0x20, b: 0x20 };
const SCROLLBAR_THUMB: Color = Color { r: 0x60, g: 0x60, b: 0x60 };
const SCROLLBAR_THUMB_MIN: usize = 20;

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
    fg_buf: Vec<Color>,
    bg_buf: Vec<Color>,
    wrapped: Vec<bool>,
    cursor_col: usize,
    cursor_row: usize,
}

/// A screen cell as the view says it should look.
///
/// Compared for equality against what was last put on the panel at that
/// position, which is the whole of the damage test. It holds the three values
/// a glyph is drawn from rather than a hash of them, so two cells that differ
/// cannot compare equal and leave a stale one on screen.
#[derive(Clone, Copy, PartialEq)]
struct Painted {
    ch: char,
    fg: Color,
    bg: Color,
}

struct ScrollbackRow {
    chars: Vec<char>,
    fg: Vec<Color>,
    bg: Vec<Color>,
}

/// A terminal over a [`Screen`].
///
/// Everything is composed in system RAM and the panel only ever receives
/// finished pixels: the emulator writes cells, and one pass at the end of a
/// batch blits the cells that no longer match what is on the panel. Two things
/// follow from that and are the point of it.
///
/// The panel is never read. Scrolling used to be a `memmove` inside the
/// mapping, which reads back every byte it moves — 16.8 MiB per scrolled line
/// at 2048x2048, measured — and reads are uncached under both of the memory
/// types a GOP scanout is ever mapped with.
///
/// And a batch costs one paint rather than one per line. A thousand lines
/// arriving together scroll the cell grid a thousand times and reach the panel
/// once, so what is blitted is the difference between the screen before and the
/// screen after, not the thousand screens in between.
pub struct Console {
    screen: Screen,
    font: Font,
    cols: usize,
    rows: usize,
    cursor_col: usize,
    cursor_row: usize,
    fg: Color,
    bg: Color,
    char_buf: Vec<char>,
    fg_buf: Vec<Color>,
    bg_buf: Vec<Color>,
    /// Per-row flag: true if this row soft-wrapped into the next row.
    wrapped: Vec<bool>,
    /// What is on the panel, per screen cell. `None` where nothing is known to
    /// be — a fresh console, whose panel still holds whatever drew there last.
    painted: Vec<Option<Painted>>,
    /// The scrollbar thumb as last drawn, `None` when there is no bar on
    /// screen.
    painted_scrollbar: Option<(usize, usize)>,
    ansi_state: AnsiState,
    ansi_buf: [u8; 16],
    ansi_len: usize,
    reverse_video: bool,
    cursor_enabled: bool,
    saved_screen: Option<SavedScreen>,
    utf8_buf: [u8; 4],
    utf8_len: usize,
    utf8_needed: usize,
    sel_anchor: Option<(usize, usize)>,
    sel_end: Option<(usize, usize)>,
    scrollback: VecDeque<ScrollbackRow>,
    /// Rows of [`Console::scrollback`] between the bottom of the view and the
    /// live screen. Zero is the live screen.
    view_offset: usize,
    /// Where a damaged span is composed before it is handed over. One text row
    /// wide and tall, or the scrollbar's column, whichever needs more.
    strip: Vec<u8>,
    /// Whether the strip below the last cell row has been painted. A panel is
    /// not obliged to be a whole number of glyph rows tall — 1080 is not, and
    /// 2048 is, which is why every screen test was blind to this — and no cell
    /// covers what is left over.
    margin_painted: bool,
}

impl Console {
    pub fn new(screen: Screen, font: Font) -> Self {
        let cols = screen.width() / font.width();
        let rows = screen.height() / font.height();
        let strip = vec![0u8; strip_bytes(screen.width(), screen.height(), font.height())];

        let mut console = Self {
            screen,
            font,
            cols,
            rows,
            cursor_col: 0,
            cursor_row: 0,
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            char_buf: vec![' '; cols * rows],
            fg_buf: vec![DEFAULT_FG; cols * rows],
            bg_buf: vec![DEFAULT_BG; cols * rows],
            wrapped: vec![false; rows],
            painted: vec![None; cols * rows],
            painted_scrollbar: None,
            ansi_state: AnsiState::Normal,
            ansi_buf: [0; 16],
            ansi_len: 0,
            reverse_video: false,
            cursor_enabled: true,
            saved_screen: None,
            utf8_buf: [0; 4],
            utf8_len: 0,
            utf8_needed: 0,
            sel_anchor: None,
            sel_end: None,
            scrollback: VecDeque::new(),
            view_offset: 0,
            strip,
            margin_painted: false,
        };

        console.flush();
        console
    }

    /// What the view says belongs at this screen position.
    fn view_cell(&self, row: usize, col: usize) -> Painted {
        let history = self.scrollback.len();
        let abs = history - self.view_offset + row;
        if abs < history {
            let sb = &self.scrollback[abs];
            return match sb.chars.get(col) {
                Some(&ch) => Painted { ch, fg: sb.fg[col], bg: sb.bg[col] },
                None => Painted { ch: ' ', fg: DEFAULT_FG, bg: DEFAULT_BG },
            };
        }
        let idx = (abs - history) * self.cols + col;
        let ch = self.char_buf[idx];
        if self.is_selected(idx) {
            return Painted { ch, fg: SEL_FG, bg: SEL_BG };
        }
        if self.cursor_enabled
            && self.view_offset == 0
            && idx == self.cursor_row * self.cols + self.cursor_col
        {
            return Painted { ch, fg: self.bg, bg: self.fg };
        }
        Painted { ch, fg: self.fg_buf[idx], bg: self.bg_buf[idx] }
    }

    /// Put the view on the panel, blitting only what changed.
    fn flush(&mut self) {
        let fw = self.font.width();
        let fh = self.font.height();
        let bar = self.scrollbar();
        if bar != self.painted_scrollbar {
            // The columns the bar sits over change hands in either direction:
            // to it, or back to the cells it was covering.
            let first = (self.screen.width() - SCROLLBAR_WIDTH) / fw;
            for row in 0..self.rows {
                self.painted[row * self.cols + first..(row + 1) * self.cols].fill(None);
            }
        }
        let paint_width = match bar {
            Some(_) => self.screen.width() - SCROLLBAR_WIDTH,
            None => self.screen.width(),
        };

        let mut strip = core::mem::take(&mut self.strip);
        for row in 0..self.rows {
            let Some((first, last)) = self.damage(row) else { continue };
            let span = last + 1 - first;
            let surface = Framebuffer::new(
                strip.as_mut_ptr(),
                span * fw,
                fh,
                span * fw,
                self.screen.pixel_format_raw(),
            );
            for col in first..=last {
                let cell = self.view_cell(row, col);
                self.font
                    .draw_char(&surface, (col - first) * fw, 0, cell.ch, cell.fg, cell.bg);
                self.painted[row * self.cols + col] = Some(cell);
            }
            let x = first * fw;
            let w = (span * fw).min(paint_width.saturating_sub(x));
            if w > 0 {
                self.screen.blit(x, row * fh, w, fh, span * fw, &strip);
            }
        }

        if bar != self.painted_scrollbar {
            if let Some((thumb_top, thumb_height)) = bar {
                let height = self.rows * fh;
                let surface = Framebuffer::new(
                    strip.as_mut_ptr(),
                    SCROLLBAR_WIDTH,
                    height,
                    SCROLLBAR_WIDTH,
                    self.screen.pixel_format_raw(),
                );
                surface.fill_rect(0, 0, SCROLLBAR_WIDTH, height, SCROLLBAR_TRACK);
                surface.fill_rect(0, thumb_top, SCROLLBAR_WIDTH, thumb_height, SCROLLBAR_THUMB);
                let x = self.screen.width() - SCROLLBAR_WIDTH;
                self.screen
                    .blit(x, 0, SCROLLBAR_WIDTH, height, SCROLLBAR_WIDTH, &strip);
            }
            self.painted_scrollbar = bar;
        }

        if !self.margin_painted {
            let covered = self.rows * fh;
            let margin = self.screen.height() - covered;
            if margin > 0 {
                let width = self.screen.width();
                let surface = Framebuffer::new(
                    strip.as_mut_ptr(),
                    width,
                    margin,
                    width,
                    self.screen.pixel_format_raw(),
                );
                surface.fill_rect(0, 0, width, margin, DEFAULT_BG);
                self.screen.blit(0, covered, width, margin, width, &strip);
            }
            self.margin_painted = true;
        }
        self.strip = strip;
    }

    /// The columns of `row` that no longer match the panel, as one span.
    ///
    /// One span rather than a list of them: the cells a scroll changes are
    /// contiguous far more often than not, and re-composing an unchanged cell
    /// inside the span costs system RAM where splitting the blit costs another
    /// pass over the mapping.
    fn damage(&self, row: usize) -> Option<(usize, usize)> {
        let base = row * self.cols;
        let changed =
            |col: &usize| self.painted[base + col] != Some(self.view_cell(row, *col));
        let first = (0..self.cols).find(changed)?;
        let last = (first..self.cols).rfind(changed).unwrap_or(first);
        Some((first, last))
    }

    /// The scrollbar thumb as `(top, height)`, or `None` when the view is at
    /// the bottom and there is nothing to indicate.
    fn scrollbar(&self) -> Option<(usize, usize)> {
        if self.view_offset == 0 {
            return None;
        }
        let fh = self.font.height();
        let viewport = self.rows * fh;
        let total = (self.scrollback.len() + self.rows) * fh;
        let thumb = (viewport * viewport / total).max(SCROLLBAR_THUMB_MIN).min(viewport);
        let track = viewport - thumb;
        Some((track - self.view_offset * track / self.scrollback.len(), thumb))
    }

    fn put_char(&mut self, col: usize, row: usize, ch: char) {
        let idx = row * self.cols + col;
        self.char_buf[idx] = ch;
        let (fg, bg) = if self.reverse_video {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        };
        self.fg_buf[idx] = fg;
        self.bg_buf[idx] = bg;
    }

    fn scroll(&mut self) {
        let row_size = self.cols;
        self.scrollback.push_back(ScrollbackRow {
            chars: self.char_buf[..row_size].to_vec(),
            fg: self.fg_buf[..row_size].to_vec(),
            bg: self.bg_buf[..row_size].to_vec(),
        });
        if self.scrollback.len() > SCROLLBACK_ROWS {
            self.scrollback.pop_front();
        }

        self.char_buf.copy_within(row_size.., 0);
        self.fg_buf.copy_within(row_size.., 0);
        self.bg_buf.copy_within(row_size.., 0);
        self.wrapped.copy_within(1.., 0);
        let last_row = (self.rows - 1) * row_size;
        self.char_buf[last_row..].fill(' ');
        self.fg_buf[last_row..].fill(DEFAULT_FG);
        self.bg_buf[last_row..].fill(DEFAULT_BG);
        self.wrapped[self.rows - 1] = false;
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
        self.char_buf.fill(' ');
        self.fg_buf.fill(DEFAULT_FG);
        self.bg_buf.fill(self.bg);
        self.wrapped.fill(false);
        self.cursor_col = 0;
        self.cursor_row = 0;
        self.forget_panel();
    }

    /// Drop every claim about what is on the panel, so the next flush paints
    /// all of it.
    ///
    /// `ESC[2J` says the panel is blank afterwards. That is not the same
    /// promise as "reach the state I believe is blank", and the difference is
    /// the whole of what a damage cache trades away: a cell the cache already
    /// records as blank is not repainted, so anything on the glass the cache
    /// cannot account for survives. Two kinds can be there — the strip below
    /// the last cell row on a panel whose height is not a whole number of
    /// them, and a paint from outside this process — and a user who has one
    /// reaches for exactly this command. It is the only sequence in the
    /// emulator that means "distrust what you think is up there", so it is the
    /// only one that spends a full repaint on saying so.
    fn forget_panel(&mut self) {
        self.painted.fill(None);
        self.painted_scrollbar = None;
        self.margin_painted = false;
    }

    fn emit_char(&mut self, ch: char) {
        if self.cursor_col >= self.cols {
            self.wrapped[self.cursor_row] = true;
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
            (25, b'l') => self.cursor_enabled = false,
            (25, b'h') => self.cursor_enabled = true,
            (1049, b'h') => {
                let n = self.cols * self.rows;
                let bg = self.bg;
                self.saved_screen = Some(SavedScreen {
                    char_buf: core::mem::replace(&mut self.char_buf, vec![' '; n]),
                    fg_buf: core::mem::replace(&mut self.fg_buf, vec![DEFAULT_FG; n]),
                    bg_buf: core::mem::replace(&mut self.bg_buf, vec![bg; n]),
                    wrapped: core::mem::replace(&mut self.wrapped, vec![false; self.rows]),
                    cursor_col: self.cursor_col,
                    cursor_row: self.cursor_row,
                });
                self.cursor_col = 0;
                self.cursor_row = 0;
            }
            (1049, b'l') => {
                if let Some(saved) = self.saved_screen.take() {
                    self.char_buf = saved.char_buf;
                    self.fg_buf = saved.fg_buf;
                    self.bg_buf = saved.bg_buf;
                    self.wrapped = saved.wrapped;
                    self.cursor_col = saved.cursor_col;
                    self.cursor_row = saved.cursor_row;
                }
            }
            _ => {}
        }
    }

    pub fn resize(&mut self, screen: Screen) {
        let new_cols = screen.width() / self.font.width();
        let new_rows = screen.height() / self.font.height();

        // Find cursor's offset within its logical line
        let mut cursor_line_offset = self.cursor_col;
        let mut r = self.cursor_row;
        while r > 0 && self.wrapped[r - 1] {
            r -= 1;
            cursor_line_offset += self.cols;
        }
        let cursor_logical_start = r;

        let mut new_char_buf = vec![' '; new_cols * new_rows];
        let mut new_wrapped = vec![false; new_rows];
        let mut new_cursor_row = 0;
        let mut new_cursor_col = 0;
        let mut dest_row = 0;
        let mut src_row = 0;

        while src_row < self.rows && dest_row < new_rows {
            let logical_start = src_row;

            // Collect one logical line (join soft-wrapped rows)
            let mut line: Vec<char> = Vec::new();
            loop {
                let start = src_row * self.cols;
                let row_chars = &self.char_buf[start..start + self.cols];

                if self.wrapped[src_row] {
                    // Wrapped row: all columns are content
                    line.extend_from_slice(row_chars);
                    src_row += 1;
                    if src_row >= self.rows { break; }
                } else {
                    // Final row: trim trailing spaces
                    let len = row_chars.iter().rposition(|&c| c != ' ')
                        .map_or(0, |p| p + 1);
                    line.extend_from_slice(&row_chars[..len]);
                    src_row += 1;
                    break;
                }
            }

            // Track cursor
            if logical_start == cursor_logical_start {
                new_cursor_row = dest_row + cursor_line_offset / new_cols;
                new_cursor_col = cursor_line_offset % new_cols;
            }

            if line.is_empty() {
                dest_row += 1;
                continue;
            }

            // Write logical line to new buffer, wrapping at new_cols
            let mut col = 0;
            for (i, &ch) in line.iter().enumerate() {
                if dest_row >= new_rows { break; }
                new_char_buf[dest_row * new_cols + col] = ch;
                col += 1;
                if col >= new_cols && i + 1 < line.len() {
                    new_wrapped[dest_row] = true;
                    dest_row += 1;
                    col = 0;
                }
            }
            dest_row += 1;
        }

        self.strip = vec![0u8; strip_bytes(screen.width(), screen.height(), self.font.height())];
        self.screen = screen;
        self.cols = new_cols;
        self.rows = new_rows;
        self.char_buf = new_char_buf;
        self.fg_buf = vec![DEFAULT_FG; new_cols * new_rows];
        self.bg_buf = vec![DEFAULT_BG; new_cols * new_rows];
        self.wrapped = new_wrapped;
        self.painted = vec![None; new_cols * new_rows];
        self.painted_scrollbar = None;
        self.margin_painted = false;
        self.cursor_row = new_cursor_row.min(new_rows.saturating_sub(1));
        self.cursor_col = new_cursor_col.min(new_cols.saturating_sub(1));
        self.saved_screen = None;
        self.sel_anchor = None;
        self.sel_end = None;
        self.view_offset = 0;
        self.flush();
    }

    pub fn font_width(&self) -> usize {
        self.font.width()
    }

    pub fn font_height(&self) -> usize {
        self.font.height()
    }

    /// Bytes this console has put on the panel, and the blits that carried
    /// them.
    pub fn screen_traffic(&self) -> (u64, u64) {
        self.screen.traffic()
    }

    fn selection_range(&self) -> Option<(usize, usize)> {
        let (ac, ar) = self.sel_anchor?;
        let (ec, er) = self.sel_end?;
        let a = ar * self.cols + ac;
        let b = er * self.cols + ec;
        if a <= b { Some((a, b)) } else { Some((b, a)) }
    }

    fn is_selected(&self, idx: usize) -> bool {
        match self.selection_range() {
            Some((start, end)) => idx >= start && idx <= end,
            None => false,
        }
    }

    pub fn mouse_down(&mut self, col: usize, row: usize) {
        let col = col.min(self.cols.saturating_sub(1));
        let row = row.min(self.rows.saturating_sub(1));
        self.sel_anchor = Some((col, row));
        self.sel_end = Some((col, row));
        self.flush();
    }

    pub fn mouse_drag(&mut self, col: usize, row: usize) {
        if self.sel_anchor.is_none() {
            return;
        }
        self.sel_end = Some((
            col.min(self.cols.saturating_sub(1)),
            row.min(self.rows.saturating_sub(1)),
        ));
        self.flush();
    }

    pub fn mouse_up(&mut self, col: usize, row: usize) -> Option<String> {
        if self.sel_anchor.is_none() {
            return None;
        }
        self.sel_end = Some((
            col.min(self.cols.saturating_sub(1)),
            row.min(self.rows.saturating_sub(1)),
        ));
        self.flush();
        self.selected_text()
    }

    fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection_range()?;
        if start == end {
            return None;
        }
        let mut result = String::new();
        let start_row = start / self.cols;
        let end_row = end / self.cols;
        for row in start_row..=end_row {
            let row_start = if row == start_row { start % self.cols } else { 0 };
            let row_end = if row == end_row { end % self.cols } else { self.cols - 1 };
            let mut line = String::new();
            for col in row_start..=row_end {
                let idx = row * self.cols + col;
                line.push(self.char_buf[idx]);
            }
            let trimmed = line.trim_end();
            result.push_str(trimmed);
            if row < end_row && !self.wrapped[row] {
                result.push('\n');
            }
        }
        Some(result)
    }

    pub fn get_selection(&self) -> Option<String> {
        self.selected_text()
    }

    /// Scroll the view `rows` text rows up, into history.
    pub fn scroll_view_up(&mut self, rows: usize) {
        let want = (self.view_offset + rows).min(self.scrollback.len());
        if want == self.view_offset {
            return;
        }
        self.view_offset = want;
        self.flush();
    }

    /// Scroll the view `rows` text rows down, toward the live screen.
    pub fn scroll_view_down(&mut self, rows: usize) {
        let want = self.view_offset.saturating_sub(rows);
        if want == self.view_offset {
            return;
        }
        self.view_offset = want;
        self.flush();
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.sel_anchor = None;
        self.sel_end = None;
        self.view_offset = 0;
        for &byte in bytes {
            self.write_byte(byte);
        }
        self.flush();
    }
}

/// Room for a full-width text row, or for the scrollbar's full-height column.
fn strip_bytes(width: usize, height: usize, font_height: usize) -> usize {
    (width * font_height).max(SCROLLBAR_WIDTH * height) * 4
}

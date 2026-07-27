use filepicker_api::PickerMode;
use font::Font;
use std::env;
use std::fs;
use std::time::Instant;
use window::{Color, Event, Framebuffer, KeyEvent, MouseEvent, Window};

// --- Colors (Catppuccin Mocha-inspired) ---

const BG: Color = Color { r: 0x1e, g: 0x1e, b: 0x2e };
const GUTTER_BG: Color = Color { r: 0x18, g: 0x18, b: 0x25 };
const GUTTER_FG: Color = Color { r: 0x58, g: 0x5b, b: 0x70 };
const TEXT_FG: Color = Color { r: 0xcd, g: 0xd6, b: 0xf4 };
const KEYWORD_FG: Color = Color { r: 0xcb, g: 0xa6, b: 0xf7 };
const TYPE_FG: Color = Color { r: 0x89, g: 0xb4, b: 0xfa };
const STRING_FG: Color = Color { r: 0xa6, g: 0xe3, b: 0xa1 };
const COMMENT_FG: Color = Color { r: 0x6c, g: 0x70, b: 0x86 };
const NUMBER_FG: Color = Color { r: 0xfa, g: 0xb3, b: 0x87 };
const PREPROC_FG: Color = Color { r: 0xf3, g: 0x8b, b: 0xa8 };
const SELECTION_BG: Color = Color { r: 0x45, g: 0x47, b: 0x5a };
const CURLINE_BG: Color = Color { r: 0x26, g: 0x26, b: 0x37 };
const STATUS_BG: Color = Color { r: 0x31, g: 0x32, b: 0x44 };
const STATUS_FG: Color = Color { r: 0xcd, g: 0xd6, b: 0xf4 };
const FINDBAR_BG: Color = Color { r: 0x31, g: 0x32, b: 0x44 };
const MATCH_BG: Color = Color { r: 0xf9, g: 0xe2, b: 0xaf };
const MATCH_FG: Color = Color { r: 0x1e, g: 0x1e, b: 0x2e };
const CURSOR_COLOR: Color = Color { r: 0xf5, g: 0xe0, b: 0xdc };
const FINDBAR_LABEL: Color = Color { r: 0xa6, g: 0xad, b: 0xc8 };
const FINDBAR_INPUT_BG: Color = Color { r: 0x1e, g: 0x1e, b: 0x2e };

// --- HID keycodes ---

const KEY_UP: u8 = 0x52;
const KEY_DOWN: u8 = 0x51;
const KEY_LEFT: u8 = 0x50;
const KEY_RIGHT: u8 = 0x4F;
const KEY_HOME: u8 = 0x4A;
const KEY_END: u8 = 0x4D;
const KEY_PAGEUP: u8 = 0x4B;
const KEY_PAGEDOWN: u8 = 0x4E;
const KEY_BACKSPACE: u8 = 0x2A;
const KEY_DELETE: u8 = 0x4C;
const KEY_ENTER: u8 = 0x28;
const KEY_TAB: u8 = 0x2B;
const KEY_ESCAPE: u8 = 0x29;

// --- Syntax highlighting ---

#[derive(Clone, Copy, PartialEq)]
enum TokenKind {
    Normal,
    Keyword,
    Type,
    String,
    Char,
    Comment,
    Number,
    Preprocessor,
}

fn token_color(kind: TokenKind) -> Color {
    match kind {
        TokenKind::Normal => TEXT_FG,
        TokenKind::Keyword => KEYWORD_FG,
        TokenKind::Type => TYPE_FG,
        TokenKind::String | TokenKind::Char => STRING_FG,
        TokenKind::Comment => COMMENT_FG,
        TokenKind::Number => NUMBER_FG,
        TokenKind::Preprocessor => PREPROC_FG,
    }
}

const C_KEYWORDS: &[&str] = &[
    "auto", "break", "case", "const", "continue", "default", "do", "else", "enum", "extern",
    "for", "goto", "if", "inline", "register", "restrict", "return", "sizeof", "static",
    "struct", "switch", "typedef", "union", "volatile", "while", "_Alignas", "_Alignof",
    "_Atomic", "_Bool", "_Complex", "_Generic", "_Imaginary", "_Noreturn", "_Static_assert",
    "_Thread_local",
];

const C_TYPES: &[&str] = &[
    "void", "char", "short", "int", "long", "float", "double", "signed", "unsigned", "size_t",
    "ssize_t", "uint8_t", "int8_t", "uint16_t", "int16_t", "uint32_t", "int32_t", "uint64_t",
    "int64_t", "uintptr_t", "intptr_t", "ptrdiff_t", "bool", "NULL", "true", "false", "FILE",
];

/// Highlight a single line, returning (byte_offset, kind) spans.
/// `in_block_comment` is the state entering this line; returns the state exiting.
fn highlight_line(line: &str, in_block_comment: bool) -> (Vec<(usize, TokenKind)>, bool) {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut spans: Vec<(usize, TokenKind)> = Vec::new();
    let mut i = 0;
    let mut in_comment = in_block_comment;

    while i < len {
        if in_comment {
            let start = i;
            while i + 1 < len {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    in_comment = false;
                    break;
                }
                i += 1;
            }
            if in_comment {
                i = len;
            }
            spans.push((start, TokenKind::Comment));
            continue;
        }

        // Skip whitespace
        if bytes[i] == b' ' || bytes[i] == b'\t' {
            spans.push((i, TokenKind::Normal));
            i += 1;
            continue;
        }

        // Line comment
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            spans.push((i, TokenKind::Comment));
            return (spans, in_comment);
        }

        // Block comment start
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            in_comment = true;
            while i + 1 < len {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    in_comment = false;
                    break;
                }
                i += 1;
            }
            if in_comment {
                i = len;
            }
            spans.push((start, TokenKind::Comment));
            continue;
        }

        // Preprocessor (only at line start, ignoring whitespace)
        if bytes[i] == b'#' && line[..i].trim().is_empty() {
            spans.push((i, TokenKind::Preprocessor));
            return (spans, in_comment);
        }

        // String literal
        if bytes[i] == b'"' {
            let start = i;
            i += 1;
            while i < len {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 2;
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            spans.push((start, TokenKind::String));
            continue;
        }

        // Char literal
        if bytes[i] == b'\'' {
            let start = i;
            i += 1;
            while i < len {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 2;
                } else if bytes[i] == b'\'' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            spans.push((start, TokenKind::Char));
            continue;
        }

        // Number
        if bytes[i].is_ascii_digit()
            || (bytes[i] == b'.' && i + 1 < len && bytes[i + 1].is_ascii_digit())
        {
            let start = i;
            if bytes[i] == b'0' && i + 1 < len && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X')
            {
                i += 2;
                while i < len && bytes[i].is_ascii_hexdigit() {
                    i += 1;
                }
            } else if bytes[i] == b'0'
                && i + 1 < len
                && (bytes[i + 1] == b'b' || bytes[i + 1] == b'B')
            {
                i += 2;
                while i < len && (bytes[i] == b'0' || bytes[i] == b'1') {
                    i += 1;
                }
            } else {
                while i < len && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                if i < len && (bytes[i] == b'e' || bytes[i] == b'E') {
                    i += 1;
                    if i < len && (bytes[i] == b'+' || bytes[i] == b'-') {
                        i += 1;
                    }
                    while i < len && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            // Suffixes: u, l, ll, f, etc.
            while i < len
                && (bytes[i] == b'u'
                    || bytes[i] == b'U'
                    || bytes[i] == b'l'
                    || bytes[i] == b'L'
                    || bytes[i] == b'f'
                    || bytes[i] == b'F')
            {
                i += 1;
            }
            spans.push((start, TokenKind::Number));
            continue;
        }

        // Identifier or keyword
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            let kind = if C_KEYWORDS.contains(&word) {
                TokenKind::Keyword
            } else if C_TYPES.contains(&word) {
                TokenKind::Type
            } else {
                TokenKind::Normal
            };
            spans.push((start, kind));
            continue;
        }

        // Other characters (operators, punctuation)
        spans.push((i, TokenKind::Normal));
        i += 1;
    }

    (spans, in_comment)
}

// --- Edit operations for undo/redo ---

#[derive(Clone)]
enum Edit {
    Insert {
        row: usize,
        col: usize,
        text: String,
    },
    Delete {
        row: usize,
        col: usize,
        text: String,
    },
    Batch(Vec<Edit>),
}

impl Edit {
    fn apply(&self, lines: &mut Vec<String>) -> (usize, usize) {
        match self {
            Edit::Insert { row, col, text } => insert_text(lines, *row, *col, text),
            Edit::Delete { row, col, text } => {
                let (end_row, end_col) = compute_end(*row, *col, text);
                delete_range(lines, *row, *col, end_row, end_col);
                (*row, *col)
            }
            Edit::Batch(edits) => {
                let mut pos = (0, 0);
                for e in edits {
                    pos = e.apply(lines);
                }
                pos
            }
        }
    }

    fn invert(&self) -> Edit {
        match self {
            Edit::Insert { row, col, text } => Edit::Delete {
                row: *row,
                col: *col,
                text: text.clone(),
            },
            Edit::Delete { row, col, text } => Edit::Insert {
                row: *row,
                col: *col,
                text: text.clone(),
            },
            Edit::Batch(edits) => Edit::Batch(edits.iter().rev().map(|e| e.invert()).collect()),
        }
    }
}

fn compute_end(row: usize, col: usize, text: &str) -> (usize, usize) {
    let mut r = row;
    let mut c = col;
    for ch in text.chars() {
        if ch == '\n' {
            r += 1;
            c = 0;
        } else {
            c += 1;
        }
    }
    (r, c)
}

fn insert_text(lines: &mut Vec<String>, row: usize, col: usize, text: &str) -> (usize, usize) {
    if lines.is_empty() {
        lines.push(String::new());
    }
    let r = row.min(lines.len() - 1);
    let c = col.min(lines[r].len());

    let after = lines[r][c..].to_string();
    lines[r].truncate(c);

    let mut cur_row = r;
    for (i, part) in text.split('\n').enumerate() {
        if i == 0 {
            lines[cur_row].push_str(part);
        } else {
            cur_row += 1;
            lines.insert(cur_row, part.to_string());
        }
    }

    let end_col = lines[cur_row].len();
    lines[cur_row].push_str(&after);
    (cur_row, end_col)
}

fn delete_range(
    lines: &mut Vec<String>,
    r1: usize,
    c1: usize,
    r2: usize,
    c2: usize,
) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let r1 = r1.min(lines.len() - 1);
    let c1 = c1.min(lines[r1].len());
    let r2 = r2.min(lines.len() - 1);
    let c2 = c2.min(lines[r2].len());

    if r1 == r2 {
        let (start, end) = if c1 <= c2 { (c1, c2) } else { (c2, c1) };
        let deleted: String = lines[r1][start..end].to_string();
        lines[r1] = format!("{}{}", &lines[r1][..start], &lines[r1][end..]);
        return deleted;
    }

    let mut deleted = lines[r1][c1..].to_string();
    deleted.push('\n');
    for row in (r1 + 1)..r2 {
        deleted.push_str(&lines[row]);
        deleted.push('\n');
    }
    deleted.push_str(&lines[r2][..c2]);

    let remaining = lines[r2][c2..].to_string();
    lines.drain((r1 + 1)..=r2);
    lines[r1].truncate(c1);
    lines[r1].push_str(&remaining);

    deleted
}

// --- Buffer ---

struct Buffer {
    lines: Vec<String>,
    path: Option<String>,
    dirty: bool,
    undo_stack: Vec<Edit>,
    redo_stack: Vec<Edit>,
}

impl Buffer {
    fn new() -> Self {
        Self {
            lines: vec![String::new()],
            path: None,
            dirty: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    fn from_file(path: &str) -> Self {
        let content = fs::read_to_string(path).unwrap_or_default();
        let mut lines: Vec<String> = content.lines().map(String::from).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            lines,
            path: Some(path.to_string()),
            dirty: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    fn save(&mut self) -> Result<(), String> {
        let path = self.path.as_ref().ok_or("No file path")?;
        let content = self.lines.join("\n");
        fs::write(path, &content).map_err(|e| e.to_string())?;
        self.dirty = false;
        Ok(())
    }

    fn apply_edit(&mut self, edit: Edit) -> (usize, usize) {
        let pos = edit.apply(&mut self.lines);
        self.undo_stack.push(edit);
        self.redo_stack.clear();
        self.dirty = true;
        pos
    }

    fn undo(&mut self) -> Option<(usize, usize)> {
        let edit = self.undo_stack.pop()?;
        let inv = edit.invert();
        let pos = inv.apply(&mut self.lines);
        self.redo_stack.push(edit);
        self.dirty = true;
        Some(pos)
    }

    fn redo(&mut self) -> Option<(usize, usize)> {
        let edit = self.redo_stack.pop()?;
        let pos = edit.apply(&mut self.lines);
        self.undo_stack.push(edit);
        self.dirty = true;
        Some(pos)
    }
}

// --- Find state ---

#[derive(PartialEq)]
enum FindMode {
    Find,
    Replace,
}

struct Finder {
    active: bool,
    mode: FindMode,
    query: String,
    replace: String,
    query_cursor: usize,
    replace_cursor: usize,
    editing_replace: bool,
    matches: Vec<(usize, usize, usize)>, // (row, col, len)
    current_match: usize,
}

impl Finder {
    fn new() -> Self {
        Self {
            active: false,
            mode: FindMode::Find,
            query: String::new(),
            replace: String::new(),
            query_cursor: 0,
            replace_cursor: 0,
            editing_replace: false,
            matches: Vec::new(),
            current_match: 0,
        }
    }

    fn search(&mut self, lines: &[String]) {
        self.matches.clear();
        if self.query.is_empty() {
            return;
        }
        for (row, line) in lines.iter().enumerate() {
            let mut start = 0;
            while let Some(pos) = line[start..].find(&self.query) {
                self.matches.push((row, start + pos, self.query.len()));
                start += pos + 1;
            }
        }
        if !self.matches.is_empty() {
            self.current_match = self.current_match.min(self.matches.len() - 1);
        }
    }

    fn find_next(&mut self) {
        if !self.matches.is_empty() {
            self.current_match = (self.current_match + 1) % self.matches.len();
        }
    }

    fn find_prev(&mut self) {
        if !self.matches.is_empty() {
            self.current_match = if self.current_match == 0 {
                self.matches.len() - 1
            } else {
                self.current_match - 1
            };
        }
    }

    fn find_nearest(&mut self, row: usize, col: usize) {
        if self.matches.is_empty() {
            return;
        }
        let mut best = 0;
        for (i, &(mr, mc, _)) in self.matches.iter().enumerate() {
            if mr > row || (mr == row && mc >= col) {
                best = i;
                break;
            }
            best = i;
        }
        self.current_match = best;
    }
}

// --- Go-to-line dialog ---

struct GoToLine {
    active: bool,
    input: String,
}

impl GoToLine {
    fn new() -> Self {
        Self {
            active: false,
            input: String::new(),
        }
    }
}

// --- Editor state ---

struct Editor {
    buffer: Buffer,
    cursor_row: usize,
    cursor_col: usize,
    desired_col: usize,
    anchor: Option<(usize, usize)>,
    scroll_row: usize,
    scroll_col: usize,
    finder: Finder,
    goto_line: GoToLine,

    // Layout
    font_w: usize,
    font_h: usize,
    gutter_width: usize,
    status_height: usize,
    findbar_height: usize,
}

impl Editor {
    fn new(buffer: Buffer, font_w: usize, font_h: usize) -> Self {
        let gutter_width = Self::compute_gutter_width(buffer.lines.len(), font_w);
        Self {
            buffer,
            cursor_row: 0,
            cursor_col: 0,
            desired_col: 0,
            anchor: None,
            scroll_row: 0,
            scroll_col: 0,
            finder: Finder::new(),
            goto_line: GoToLine::new(),
            font_w,
            font_h,
            gutter_width,
            status_height: font_h + 4,
            findbar_height: font_h + 8,
        }
    }

    fn compute_gutter_width(line_count: usize, font_w: usize) -> usize {
        let digits = format!("{}", line_count).len().max(3);
        (digits + 2) * font_w
    }

    fn update_gutter(&mut self) {
        self.gutter_width = Self::compute_gutter_width(self.buffer.lines.len(), self.font_w);
    }

    fn visible_lines(&self, win_h: usize) -> usize {
        let top = if self.finder.active {
            self.findbar_height
        } else {
            0
        };
        (win_h.saturating_sub(self.status_height + top)) / self.font_h
    }

    fn text_area_top(&self) -> usize {
        if self.finder.active {
            self.findbar_height
        } else {
            0
        }
    }

    fn ensure_cursor_visible(&mut self, win_h: usize, win_w: usize) {
        let vis = self.visible_lines(win_h);
        if vis == 0 {
            return;
        }
        if self.cursor_row < self.scroll_row {
            self.scroll_row = self.cursor_row;
        } else if self.cursor_row >= self.scroll_row + vis {
            self.scroll_row = self.cursor_row - vis + 1;
        }

        let text_cols = (win_w.saturating_sub(self.gutter_width)) / self.font_w;
        if text_cols > 0 {
            if self.cursor_col < self.scroll_col {
                self.scroll_col = self.cursor_col;
            } else if self.cursor_col >= self.scroll_col + text_cols {
                self.scroll_col = self.cursor_col - text_cols + 1;
            }
        }
    }

    fn clamp_cursor(&mut self) {
        if self.cursor_row >= self.buffer.lines.len() {
            self.cursor_row = self.buffer.lines.len() - 1;
        }
        let line_len = self.buffer.lines[self.cursor_row].len();
        if self.cursor_col > line_len {
            self.cursor_col = line_len;
        }
    }

    fn selection_ordered(&self) -> Option<((usize, usize), (usize, usize))> {
        let anchor = self.anchor?;
        let cursor = (self.cursor_row, self.cursor_col);
        if anchor <= cursor {
            Some((anchor, cursor))
        } else {
            Some((cursor, anchor))
        }
    }

    fn selected_text(&self) -> Option<String> {
        let ((r1, c1), (r2, c2)) = self.selection_ordered()?;
        if r1 == r2 {
            Some(self.buffer.lines[r1][c1..c2].to_string())
        } else {
            let mut s = self.buffer.lines[r1][c1..].to_string();
            s.push('\n');
            for row in (r1 + 1)..r2 {
                s.push_str(&self.buffer.lines[row]);
                s.push('\n');
            }
            s.push_str(&self.buffer.lines[r2][..c2]);
            Some(s)
        }
    }

    fn delete_selection(&mut self) -> Option<String> {
        let ((r1, c1), (r2, c2)) = self.selection_ordered()?;
        let text = delete_range(&mut self.buffer.lines, r1, c1, r2, c2);
        self.buffer.undo_stack.push(Edit::Delete {
            row: r1,
            col: c1,
            text: text.clone(),
        });
        self.buffer.redo_stack.clear();
        self.buffer.dirty = true;
        self.cursor_row = r1;
        self.cursor_col = c1;
        self.anchor = None;
        self.update_gutter();
        Some(text)
    }

    fn insert_char(&mut self, ch: char) {
        if self.anchor.is_some() {
            self.delete_selection();
        }
        let text = ch.to_string();
        let (r, c) = self.buffer.apply_edit(Edit::Insert {
            row: self.cursor_row,
            col: self.cursor_col,
            text,
        });
        self.cursor_row = r;
        self.cursor_col = c;
        self.desired_col = self.cursor_col;
        self.update_gutter();
    }

    fn insert_newline(&mut self) {
        if self.anchor.is_some() {
            self.delete_selection();
        }
        // Auto-indent: copy leading whitespace from current line
        let indent: String = self.buffer.lines[self.cursor_row]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let text = format!("\n{}", indent);
        let (r, c) = self.buffer.apply_edit(Edit::Insert {
            row: self.cursor_row,
            col: self.cursor_col,
            text,
        });
        self.cursor_row = r;
        self.cursor_col = c;
        self.desired_col = self.cursor_col;
        self.update_gutter();
    }

    fn backspace(&mut self) {
        if self.anchor.is_some() {
            self.delete_selection();
            return;
        }
        if self.cursor_col > 0 {
            let ch = self.buffer.lines[self.cursor_row]
                .chars()
                .nth(self.cursor_col - 1)
                .unwrap_or(' ');
            self.buffer.apply_edit(Edit::Delete {
                row: self.cursor_row,
                col: self.cursor_col - 1,
                text: ch.to_string(),
            });
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            let prev_len = self.buffer.lines[self.cursor_row - 1].len();
            self.buffer.apply_edit(Edit::Delete {
                row: self.cursor_row - 1,
                col: prev_len,
                text: "\n".to_string(),
            });
            self.cursor_row -= 1;
            self.cursor_col = prev_len;
        }
        self.desired_col = self.cursor_col;
        self.update_gutter();
    }

    fn delete_forward(&mut self) {
        if self.anchor.is_some() {
            self.delete_selection();
            return;
        }
        let line_len = self.buffer.lines[self.cursor_row].len();
        if self.cursor_col < line_len {
            let ch = self.buffer.lines[self.cursor_row]
                .chars()
                .nth(self.cursor_col)
                .unwrap_or(' ');
            self.buffer.apply_edit(Edit::Delete {
                row: self.cursor_row,
                col: self.cursor_col,
                text: ch.to_string(),
            });
        } else if self.cursor_row + 1 < self.buffer.lines.len() {
            self.buffer.apply_edit(Edit::Delete {
                row: self.cursor_row,
                col: self.cursor_col,
                text: "\n".to_string(),
            });
        }
        self.update_gutter();
    }

    fn delete_word_backward(&mut self) {
        if self.anchor.is_some() {
            self.delete_selection();
            return;
        }
        if self.cursor_col == 0 {
            self.backspace();
            return;
        }
        let line = &self.buffer.lines[self.cursor_row];
        let mut end = self.cursor_col;
        // Skip whitespace
        while end > 0 && line.as_bytes()[end - 1] == b' ' {
            end -= 1;
        }
        // Skip word chars
        while end > 0 && (line.as_bytes()[end - 1].is_ascii_alphanumeric() || line.as_bytes()[end - 1] == b'_') {
            end -= 1;
        }
        if end == self.cursor_col {
            end -= 1; // Delete at least one char
        }
        let text = line[end..self.cursor_col].to_string();
        self.buffer.apply_edit(Edit::Delete {
            row: self.cursor_row,
            col: end,
            text,
        });
        self.cursor_col = end;
        self.desired_col = self.cursor_col;
    }

    fn insert_tab(&mut self) {
        if let Some(((r1, _), (r2, _))) = self.selection_ordered() {
            // Indent selected lines
            let mut edits = Vec::new();
            for row in r1..=r2 {
                edits.push(Edit::Insert {
                    row,
                    col: 0,
                    text: "    ".to_string(),
                });
            }
            // Apply in reverse so row offsets don't shift
            for edit in edits.iter().rev() {
                edit.apply(&mut self.buffer.lines);
            }
            self.buffer.undo_stack.push(Edit::Batch(edits));
            self.buffer.redo_stack.clear();
            self.buffer.dirty = true;
            // Adjust selection
            if let Some(ref mut anchor) = self.anchor {
                anchor.1 += 4;
            }
            self.cursor_col += 4;
            self.desired_col = self.cursor_col;
        } else {
            let text = "    ".to_string();
            let (r, c) = self.buffer.apply_edit(Edit::Insert {
                row: self.cursor_row,
                col: self.cursor_col,
                text,
            });
            self.cursor_row = r;
            self.cursor_col = c;
            self.desired_col = self.cursor_col;
        }
    }

    fn dedent(&mut self) {
        let (r1, r2) = if let Some(((r1, _), (r2, _))) = self.selection_ordered() {
            (r1, r2)
        } else {
            (self.cursor_row, self.cursor_row)
        };
        let mut edits = Vec::new();
        for row in r1..=r2 {
            let line = &self.buffer.lines[row];
            let spaces = line.len() - line.trim_start_matches(' ').len();
            let remove = spaces.min(4);
            if remove > 0 {
                edits.push(Edit::Delete {
                    row,
                    col: 0,
                    text: " ".repeat(remove),
                });
            }
        }
        if edits.is_empty() {
            return;
        }
        for edit in edits.iter().rev() {
            edit.apply(&mut self.buffer.lines);
        }
        self.buffer.undo_stack.push(Edit::Batch(edits));
        self.buffer.redo_stack.clear();
        self.buffer.dirty = true;
        self.clamp_cursor();
        self.desired_col = self.cursor_col;
    }

    fn duplicate_line(&mut self) {
        let line = self.buffer.lines[self.cursor_row].clone();
        let text = format!("\n{}", line);
        let line_len = self.buffer.lines[self.cursor_row].len();
        self.buffer.apply_edit(Edit::Insert {
            row: self.cursor_row,
            col: line_len,
            text,
        });
        self.cursor_row += 1;
        self.update_gutter();
    }

    fn select_all(&mut self) {
        self.anchor = Some((0, 0));
        let last = self.buffer.lines.len() - 1;
        self.cursor_row = last;
        self.cursor_col = self.buffer.lines[last].len();
    }

    fn move_cursor(&mut self, drow: isize, dcol: isize, shift: bool) {
        if shift && self.anchor.is_none() {
            self.anchor = Some((self.cursor_row, self.cursor_col));
        } else if !shift {
            self.anchor = None;
        }

        if drow != 0 {
            let new_row = (self.cursor_row as isize + drow)
                .max(0)
                .min(self.buffer.lines.len() as isize - 1) as usize;
            self.cursor_row = new_row;
            self.cursor_col = self.desired_col.min(self.buffer.lines[self.cursor_row].len());
        }
        if dcol != 0 {
            let line_len = self.buffer.lines[self.cursor_row].len();
            let new_col = self.cursor_col as isize + dcol;
            if new_col < 0 {
                if self.cursor_row > 0 {
                    self.cursor_row -= 1;
                    self.cursor_col = self.buffer.lines[self.cursor_row].len();
                }
            } else if new_col as usize > line_len {
                if self.cursor_row + 1 < self.buffer.lines.len() {
                    self.cursor_row += 1;
                    self.cursor_col = 0;
                }
            } else {
                self.cursor_col = new_col as usize;
            }
            self.desired_col = self.cursor_col;
        }
    }

    fn move_word_left(&mut self, shift: bool) {
        if shift && self.anchor.is_none() {
            self.anchor = Some((self.cursor_row, self.cursor_col));
        } else if !shift {
            self.anchor = None;
        }

        if self.cursor_col == 0 {
            if self.cursor_row > 0 {
                self.cursor_row -= 1;
                self.cursor_col = self.buffer.lines[self.cursor_row].len();
            }
            self.desired_col = self.cursor_col;
            return;
        }

        let line = &self.buffer.lines[self.cursor_row];
        let bytes = line.as_bytes();
        let mut pos = self.cursor_col;
        while pos > 0 && bytes[pos - 1] == b' ' {
            pos -= 1;
        }
        while pos > 0 && (bytes[pos - 1].is_ascii_alphanumeric() || bytes[pos - 1] == b'_') {
            pos -= 1;
        }
        if pos == self.cursor_col && pos > 0 {
            pos -= 1;
        }
        self.cursor_col = pos;
        self.desired_col = self.cursor_col;
    }

    fn move_word_right(&mut self, shift: bool) {
        if shift && self.anchor.is_none() {
            self.anchor = Some((self.cursor_row, self.cursor_col));
        } else if !shift {
            self.anchor = None;
        }

        let line_len = self.buffer.lines[self.cursor_row].len();
        if self.cursor_col >= line_len {
            if self.cursor_row + 1 < self.buffer.lines.len() {
                self.cursor_row += 1;
                self.cursor_col = 0;
            }
            self.desired_col = self.cursor_col;
            return;
        }

        let line = &self.buffer.lines[self.cursor_row];
        let bytes = line.as_bytes();
        let mut pos = self.cursor_col;
        while pos < line_len && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'_') {
            pos += 1;
        }
        while pos < line_len && bytes[pos] == b' ' {
            pos += 1;
        }
        if pos == self.cursor_col {
            pos += 1;
        }
        self.cursor_col = pos.min(line_len);
        self.desired_col = self.cursor_col;
    }

    fn home(&mut self, shift: bool) {
        if shift && self.anchor.is_none() {
            self.anchor = Some((self.cursor_row, self.cursor_col));
        } else if !shift {
            self.anchor = None;
        }
        // Smart home: toggle between first non-space and column 0
        let first_nonspace = self.buffer.lines[self.cursor_row]
            .len()
            - self.buffer.lines[self.cursor_row].trim_start().len();
        if self.cursor_col == first_nonspace {
            self.cursor_col = 0;
        } else {
            self.cursor_col = first_nonspace;
        }
        self.desired_col = self.cursor_col;
    }

    fn end(&mut self, shift: bool) {
        if shift && self.anchor.is_none() {
            self.anchor = Some((self.cursor_row, self.cursor_col));
        } else if !shift {
            self.anchor = None;
        }
        self.cursor_col = self.buffer.lines[self.cursor_row].len();
        self.desired_col = self.cursor_col;
    }

    fn page_up(&mut self, win_h: usize) {
        let vis = self.visible_lines(win_h);
        self.cursor_row = self.cursor_row.saturating_sub(vis);
        self.cursor_col = self.desired_col.min(self.buffer.lines[self.cursor_row].len());
        self.scroll_row = self.scroll_row.saturating_sub(vis);
        self.anchor = None;
    }

    fn page_down(&mut self, win_h: usize) {
        let vis = self.visible_lines(win_h);
        self.cursor_row = (self.cursor_row + vis).min(self.buffer.lines.len() - 1);
        self.cursor_col = self.desired_col.min(self.buffer.lines[self.cursor_row].len());
        self.scroll_row = (self.scroll_row + vis).min(self.buffer.lines.len().saturating_sub(1));
        self.anchor = None;
    }

    fn goto_file_start(&mut self) {
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.desired_col = 0;
        self.anchor = None;
    }

    fn goto_file_end(&mut self) {
        self.cursor_row = self.buffer.lines.len() - 1;
        self.cursor_col = self.buffer.lines[self.cursor_row].len();
        self.desired_col = self.cursor_col;
        self.anchor = None;
    }

    fn replace_current(&mut self) {
        if self.finder.matches.is_empty() {
            return;
        }
        let (mr, mc, ml) = self.finder.matches[self.finder.current_match];
        let old = self.buffer.lines[mr][mc..mc + ml].to_string();
        let new = self.finder.replace.clone();
        delete_range(&mut self.buffer.lines, mr, mc, mr, mc + ml);
        insert_text(&mut self.buffer.lines, mr, mc, &new);
        self.buffer.undo_stack.push(Edit::Batch(vec![
            Edit::Delete {
                row: mr,
                col: mc,
                text: old,
            },
            Edit::Insert {
                row: mr,
                col: mc,
                text: new,
            },
        ]));
        self.buffer.redo_stack.clear();
        self.buffer.dirty = true;
        self.finder.search(&self.buffer.lines);
        self.update_gutter();
    }

    fn replace_all(&mut self) {
        if self.finder.matches.is_empty() {
            return;
        }
        let mut batch = Vec::new();
        // Process in reverse to keep positions valid
        let matches: Vec<_> = self.finder.matches.clone();
        for &(mr, mc, ml) in matches.iter().rev() {
            let old = self.buffer.lines[mr][mc..mc + ml].to_string();
            let new = self.finder.replace.clone();
            delete_range(&mut self.buffer.lines, mr, mc, mr, mc + ml);
            insert_text(&mut self.buffer.lines, mr, mc, &new);
            batch.push(Edit::Delete {
                row: mr,
                col: mc,
                text: old,
            });
            batch.push(Edit::Insert {
                row: mr,
                col: mc,
                text: new,
            });
        }
        batch.reverse();
        self.buffer.undo_stack.push(Edit::Batch(batch));
        self.buffer.redo_stack.clear();
        self.buffer.dirty = true;
        self.finder.search(&self.buffer.lines);
        self.update_gutter();
    }

    fn pixel_to_pos(&self, px: usize, py: usize) -> (usize, usize) {
        let top = self.text_area_top();
        let row = if py >= top {
            self.scroll_row + (py - top) / self.font_h
        } else {
            self.scroll_row
        };
        let row = row.min(self.buffer.lines.len() - 1);

        let col = if px > self.gutter_width {
            self.scroll_col + (px - self.gutter_width) / self.font_w
        } else {
            0
        };
        let col = col.min(self.buffer.lines[row].len());
        (row, col)
    }

    fn word_at(&self, row: usize, col: usize) -> (usize, usize) {
        let line = &self.buffer.lines[row];
        let bytes = line.as_bytes();
        let len = bytes.len();
        if col >= len {
            return (len, len);
        }

        let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        let mut start = col;
        let mut end = col;

        if is_word(bytes[col]) {
            while start > 0 && is_word(bytes[start - 1]) {
                start -= 1;
            }
            while end < len && is_word(bytes[end]) {
                end += 1;
            }
        } else {
            end = col + 1;
        }
        (start, end)
    }
}

// --- Rendering ---

fn render(
    fb: &Framebuffer,
    font: &Font,
    editor: &Editor,
    cursor_visible: bool,
) {
    let win_w = fb.width();
    let win_h = fb.height();
    let fw = editor.font_w;
    let fh = editor.font_h;

    fb.clear(BG);

    let top = editor.text_area_top();
    let vis = editor.visible_lines(win_h);
    let sel = editor.selection_ordered();

    // Precompute syntax highlighting for visible lines
    let start_row = editor.scroll_row;
    let end_row = (start_row + vis).min(editor.buffer.lines.len());

    // Find block comment state at start_row by scanning from top
    let mut in_block_comment = false;
    for row in 0..start_row {
        let (_, new_state) = highlight_line(&editor.buffer.lines[row], in_block_comment);
        in_block_comment = new_state;
    }

    let mut highlights: Vec<Vec<(usize, TokenKind)>> = Vec::with_capacity(vis);
    for row in start_row..end_row {
        let (spans, new_state) = highlight_line(&editor.buffer.lines[row], in_block_comment);
        in_block_comment = new_state;
        highlights.push(spans);
    }

    // Draw lines
    for (vi, row) in (start_row..end_row).enumerate() {
        let y = top + vi * fh;
        let line = &editor.buffer.lines[row];

        // Current line highlight
        if row == editor.cursor_row {
            fb.fill_rect(editor.gutter_width, y, win_w - editor.gutter_width, fh, CURLINE_BG);
        }

        // Selection highlight
        if let Some(((sr, sc), (er, ec))) = sel {
            if row >= sr && row <= er {
                let sel_start = if row == sr { sc } else { 0 };
                let sel_end = if row == er { ec } else { line.len() + 1 };
                let x1 = editor.gutter_width
                    + sel_start.saturating_sub(editor.scroll_col) * fw;
                let x2 = editor.gutter_width
                    + sel_end.saturating_sub(editor.scroll_col) * fw;
                if x2 > x1 {
                    fb.fill_rect(x1, y, (x2 - x1).min(win_w - x1), fh, SELECTION_BG);
                }
            }
        }

        // Find match highlights
        if editor.finder.active {
            for (mi, &(mr, mc, ml)) in editor.finder.matches.iter().enumerate() {
                if mr == row {
                    let x = editor.gutter_width + mc.saturating_sub(editor.scroll_col) * fw;
                    let w = ml * fw;
                    let bg = if mi == editor.finder.current_match {
                        MATCH_BG
                    } else {
                        Color {
                            r: 0x58,
                            g: 0x5b,
                            b: 0x70,
                        }
                    };
                    fb.fill_rect(x, y, w.min(win_w.saturating_sub(x)), fh, bg);
                }
            }
        }

        // Gutter
        fb.fill_rect(0, y, editor.gutter_width, fh, GUTTER_BG);
        let num = format!("{}", row + 1);
        let gutter_x = editor.gutter_width - (num.len() + 1) * fw;
        font.draw_string(fb, gutter_x, y, &num, GUTTER_FG, GUTTER_BG);

        // Text with syntax highlighting
        let spans = &highlights[vi];
        if spans.is_empty() {
            continue;
        }

        let bytes = line.as_bytes();
        let line_len = bytes.len();

        for (si, &(span_start, kind)) in spans.iter().enumerate() {
            let span_end = if si + 1 < spans.len() {
                spans[si + 1].0
            } else {
                line_len
            };

            let fg = if editor.finder.active {
                // Check if this char is in the current match
                token_color(kind)
            } else {
                token_color(kind)
            };

            for ci in span_start..span_end {
                if ci < editor.scroll_col {
                    continue;
                }
                let screen_col = ci - editor.scroll_col;
                let x = editor.gutter_width + screen_col * fw;
                if x + fw > win_w {
                    break;
                }

                let ch = bytes[ci] as char;

                // Determine background for this character
                let char_bg = if editor.finder.active
                    && editor.finder.matches.iter().enumerate().any(|(mi, &(mr, mc, ml))| {
                        mr == row && ci >= mc && ci < mc + ml && mi == editor.finder.current_match
                    })
                {
                    // Current match
                    font.draw_char(fb, x, y, ch, MATCH_FG, MATCH_BG);
                    continue;
                } else if let Some(((sr, sc), (er, ec))) = sel {
                    if row >= sr
                        && row <= er
                        && ci >= (if row == sr { sc } else { 0 })
                        && ci < (if row == er { ec } else { line_len + 1 })
                    {
                        SELECTION_BG
                    } else if row == editor.cursor_row {
                        CURLINE_BG
                    } else {
                        BG
                    }
                } else if row == editor.cursor_row {
                    CURLINE_BG
                } else {
                    BG
                };

                font.draw_char(fb, x, y, ch, fg, char_bg);
            }
        }
    }

    // Fill gutter below text
    let text_bottom = top + (end_row - start_row) * fh;
    if text_bottom < win_h.saturating_sub(editor.status_height) {
        fb.fill_rect(0, text_bottom, editor.gutter_width,
            win_h - editor.status_height - text_bottom, GUTTER_BG);
    }

    // Cursor
    if cursor_visible {
        let cy = top + (editor.cursor_row.saturating_sub(editor.scroll_row)) * fh;
        let cx = editor.gutter_width
            + editor.cursor_col.saturating_sub(editor.scroll_col) * fw;
        if cy >= top && cy + fh <= win_h.saturating_sub(editor.status_height) && cx < win_w {
            // Thin line cursor (2px wide)
            fb.fill_rect(cx, cy, 2, fh, CURSOR_COLOR);
        }
    }

    // Status bar
    let status_y = win_h - editor.status_height;
    fb.fill_rect(0, status_y, win_w, editor.status_height, STATUS_BG);
    let filename = editor
        .buffer
        .path
        .as_deref()
        .unwrap_or("[untitled]");
    let dirty_mark = if editor.buffer.dirty { " [modified]" } else { "" };
    let left = format!(" {}{}", filename, dirty_mark);
    let right = format!(
        "Ln {}, Col {}  ",
        editor.cursor_row + 1,
        editor.cursor_col + 1
    );
    font.draw_string(fb, 4, status_y + 2, &left, STATUS_FG, STATUS_BG);
    let right_x = win_w.saturating_sub(right.len() * fw + 4);
    font.draw_string(fb, right_x, status_y + 2, &right, STATUS_FG, STATUS_BG);

    // Find bar
    if editor.finder.active {
        fb.fill_rect(0, 0, win_w, editor.findbar_height, FINDBAR_BG);

        let label = if editor.finder.mode == FindMode::Replace {
            "Replace:"
        } else {
            "Find:"
        };
        font.draw_string(fb, 4, 4, label, FINDBAR_LABEL, FINDBAR_BG);

        let input_x = (label.len() + 1) * fw + 4;
        let input_w = if editor.finder.mode == FindMode::Replace {
            (win_w - input_x) / 2 - fw
        } else {
            win_w - input_x - 4
        };

        // Find input
        fb.fill_rect(input_x, 2, input_w, fh + 4, FINDBAR_INPUT_BG);
        font.draw_string(
            fb,
            input_x + 2,
            4,
            &editor.finder.query,
            TEXT_FG,
            FINDBAR_INPUT_BG,
        );
        if !editor.finder.editing_replace {
            let cx = input_x + 2 + editor.finder.query_cursor * fw;
            fb.fill_rect(cx, 4, 2, fh, CURSOR_COLOR);
        }

        if editor.finder.mode == FindMode::Replace {
            let rep_x = input_x + input_w + fw;
            let rep_w = win_w - rep_x - 4;
            fb.fill_rect(rep_x, 2, rep_w, fh + 4, FINDBAR_INPUT_BG);
            font.draw_string(
                fb,
                rep_x + 2,
                4,
                &editor.finder.replace,
                TEXT_FG,
                FINDBAR_INPUT_BG,
            );
            if editor.finder.editing_replace {
                let cx = rep_x + 2 + editor.finder.replace_cursor * fw;
                fb.fill_rect(cx, 4, 2, fh, CURSOR_COLOR);
            }
        }

        // Match count
        if !editor.finder.matches.is_empty() {
            let info = format!(
                "{}/{}",
                editor.finder.current_match + 1,
                editor.finder.matches.len()
            );
            let info_x = win_w.saturating_sub(info.len() * fw + 8);
            font.draw_string(fb, info_x, 4, &info, FINDBAR_LABEL, FINDBAR_BG);
        }
    }

    // Go-to-line dialog
    if editor.goto_line.active {
        let dlg_w = 30 * fw;
        let dlg_h = fh + 8;
        let dlg_x = (win_w.saturating_sub(dlg_w)) / 2;
        let dlg_y = (win_h.saturating_sub(dlg_h)) / 2;
        fb.fill_rect(dlg_x, dlg_y, dlg_w, dlg_h, FINDBAR_BG);
        let label = "Go to line: ";
        font.draw_string(fb, dlg_x + 4, dlg_y + 4, label, FINDBAR_LABEL, FINDBAR_BG);
        let ix = dlg_x + 4 + label.len() * fw;
        fb.fill_rect(ix, dlg_y + 2, dlg_w - label.len() * fw - 8, fh + 4, FINDBAR_INPUT_BG);
        font.draw_string(fb, ix + 2, dlg_y + 4, &editor.goto_line.input, TEXT_FG, FINDBAR_INPUT_BG);
        let cx = ix + 2 + editor.goto_line.input.len() * fw;
        fb.fill_rect(cx, dlg_y + 4, 2, fh, CURSOR_COLOR);
    }
}

// --- Main ---

fn main() {
    let args: Vec<String> = env::args().collect();
    let buffer = if args.len() > 1 {
        Buffer::from_file(&args[1])
    } else {
        Buffer::new()
    };

    let title = match &buffer.path {
        Some(p) => format!("editor - {}", p.rsplit('/').next().unwrap_or(p)),
        None => "editor".to_string(),
    };

    let mut window = Window::create_with_title(0, 0, &title);
    let mut fb = window.framebuffer();

    let font_data = fs::read("/share/fonts/JetBrainsMono-Regular-8x16.font")
        .expect("Failed to load font");
    let font = Font::from_prebuilt(&font_data);

    let mut editor = Editor::new(buffer, font.width(), font.height());

    let mut cursor_visible = true;
    let mut last_blink = Instant::now();
    let blink_ms: u64 = 530;

    // Last click tracking for double-click
    let mut last_click_time = Instant::now();
    let mut last_click_pos = (0usize, 0usize);
    let mut dragging = false;

    let mut prev_cursor_visible = false;
    render(&fb, &font, &editor, cursor_visible);
    window.present();

    loop {
        // Calculate timeout: sleep until next blink toggle
        let since_blink_ms = last_blink.elapsed().as_millis() as u64;
        let until_blink_ms = blink_ms.saturating_sub(since_blink_ms);
        let timeout_ns = until_blink_ms * 1_000_000;

        let event = window.poll_event(timeout_ns.max(1));

        let mut needs_redraw = false;

        // Cursor blink
        if last_blink.elapsed().as_millis() as u64 >= blink_ms {
            cursor_visible = !cursor_visible;
            last_blink = Instant::now();
        }
        if cursor_visible != prev_cursor_visible {
            needs_redraw = true;
            prev_cursor_visible = cursor_visible;
        }

        match event {
            Some(Event::Close) => break,

            Some(Event::Resized) => {
                fb = window.framebuffer();
                editor.ensure_cursor_visible(fb.height(), fb.width());
                needs_redraw = true;
            }

            Some(Event::ClipboardPaste(data)) => {
                if let Ok(text) = std::str::from_utf8(&data) {
                    if editor.anchor.is_some() {
                        editor.delete_selection();
                    }
                    let (r, c) = editor.buffer.apply_edit(Edit::Insert {
                        row: editor.cursor_row,
                        col: editor.cursor_col,
                        text: text.to_string(),
                    });
                    editor.cursor_row = r;
                    editor.cursor_col = c;
                    editor.desired_col = editor.cursor_col;
                    editor.update_gutter();
                }
                reset_blink(&mut cursor_visible, &mut last_blink);
                needs_redraw = true;
            }

            Some(Event::KeyInput(key)) => {
                if key.pressed() {
                    handle_key(&mut editor, &key, &mut fb);
                    reset_blink(&mut cursor_visible, &mut last_blink);
                    editor.ensure_cursor_visible(fb.height(), fb.width());
                    needs_redraw = true;
                }
            }

            Some(Event::MouseInput(mouse)) => {
                handle_mouse(
                    &mut editor,
                    &mouse,
                    &mut last_click_time,
                    &mut last_click_pos,
                    &mut dragging,
                );
                reset_blink(&mut cursor_visible, &mut last_blink);
                editor.ensure_cursor_visible(fb.height(), fb.width());
                needs_redraw = true;
            }

            _ => {}
        }

        if needs_redraw {
            prev_cursor_visible = cursor_visible;
            render(&fb, &font, &editor, cursor_visible);
            window.present();
        }
    }
}

fn reset_blink(visible: &mut bool, last: &mut Instant) {
    *visible = true;
    *last = Instant::now();
}

fn handle_key(editor: &mut Editor, key: &KeyEvent, fb: &mut Framebuffer) {
    // Go-to-line dialog
    if editor.goto_line.active {
        handle_goto_line_key(editor, key);
        return;
    }

    // Find bar
    if editor.finder.active {
        handle_find_key(editor, key);
        return;
    }

    let cmd = key.gui();
    let shift = key.shift();

    // Get the translated character (lowercase) for layout-independent matching
    let ch = if key.len > 0 {
        let s = std::str::from_utf8(&key.translated[..key.len as usize]).unwrap_or("");
        s.chars().next().map(|c| c.to_ascii_lowercase())
    } else {
        None
    };

    if cmd {
        match ch {
            Some('s') => {
                if editor.buffer.path.is_none() {
                    if let Some(path) = filepicker_api::pick_file(PickerMode::Save, "/") {
                        editor.buffer.path = Some(path);
                    }
                }
                if editor.buffer.path.is_some() {
                    if let Err(e) = editor.buffer.save() {
                        eprintln!("Save error: {}", e);
                    }
                }
            }
            Some('o') => {
                if let Some(path) = filepicker_api::pick_file(PickerMode::Open, "/") {
                    editor.buffer = Buffer::from_file(&path);
                    editor.cursor_row = 0;
                    editor.cursor_col = 0;
                    editor.desired_col = 0;
                    editor.anchor = None;
                    editor.scroll_row = 0;
                    editor.scroll_col = 0;
                    editor.update_gutter();
                }
            }
            Some('z') => {
                if let Some((r, c)) = editor.buffer.undo() {
                    editor.cursor_row = r;
                    editor.cursor_col = c;
                    editor.desired_col = c;
                    editor.anchor = None;
                    editor.update_gutter();
                }
            }
            Some('y') => {
                if let Some((r, c)) = editor.buffer.redo() {
                    editor.cursor_row = r;
                    editor.cursor_col = c;
                    editor.desired_col = c;
                    editor.anchor = None;
                    editor.update_gutter();
                }
            }
            Some('c') => {
                if let Some(text) = editor.selected_text() {
                    window::clipboard_set(&text);
                }
            }
            Some('x') => {
                if let Some(text) = editor.selected_text() {
                    window::clipboard_set(&text);
                    editor.delete_selection();
                }
            }
            Some('v') => {} // Paste handled via ClipboardPaste event
            Some('a') => editor.select_all(),
            Some('q') => {
                if editor.buffer.dirty {
                    let _ = editor.buffer.save();
                }
                std::process::exit(0);
            }
            Some('f') => {
                editor.finder.active = true;
                editor.finder.mode = FindMode::Find;
                editor.finder.editing_replace = false;
                if let Some(text) = editor.selected_text() {
                    editor.finder.query = text;
                    editor.finder.query_cursor = editor.finder.query.len();
                }
                editor.finder.search(&editor.buffer.lines);
                editor.finder.find_nearest(editor.cursor_row, editor.cursor_col);
            }
            Some('h') => {
                editor.finder.active = true;
                editor.finder.mode = FindMode::Replace;
                editor.finder.editing_replace = false;
                if let Some(text) = editor.selected_text() {
                    editor.finder.query = text;
                    editor.finder.query_cursor = editor.finder.query.len();
                }
                editor.finder.search(&editor.buffer.lines);
                editor.finder.find_nearest(editor.cursor_row, editor.cursor_col);
            }
            Some('g') => {
                editor.goto_line.active = true;
                editor.goto_line.input.clear();
            }
            Some('d') => editor.duplicate_line(),
            _ => {
                // Cmd + non-letter keys (arrows, home, end, backspace)
                match key.keycode {
                    KEY_HOME => editor.goto_file_start(),
                    KEY_END => editor.goto_file_end(),
                    KEY_LEFT => editor.move_word_left(shift),
                    KEY_RIGHT => editor.move_word_right(shift),
                    KEY_BACKSPACE => editor.delete_word_backward(),
                    _ => {}
                }
            }
        }
        return;
    }

    match key.keycode {
        KEY_UP => editor.move_cursor(-1, 0, shift),
        KEY_DOWN => editor.move_cursor(1, 0, shift),
        KEY_LEFT => editor.move_cursor(0, -1, shift),
        KEY_RIGHT => editor.move_cursor(0, 1, shift),
        KEY_HOME => editor.home(shift),
        KEY_END => editor.end(shift),
        KEY_PAGEUP => editor.page_up(fb.height()),
        KEY_PAGEDOWN => editor.page_down(fb.height()),
        KEY_BACKSPACE => editor.backspace(),
        KEY_DELETE => editor.delete_forward(),
        KEY_ENTER => editor.insert_newline(),
        KEY_TAB => {
            if shift {
                editor.dedent();
            } else {
                editor.insert_tab();
            }
        }
        KEY_ESCAPE => {
            editor.anchor = None;
        }
        _ => {
            // Printable character
            if key.len > 0 {
                let text = std::str::from_utf8(&key.translated[..key.len as usize]).unwrap_or("");
                for ch in text.chars() {
                    if ch >= ' ' || ch == '\t' {
                        editor.insert_char(ch);
                    }
                }
            }
        }
    }
}

fn handle_find_key(editor: &mut Editor, key: &KeyEvent) {
    let ctrl = key.gui();
    let shift = key.shift();

    match key.keycode {
        KEY_ESCAPE => {
            editor.finder.active = false;
        }
        KEY_ENTER => {
            if ctrl && editor.finder.mode == FindMode::Replace {
                if shift {
                    editor.replace_all();
                } else {
                    editor.replace_current();
                }
            } else if shift {
                editor.finder.find_prev();
                if !editor.finder.matches.is_empty() {
                    let (r, c, _) = editor.finder.matches[editor.finder.current_match];
                    editor.cursor_row = r;
                    editor.cursor_col = c;
                    editor.desired_col = c;
                }
            } else {
                editor.finder.find_next();
                if !editor.finder.matches.is_empty() {
                    let (r, c, _) = editor.finder.matches[editor.finder.current_match];
                    editor.cursor_row = r;
                    editor.cursor_col = c;
                    editor.desired_col = c;
                }
            }
        }
        KEY_TAB => {
            if editor.finder.mode == FindMode::Replace {
                editor.finder.editing_replace = !editor.finder.editing_replace;
            }
        }
        KEY_BACKSPACE => {
            if editor.finder.editing_replace {
                if editor.finder.replace_cursor > 0 {
                    editor.finder.replace_cursor -= 1;
                    editor.finder.replace.remove(editor.finder.replace_cursor);
                }
            } else if editor.finder.query_cursor > 0 {
                editor.finder.query_cursor -= 1;
                editor.finder.query.remove(editor.finder.query_cursor);
                editor.finder.search(&editor.buffer.lines);
                editor.finder.find_nearest(editor.cursor_row, editor.cursor_col);
            }
        }
        KEY_LEFT => {
            if editor.finder.editing_replace {
                editor.finder.replace_cursor = editor.finder.replace_cursor.saturating_sub(1);
            } else {
                editor.finder.query_cursor = editor.finder.query_cursor.saturating_sub(1);
            }
        }
        KEY_RIGHT => {
            if editor.finder.editing_replace {
                editor.finder.replace_cursor =
                    (editor.finder.replace_cursor + 1).min(editor.finder.replace.len());
            } else {
                editor.finder.query_cursor =
                    (editor.finder.query_cursor + 1).min(editor.finder.query.len());
            }
        }
        _ => {
            if key.len > 0 {
                let text =
                    std::str::from_utf8(&key.translated[..key.len as usize]).unwrap_or("");
                for ch in text.chars() {
                    if ch >= ' ' {
                        if editor.finder.editing_replace {
                            editor.finder.replace.insert(editor.finder.replace_cursor, ch);
                            editor.finder.replace_cursor += 1;
                        } else {
                            editor.finder.query.insert(editor.finder.query_cursor, ch);
                            editor.finder.query_cursor += 1;
                            editor.finder.search(&editor.buffer.lines);
                            editor.finder.find_nearest(editor.cursor_row, editor.cursor_col);
                        }
                    }
                }
            }
        }
    }
}

fn handle_goto_line_key(editor: &mut Editor, key: &KeyEvent) {
    match key.keycode {
        KEY_ESCAPE => {
            editor.goto_line.active = false;
        }
        KEY_ENTER => {
            if let Ok(line_num) = editor.goto_line.input.parse::<usize>() {
                let row = line_num.saturating_sub(1).min(editor.buffer.lines.len() - 1);
                editor.cursor_row = row;
                editor.cursor_col = 0;
                editor.desired_col = 0;
                editor.anchor = None;
            }
            editor.goto_line.active = false;
        }
        KEY_BACKSPACE => {
            editor.goto_line.input.pop();
        }
        _ => {
            if key.len > 0 {
                let text =
                    std::str::from_utf8(&key.translated[..key.len as usize]).unwrap_or("");
                for ch in text.chars() {
                    if ch.is_ascii_digit() {
                        editor.goto_line.input.push(ch);
                    }
                }
            }
        }
    }
}

fn handle_mouse(
    editor: &mut Editor,
    mouse: &MouseEvent,
    last_click_time: &mut Instant,
    last_click_pos: &mut (usize, usize),
    dragging: &mut bool,
) {
    let px = mouse.x as usize;
    let py = mouse.y as usize;

    match mouse.event_type {
        window::MOUSE_PRESS if mouse.changed == 1 => {
            let (row, col) = editor.pixel_to_pos(px, py);

            // Double-click detection
            let now = Instant::now();
            if now.duration_since(*last_click_time).as_millis() < 400
                && *last_click_pos == (row, col)
            {
                // Double-click: select word
                let (ws, we) = editor.word_at(row, col);
                editor.anchor = Some((row, ws));
                editor.cursor_row = row;
                editor.cursor_col = we;
                editor.desired_col = we;
                *dragging = false;
            } else if px < editor.gutter_width {
                // Click in gutter: select line
                editor.anchor = Some((row, 0));
                if row + 1 < editor.buffer.lines.len() {
                    editor.cursor_row = row + 1;
                    editor.cursor_col = 0;
                } else {
                    editor.cursor_row = row;
                    editor.cursor_col = editor.buffer.lines[row].len();
                }
                editor.desired_col = 0;
                *dragging = true;
            } else {
                editor.cursor_row = row;
                editor.cursor_col = col;
                editor.desired_col = col;
                editor.anchor = None;
                *dragging = true;
            }

            *last_click_time = now;
            *last_click_pos = (row, col);
        }

        window::MOUSE_MOVE if *dragging && mouse.buttons & 1 != 0 => {
            let (row, col) = editor.pixel_to_pos(px, py);
            if editor.anchor.is_none() {
                editor.anchor = Some((editor.cursor_row, editor.cursor_col));
            }
            editor.cursor_row = row;
            editor.cursor_col = col;
            editor.desired_col = col;
        }

        window::MOUSE_RELEASE if mouse.changed == 1 => {
            *dragging = false;
        }

        window::MOUSE_SCROLL => {
            let scroll_lines = 3usize;
            if mouse.scroll < 0 {
                editor.scroll_row = editor.scroll_row.saturating_sub(scroll_lines);
            } else {
                editor.scroll_row = (editor.scroll_row + scroll_lines)
                    .min(editor.buffer.lines.len().saturating_sub(1));
            }
        }

        _ => {}
    }
}

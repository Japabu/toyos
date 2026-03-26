use filepicker_api::{PickerMode, MSG_FILEPICKER_REQUEST, MSG_FILEPICKER_RESULT};
use font::Font;
use std::fs;
use toyos_abi::ipc;
use toyos_abi::services;
use toyos_abi::syscall;
use toyos_abi::Fd;
use std::path::{Path, PathBuf};
use window::{Color, Event, Framebuffer, KeyEvent, MouseEvent, Window};

// --- Colors (matching editor theme) ---

const BG: Color = Color { r: 0x1e, g: 0x1e, b: 0x2e };
const TEXT_FG: Color = Color { r: 0xcd, g: 0xd6, b: 0xf4 };
const DIM_FG: Color = Color { r: 0x6c, g: 0x70, b: 0x86 };
const DIR_FG: Color = Color { r: 0x89, g: 0xb4, b: 0xfa };
const SEL_BG: Color = Color { r: 0x45, g: 0x47, b: 0x5a };
const PATH_BG: Color = Color { r: 0x31, g: 0x32, b: 0x44 };
const INPUT_BG: Color = Color { r: 0x11, g: 0x11, b: 0x1b };
const BUTTON_BG: Color = Color { r: 0x45, g: 0x47, b: 0x5a };
const BUTTON_FG: Color = Color { r: 0xcd, g: 0xd6, b: 0xf4 };
const ACCENT_BG: Color = Color { r: 0x89, g: 0xb4, b: 0xfa };
const ACCENT_FG: Color = Color { r: 0x1e, g: 0x1e, b: 0x2e };
const CURSOR_COLOR: Color = Color { r: 0xf5, g: 0xe0, b: 0xdc };

// --- HID keycodes ---

const KEY_UP: u8 = 0x52;
const KEY_DOWN: u8 = 0x51;
const KEY_LEFT: u8 = 0x50;
const KEY_RIGHT: u8 = 0x4F;
const KEY_BACKSPACE: u8 = 0x2A;
const KEY_ENTER: u8 = 0x28;
const KEY_TAB: u8 = 0x2B;
const KEY_ESCAPE: u8 = 0x29;

// --- Directory entry ---

struct Entry {
    name: String,
    is_dir: bool,
}

fn list_dir(path: &Path) -> Vec<Entry> {
    let mut entries = Vec::new();

    if let Ok(read_dir) = fs::read_dir(path) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().map_or(false, |ft| ft.is_dir());
            entries.push(Entry { name, is_dir });
        }
    }

    // Sort: directories first, then alphabetical
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    entries
}

// --- Picker state ---

struct Picker {
    mode: PickerMode,
    current_dir: PathBuf,
    entries: Vec<Entry>,
    selected: usize,
    scroll: usize,
    filename: String,
    filename_cursor: usize,
    focus_filename: bool, // true = filename input focused, false = file list focused
    font_w: usize,
    font_h: usize,
}

impl Picker {
    fn new(mode: PickerMode, start_dir: &str, font_w: usize, font_h: usize) -> Self {
        let current_dir = PathBuf::from(if start_dir.is_empty() { "/" } else { start_dir });
        let entries = list_dir(&current_dir);
        Self {
            mode,
            current_dir,
            entries,
            selected: 0,
            scroll: 0,
            filename: String::new(),
            filename_cursor: 0,
            focus_filename: mode == PickerMode::Save,
            font_w,
            font_h,
        }
    }

    fn refresh(&mut self) {
        self.entries = list_dir(&self.current_dir);
        self.selected = 0;
        self.scroll = 0;
    }

    fn navigate_into(&mut self, dir_name: &str) {
        if dir_name == ".." {
            if let Some(parent) = self.current_dir.parent() {
                self.current_dir = parent.to_path_buf();
            }
        } else {
            self.current_dir.push(dir_name);
        }
        self.refresh();
    }

    fn visible_rows(&self, win_h: usize) -> usize {
        let top = self.font_h + 8; // path bar
        let bottom = if self.mode == PickerMode::Save {
            (self.font_h + 8) * 2 // filename input + action bar
        } else {
            self.font_h + 8 // action bar only
        };
        (win_h.saturating_sub(top + bottom)) / self.font_h
    }

    fn ensure_visible(&mut self, win_h: usize) {
        let vis = self.visible_rows(win_h);
        if vis == 0 {
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + vis {
            self.scroll = self.selected - vis + 1;
        }
    }

    /// Returns the full path of the result, or None to cancel.
    fn activate(&self) -> PickerAction {
        if self.focus_filename && self.mode == PickerMode::Save {
            if self.filename.is_empty() {
                return PickerAction::None;
            }
            let path = self.current_dir.join(&self.filename);
            return PickerAction::Pick(path.to_string_lossy().into_owned());
        }

        if self.entries.is_empty() {
            return PickerAction::None;
        }

        let entry = &self.entries[self.selected];
        if entry.is_dir {
            PickerAction::EnterDir(entry.name.clone())
        } else {
            let path = self.current_dir.join(&entry.name);
            if self.mode == PickerMode::Save {
                // In save mode, selecting a file populates the filename field
                PickerAction::SetFilename(entry.name.clone())
            } else {
                PickerAction::Pick(path.to_string_lossy().into_owned())
            }
        }
    }
}

enum PickerAction {
    None,
    Pick(String),
    EnterDir(String),
    SetFilename(String),
}

// --- Rendering ---

fn render(fb: &Framebuffer, font: &Font, picker: &Picker) {
    let w = fb.width();
    let h = fb.height();
    let fw = picker.font_w;
    let fh = picker.font_h;

    fb.clear(BG);

    // Path bar
    let path_str = picker.current_dir.to_string_lossy();
    fb.fill_rect(0, 0, w, fh + 8, PATH_BG);
    font.draw_string(fb, 8, 4, &path_str, TEXT_FG, PATH_BG);

    // File list
    let list_y = fh + 8;
    let vis = picker.visible_rows(h);

    for i in 0..vis {
        let idx = picker.scroll + i;
        if idx >= picker.entries.len() {
            break;
        }
        let entry = &picker.entries[idx];
        let y = list_y + i * fh;

        let (bg, fg) = if idx == picker.selected && !picker.focus_filename {
            (SEL_BG, TEXT_FG)
        } else if entry.is_dir {
            (BG, DIR_FG)
        } else {
            (BG, TEXT_FG)
        };

        if idx == picker.selected && !picker.focus_filename {
            fb.fill_rect(0, y, w, fh, SEL_BG);
        }

        let display = if entry.is_dir {
            format!("  {}/", entry.name)
        } else {
            format!("  {}", entry.name)
        };
        font.draw_string(fb, 8, y, &display, fg, bg);
    }

    // Bottom area
    let mut bottom_y = h;

    // Action bar (always at very bottom)
    bottom_y -= fh + 8;
    let action_y = bottom_y;
    fb.fill_rect(0, action_y, w, fh + 8, PATH_BG);

    let cancel_label = " Cancel ";
    let action_label = if picker.mode == PickerMode::Save {
        " Save "
    } else {
        " Open "
    };

    let action_w = action_label.len() * fw;
    let cancel_w = cancel_label.len() * fw;
    let action_x = w - action_w - 8;
    let cancel_x = action_x - cancel_w - 8;

    // Cancel button
    fb.fill_rect(cancel_x, action_y + 2, cancel_w, fh + 4, BUTTON_BG);
    font.draw_string(fb, cancel_x, action_y + 4, cancel_label, BUTTON_FG, BUTTON_BG);

    // Action button
    fb.fill_rect(action_x, action_y + 2, action_w, fh + 4, ACCENT_BG);
    font.draw_string(fb, action_x, action_y + 4, action_label, ACCENT_FG, ACCENT_BG);

    // Filename input (Save mode only)
    if picker.mode == PickerMode::Save {
        bottom_y -= fh + 8;
        let input_y = bottom_y;
        fb.fill_rect(0, input_y, w, fh + 8, PATH_BG);

        let label = "Filename: ";
        font.draw_string(fb, 8, input_y + 4, label, DIM_FG, PATH_BG);

        let input_x = 8 + label.len() * fw;
        let input_w = w - input_x - 8;
        fb.fill_rect(input_x, input_y + 2, input_w, fh + 4, INPUT_BG);
        font.draw_string(fb, input_x + 4, input_y + 4, &picker.filename, TEXT_FG, INPUT_BG);

        if picker.focus_filename {
            let cx = input_x + 4 + picker.filename_cursor * fw;
            fb.fill_rect(cx, input_y + 4, 2, fh, CURSOR_COLOR);
        }
    }
}

// --- Event handling ---

enum PickerResult {
    Continue,
    Pick(String),
    Cancel,
}

fn handle_key(picker: &mut Picker, key: &KeyEvent, win_h: usize) -> PickerResult {
    if !key.pressed() {
        return PickerResult::Continue;
    }

    match key.keycode {
        KEY_ESCAPE => return PickerResult::Cancel,

        KEY_TAB if picker.mode == PickerMode::Save => {
            picker.focus_filename = !picker.focus_filename;
        }

        KEY_ENTER => {
            match picker.activate() {
                PickerAction::Pick(path) => return PickerResult::Pick(path),
                PickerAction::EnterDir(name) => {
                    picker.navigate_into(&name);
                }
                PickerAction::SetFilename(name) => {
                    picker.filename = name;
                    picker.filename_cursor = picker.filename.len();
                    picker.focus_filename = true;
                }
                PickerAction::None => {}
            }
        }

        KEY_BACKSPACE => {
            if picker.focus_filename {
                if picker.filename_cursor > 0 {
                    picker.filename_cursor -= 1;
                    picker.filename.remove(picker.filename_cursor);
                }
            } else {
                // Go up a directory
                picker.navigate_into("..");
            }
        }

        KEY_UP if !picker.focus_filename => {
            if picker.selected > 0 {
                picker.selected -= 1;
                picker.ensure_visible(win_h);
            }
        }

        KEY_DOWN if !picker.focus_filename => {
            if picker.selected + 1 < picker.entries.len() {
                picker.selected += 1;
                picker.ensure_visible(win_h);
            }
        }

        KEY_LEFT if picker.focus_filename => {
            picker.filename_cursor = picker.filename_cursor.saturating_sub(1);
        }

        KEY_RIGHT if picker.focus_filename => {
            picker.filename_cursor = (picker.filename_cursor + 1).min(picker.filename.len());
        }

        _ => {
            if picker.focus_filename && key.len > 0 {
                let text =
                    std::str::from_utf8(&key.translated[..key.len as usize]).unwrap_or("");
                for ch in text.chars() {
                    if ch >= ' ' && ch != '/' {
                        picker.filename.insert(picker.filename_cursor, ch);
                        picker.filename_cursor += 1;
                    }
                }
            }
        }
    }

    PickerResult::Continue
}

fn handle_mouse(picker: &mut Picker, mouse: &MouseEvent, win_h: usize) -> PickerResult {
    let fh = picker.font_h;
    let py = mouse.y as usize;

    match mouse.event_type {
        window::MOUSE_PRESS if mouse.changed == 1 => {
            let list_y = fh + 8;
            let vis = picker.visible_rows(win_h);
            let list_end = list_y + vis * fh;

            if py >= list_y && py < list_end {
                let idx = picker.scroll + (py - list_y) / fh;
                if idx < picker.entries.len() {
                    picker.selected = idx;
                    picker.focus_filename = false;
                }
            }

            // Check filename input area click (Save mode)
            if picker.mode == PickerMode::Save {
                let input_y = win_h - (fh + 8) * 2;
                if py >= input_y && py < input_y + fh + 8 {
                    picker.focus_filename = true;
                }
            }
        }

        window::MOUSE_SCROLL => {
            if mouse.scroll < 0 {
                picker.scroll = picker.scroll.saturating_sub(3);
            } else {
                let max_scroll = picker.entries.len().saturating_sub(1);
                picker.scroll = (picker.scroll + 3).min(max_scroll);
            }
        }

        _ => {}
    }

    PickerResult::Continue
}

// --- Run a single file picker session ---

fn run_picker(mode: PickerMode, start_dir: &str, client_fd: Fd) {
    let title = if mode == PickerMode::Save {
        "Save As"
    } else {
        "Open File"
    };

    let mut window = Window::create_topmost(500, 400, title);
    let mut fb = window.framebuffer();

    let font_data = fs::read("/share/fonts/JetBrainsMono-Regular-8x16.font").expect("Failed to load font");
    let font = Font::from_prebuilt(&font_data);

    let mut picker = Picker::new(mode, start_dir, font.width(), font.height());

    render(&fb, &font, &picker);
    window.present();

    loop {
        let event = window.recv_event();
        let mut needs_redraw = true;

        let result = match event {
            Event::Close => PickerResult::Cancel,

            Event::Resized => {
                fb = window.framebuffer();
                PickerResult::Continue
            }

            Event::KeyInput(key) => handle_key(&mut picker, &key, fb.height()),

            Event::MouseInput(mouse) => handle_mouse(&mut picker, &mouse, fb.height()),

            _ => {
                needs_redraw = false;
                PickerResult::Continue
            }
        };

        match result {
            PickerResult::Pick(path) => {
                let _ = ipc::send_bytes(client_fd, MSG_FILEPICKER_RESULT,path.as_bytes());
                return;
            }
            PickerResult::Cancel => {
                let _ = ipc::send_bytes(client_fd, MSG_FILEPICKER_RESULT,&[]);
                return;
            }
            PickerResult::Continue => {}
        }

        if needs_redraw {
            render(&fb, &font, &picker);
            window.present();
        }
    }
}

// --- Main daemon loop ---

fn main() {
    let listener = services::listen("filepicker").expect("filepicker: name already taken");

    loop {
        let conn = services::accept(listener).expect("accept failed");
        let client_fd = conn.fd;
        let Ok(header) = ipc::recv_header(client_fd) else {
            syscall::close(client_fd);
            continue;
        };
        if header.msg_type != MSG_FILEPICKER_REQUEST {
            syscall::close(client_fd);
            continue;
        }

        let mut data = [0u8; 4096];
        let n = ipc::recv_bytes(client_fd, &header, &mut data).unwrap_or(0);
        let mode = if n > 0 && data[0] == PickerMode::Save as u8 {
            PickerMode::Save
        } else {
            PickerMode::Open
        };
        let start_dir = if n > 1 {
            core::str::from_utf8(&data[1..n]).unwrap_or("/")
        } else {
            "/"
        };

        run_picker(mode, start_dir, client_fd);
        syscall::close(client_fd);
    }
}

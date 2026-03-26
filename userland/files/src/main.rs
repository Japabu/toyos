use std::fs;
use std::path::PathBuf;

use sprite::Sprite;
use window::{Color, Framebuffer, Window};

const BG: Color = Color { r: 0x1e, g: 0x1e, b: 0x2e };
const TEXT_COLOR: Color = Color { r: 0xe0, g: 0xe0, b: 0xe8 };
const DIM_TEXT: Color = Color { r: 0x70, g: 0x70, b: 0x80 };
const SELECTED_BG: Color = Color { r: 0x35, g: 0x35, b: 0x55 };
const PATH_BG: Color = Color { r: 0x15, g: 0x15, b: 0x22 };
const PATH_COLOR: Color = Color { r: 0x80, g: 0x80, b: 0x90 };
const SCROLLBAR_TRACK: Color = Color { r: 0x25, g: 0x25, b: 0x35 };
const SCROLLBAR_THUMB: Color = Color { r: 0x55, g: 0x55, b: 0x65 };

const ICON_SIZE: usize = 32;
const ITEM_WIDTH: usize = 80;
const ITEM_HEIGHT: usize = 64;
const PADDING: usize = 8;
const PATH_BAR_HEIGHT: usize = 24;
const STATUS_BAR_HEIGHT: usize = 20;
const SCROLLBAR_WIDTH: usize = 6;

struct Entry {
    name: String,
    is_dir: bool,
    size: u64,
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.0}K", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    }
}

struct FileBrowser {
    window: Window,
    fb: Framebuffer,
    font: font::Font,
    folder_icon: Sprite,
    file_icon: Sprite,
    entries: Vec<Entry>,
    selected: Option<usize>,
    current_dir: PathBuf,
    scroll_y: usize,
}

impl FileBrowser {
    fn new() -> Self {
        let window = Window::create_with_title(0, 0, "Files");
        let fb = window.framebuffer();

        let font_data = fs::read("/share/fonts/JetBrainsMono-Regular-8x16.font").expect("failed to read font");
        let font = font::Font::from_prebuilt(&font_data);

        let folder_svg = fs::read("/share/icons/folder-bold.svg").expect("failed to read folder icon");
        let file_svg = fs::read("/share/icons/file-bold.svg").expect("failed to read file icon");
        let folder_icon = Sprite::from_svg_colored(&folder_svg, ICON_SIZE as u32, [0xf0, 0xc8, 0x50]);
        let file_icon = Sprite::from_svg_colored(&file_svg, ICON_SIZE as u32, [0xd0, 0xd0, 0xd8]);

        let current_dir = PathBuf::from("/home/root");

        let mut browser = Self {
            window,
            fb,
            font,
            folder_icon,
            file_icon,
            entries: Vec::new(),
            selected: None,
            current_dir,
            scroll_y: 0,
        };
        browser.load_directory();
        browser.redraw();
        browser.window.present();
        browser
    }

    fn load_directory(&mut self) {
        self.entries.clear();
        self.selected = None;
        self.scroll_y = 0;

        if self.current_dir.as_os_str() != "/" {
            self.entries.push(Entry {
                name: "..".to_string(),
                is_dir: true,
                size: 0,
            });
        }

        if let Ok(read_dir) = fs::read_dir(&self.current_dir) {
            let mut items: Vec<Entry> = read_dir
                .filter_map(|e| e.ok())
                .map(|e| {
                    let is_dir = e.file_type().map_or(false, |ft| ft.is_dir());
                    let size = if is_dir { 0 } else { e.metadata().map_or(0, |m| m.len()) };
                    Entry {
                        name: e.file_name().to_string_lossy().into_owned(),
                        is_dir,
                        size,
                    }
                })
                .collect();
            items.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
            self.entries.append(&mut items);
        }
    }

    fn cols(&self) -> usize {
        let w = self.fb.width();
        if w > PADDING { (w - PADDING) / ITEM_WIDTH } else { 1 }
    }

    fn content_height(&self) -> usize {
        let cols = self.cols();
        let rows = (self.entries.len() + cols - 1) / cols;
        rows * ITEM_HEIGHT
    }

    fn visible_height(&self) -> usize {
        self.fb.height().saturating_sub(PATH_BAR_HEIGHT + PADDING + STATUS_BAR_HEIGHT)
    }

    fn max_scroll(&self) -> usize {
        self.content_height().saturating_sub(self.visible_height())
    }

    fn redraw(&self) {
        let w = self.fb.width();
        let h = self.fb.height();

        self.fb.fill_rect(0, 0, w, h, BG);

        // Path bar
        self.fb.fill_rect(0, 0, w, PATH_BAR_HEIGHT, PATH_BG);
        let path_str = self.current_dir.display().to_string();
        let max_chars = (w - 16) / self.font.width();
        let display_path: String = path_str.chars().take(max_chars).collect();
        self.font
            .draw_string(&self.fb, 8, 4, &display_path, PATH_COLOR, PATH_BG);

        let cols = self.cols();
        let content_top = PATH_BAR_HEIGHT + PADDING;
        let content_bottom = h.saturating_sub(STATUS_BAR_HEIGHT);

        for (i, entry) in self.entries.iter().enumerate() {
            let col = i % cols;
            let row = i / cols;
            let x = PADDING + col * ITEM_WIDTH;
            let y = content_top + row * ITEM_HEIGHT - self.scroll_y;

            if y + ITEM_HEIGHT <= content_top || y >= content_bottom {
                continue;
            }

            if self.selected == Some(i) {
                let clip_y = y.max(content_top);
                let clip_h = (y + ITEM_HEIGHT).min(content_bottom) - clip_y;
                self.fb.fill_rect(x, clip_y, ITEM_WIDTH, clip_h, SELECTED_BG);
            }

            if y >= content_top && y + ICON_SIZE <= content_bottom {
                let icon_x = x + (ITEM_WIDTH - ICON_SIZE) / 2;
                let icon = if entry.is_dir {
                    &self.folder_icon
                } else {
                    &self.file_icon
                };
                icon.draw(
                    self.fb.ptr(),
                    self.fb.stride(),
                    self.fb.width(),
                    self.fb.height(),
                    self.fb.pixel_format_raw(),
                    icon_x,
                    y,
                );
            }

            let text_bg = if self.selected == Some(i) { SELECTED_BG } else { BG };

            // File name
            let max_chars = ITEM_WIDTH / self.font.width();
            let name: String = entry.name.chars().take(max_chars).collect();
            let text_x = x + (ITEM_WIDTH.saturating_sub(name.len() * self.font.width())) / 2;
            let text_y = y + ICON_SIZE + 2;
            if text_y >= content_top && text_y + self.font.height() <= content_bottom {
                self.font.draw_string(&self.fb, text_x, text_y, &name, TEXT_COLOR, text_bg);
            }

            // File size (for files only)
            if !entry.is_dir && entry.name != ".." {
                let size_str = format_size(entry.size);
                let size_x = x + (ITEM_WIDTH.saturating_sub(size_str.len() * self.font.width())) / 2;
                let size_y = text_y + self.font.height();
                if size_y >= content_top && size_y + self.font.height() <= content_bottom {
                    self.font.draw_string(&self.fb, size_x, size_y, &size_str, DIM_TEXT, text_bg);
                }
            }
        }

        // Status bar
        let status_y = h - STATUS_BAR_HEIGHT;
        self.fb.fill_rect(0, status_y, w, STATUS_BAR_HEIGHT, PATH_BG);
        let file_count = self.entries.iter().filter(|e| e.name != "..").count();
        let status = format!("{} items", file_count);
        self.font.draw_string(&self.fb, 8, status_y + 2, &status, PATH_COLOR, PATH_BG);

        // Scrollbar
        if self.max_scroll() > 0 {
            let visible = self.visible_height();
            let total = self.content_height();
            let track_x = w - SCROLLBAR_WIDTH;
            let track_top = content_top;
            let track_height = content_bottom - content_top;
            self.fb.fill_rect(track_x, track_top, SCROLLBAR_WIDTH, track_height, SCROLLBAR_TRACK);

            let thumb_height = (track_height * visible / total).max(20);
            let track_range = track_height.saturating_sub(thumb_height);
            let thumb_top = if self.max_scroll() > 0 {
                track_top + self.scroll_y * track_range / self.max_scroll()
            } else {
                track_top
            };
            self.fb.fill_rect(track_x, thumb_top, SCROLLBAR_WIDTH, thumb_height, SCROLLBAR_THUMB);
        }
    }

    fn item_at(&self, mx: usize, my: usize) -> Option<usize> {
        let content_y = PATH_BAR_HEIGHT + PADDING;
        if my < content_y || my >= self.fb.height().saturating_sub(STATUS_BAR_HEIGHT) {
            return None;
        }
        let cols = self.cols();
        let col = mx.checked_sub(PADDING)? / ITEM_WIDTH;
        let row = (my - content_y + self.scroll_y) / ITEM_HEIGHT;
        if col >= cols {
            return None;
        }
        let idx = row * cols + col;
        if idx < self.entries.len() {
            Some(idx)
        } else {
            None
        }
    }

    fn open(&mut self, idx: usize) {
        let entry = &self.entries[idx];
        if entry.is_dir {
            if entry.name == ".." {
                if let Some(parent) = self.current_dir.parent() {
                    self.current_dir = parent.to_path_buf();
                }
            } else {
                self.current_dir = self.current_dir.join(&entry.name);
            }
            self.load_directory();
            self.redraw();
            self.window.present();
        }
    }

    fn ensure_visible(&mut self, idx: usize) {
        let cols = self.cols();
        let row = idx / cols;
        let item_top = row * ITEM_HEIGHT;
        let item_bottom = item_top + ITEM_HEIGHT;
        let visible = self.visible_height();

        if item_top < self.scroll_y {
            self.scroll_y = item_top;
        } else if item_bottom > self.scroll_y + visible {
            self.scroll_y = item_bottom.saturating_sub(visible);
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.entries.len();
        if len == 0 {
            return;
        }
        let new = match self.selected {
            Some(cur) => (cur as isize + delta).clamp(0, len as isize - 1) as usize,
            None => 0,
        };
        self.selected = Some(new);
        self.ensure_visible(new);
        self.redraw();
        self.window.present();
    }

    fn run(&mut self) {
        loop {
            match self.window.recv_event() {
                window::Event::MouseInput(ev) => {
                    if ev.event_type == window::MOUSE_PRESS && ev.changed == 1 {
                        if let Some(idx) = self.item_at(ev.x as usize, ev.y as usize) {
                            if self.selected == Some(idx) {
                                self.open(idx);
                            } else {
                                self.selected = Some(idx);
                                self.redraw();
                                self.window.present();
                            }
                        }
                    } else if ev.event_type == window::MOUSE_SCROLL {
                        let step = ITEM_HEIGHT;
                        if ev.scroll < 0 {
                            self.scroll_y = (self.scroll_y + step).min(self.max_scroll());
                        } else if ev.scroll > 0 {
                            self.scroll_y = self.scroll_y.saturating_sub(step);
                        }
                        self.redraw();
                        self.window.present();
                    }
                }
                window::Event::KeyInput(ev) => {
                    let cols = self.cols() as isize;
                    match ev.keycode {
                        0x51 => self.move_selection(cols),         // Down arrow
                        0x52 => self.move_selection(-cols),        // Up arrow
                        0x4F => self.move_selection(1),            // Right arrow
                        0x50 => self.move_selection(-1),           // Left arrow
                        0x2A => {
                            // Backspace: go up
                            if self.current_dir.as_os_str() != "/" {
                                if let Some(parent) = self.current_dir.parent() {
                                    self.current_dir = parent.to_path_buf();
                                }
                                self.load_directory();
                                self.redraw();
                                self.window.present();
                            }
                        }
                        0x28 => {
                            // Enter: open selected
                            if let Some(idx) = self.selected {
                                self.open(idx);
                            }
                        }
                        _ => {}
                    }
                }
                window::Event::Resized => {
                    self.fb = self.window.framebuffer();
                    self.scroll_y = self.scroll_y.min(self.max_scroll());
                    self.redraw();
                    self.window.present();
                }
                window::Event::ClipboardPaste(_) => {}
                window::Event::Frame => {}
                window::Event::Close => break,
            }
        }
    }
}

fn main() {
    let mut browser = FileBrowser::new();
    browser.run();
}

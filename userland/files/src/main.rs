mod framebuffer;

use std::fs;
use std::path::PathBuf;

use framebuffer::{Color, Framebuffer};
use sprite::Sprite;
use window::Window;

const BG: Color = Color { r: 0x1e, g: 0x1e, b: 0x2e };
const TEXT_COLOR: Color = Color { r: 0xe0, g: 0xe0, b: 0xe8 };
const SELECTED_BG: Color = Color { r: 0x35, g: 0x35, b: 0x55 };
const PATH_BG: Color = Color { r: 0x15, g: 0x15, b: 0x22 };
const PATH_COLOR: Color = Color { r: 0x80, g: 0x80, b: 0x90 };

const ICON_SIZE: usize = 32;
const ITEM_WIDTH: usize = 80;
const ITEM_HEIGHT: usize = 56;
const PADDING: usize = 8;
const PATH_BAR_HEIGHT: usize = 24;

struct Entry {
    name: String,
    is_dir: bool,
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
}

impl FileBrowser {
    fn new() -> Self {
        let window = Window::create_with_title(0, 0, "Files");
        let fb = Framebuffer::new(
            window.buffer_ptr() as u64,
            window.width(),
            window.height(),
            window.width(),
            window.pixel_format(),
        );

        let ttf_data = fs::read("/initrd/JetBrainsMono-Regular.ttf").expect("failed to read font");
        let font = font::Font::new(&ttf_data, 8, 16);

        let folder_svg = fs::read("/initrd/folder-bold.svg").expect("failed to read folder icon");
        let file_svg = fs::read("/initrd/file-bold.svg").expect("failed to read file icon");
        let folder_icon = Sprite::from_svg_colored(&folder_svg, ICON_SIZE as u32, [0xf0, 0xc8, 0x50]);
        let file_icon = Sprite::from_svg_colored(&file_svg, ICON_SIZE as u32, [0xd0, 0xd0, 0xd8]);

        let current_dir = PathBuf::from("/initrd");

        let mut browser = Self {
            window,
            fb,
            font,
            folder_icon,
            file_icon,
            entries: Vec::new(),
            selected: None,
            current_dir,
        };
        browser.load_directory();
        browser.redraw();
        browser.window.present();
        browser
    }

    fn load_directory(&mut self) {
        self.entries.clear();
        self.selected = None;

        if self.current_dir.as_os_str() != "/" {
            self.entries.push(Entry {
                name: "..".to_string(),
                is_dir: true,
            });
        }

        if let Ok(read_dir) = fs::read_dir(&self.current_dir) {
            let mut items: Vec<Entry> = read_dir
                .filter_map(|e| e.ok())
                .map(|e| Entry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    is_dir: e.file_type().map_or(false, |ft| ft.is_dir()),
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
        let content_y = PATH_BAR_HEIGHT + PADDING;

        for (i, entry) in self.entries.iter().enumerate() {
            let col = i % cols;
            let row = i / cols;
            let x = PADDING + col * ITEM_WIDTH;
            let y = content_y + row * ITEM_HEIGHT;

            if y + ITEM_HEIGHT > h {
                break;
            }

            if self.selected == Some(i) {
                self.fb.fill_rect(x, y, ITEM_WIDTH, ITEM_HEIGHT, SELECTED_BG);
            }

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

            let max_chars = ITEM_WIDTH / self.font.width();
            let name: String = entry.name.chars().take(max_chars).collect();
            let text_x = x + (ITEM_WIDTH.saturating_sub(name.len() * self.font.width())) / 2;
            let text_y = y + ICON_SIZE + 4;
            let text_bg = if self.selected == Some(i) {
                SELECTED_BG
            } else {
                BG
            };
            self.font
                .draw_string(&self.fb, text_x, text_y, &name, TEXT_COLOR, text_bg);
        }
    }

    fn item_at(&self, mx: usize, my: usize) -> Option<usize> {
        let content_y = PATH_BAR_HEIGHT + PADDING;
        if my < content_y {
            return None;
        }
        let cols = self.cols();
        let col = mx.checked_sub(PADDING)? / ITEM_WIDTH;
        let row = (my - content_y) / ITEM_HEIGHT;
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
                    self.fb = Framebuffer::new(
                        self.window.buffer_ptr() as u64,
                        self.window.width(),
                        self.window.height(),
                        self.window.width(),
                        self.window.pixel_format(),
                    );
                    self.redraw();
                    self.window.present();
                }
                window::Event::ClipboardPaste(_) => {}
                window::Event::Close => break,
            }
        }
    }
}

fn main() {
    let mut browser = FileBrowser::new();
    browser.run();
}

use std::io::{self, Read, Write};
use toyos::system::set_keyboard_layout;

const CONFIG_PATH: &str = "/home/root/.config/keyboard_layout";
const LAYOUTS: &[&str] = &["us", "swiss-german-mac"];

pub fn main(args: Vec<String>) {
    match args.first().map(|s| s.as_str()) {
        Some("--load") => load(),
        Some(name) => set(name),
        None => interactive_select(),
    }
}

fn load() {
    let data = match std::fs::read_to_string(CONFIG_PATH) {
        Ok(data) => data,
        Err(_) => return,
    };
    let name = data.trim();
    if !name.is_empty() {
        if let Err(e) = set_keyboard_layout(name) {
            eprintln!("locale: failed to set layout '{name}': {e}");
        }
    }
}

fn set(name: &str) {
    match set_keyboard_layout(name) {
        Ok(()) => {
            std::fs::write(CONFIG_PATH, name).unwrap_or_else(|e| {
                eprintln!("locale: failed to save config: {e}");
            });
            println!("Keyboard layout set to '{name}'");
        }
        Err(e) => eprintln!("locale: failed to set layout '{name}': {e}"),
    }
}

fn interactive_select() {
    let mut selected: usize = 0;
    std::os::toyos::io::set_stdin_raw(true);

    draw_menu(selected);

    loop {
        let Some(b) = read_byte() else { break };
        match b {
            0x0D => {
                clear_menu();
                set(LAYOUTS[selected]);
                break;
            }
            0x1B => {
                // Escape sequence
                let Some(b'[') = read_byte() else {
                    // Bare Esc: cancel
                    clear_menu();
                    break;
                };
                match read_byte() {
                    Some(b'A') if selected > 0 => {
                        selected -= 1;
                        draw_menu(selected);
                    }
                    Some(b'B') if selected < LAYOUTS.len() - 1 => {
                        selected += 1;
                        draw_menu(selected);
                    }
                    Some(b'3') => { read_byte(); } // Delete key (~)
                    _ => {}
                }
            }
            b'q' => {
                clear_menu();
                break;
            }
            _ => {}
        }
    }

    std::os::toyos::io::set_stdin_raw(false);
}

fn draw_menu(selected: usize) {
    let mut out = io::stdout().lock();
    write!(out, "\r").ok();
    for (i, name) in LAYOUTS.iter().enumerate() {
        if i == selected {
            write!(out, "\x1b[7m  {name}  \x1b[0m\x1b[K\r\n").ok();
        } else {
            write!(out, "  {name}  \x1b[K\r\n").ok();
        }
    }
    // Move cursor back up to top of menu
    for _ in 0..LAYOUTS.len() {
        write!(out, "\x1b[A").ok();
    }
    out.flush().ok();
}

fn clear_menu() {
    let mut out = io::stdout().lock();
    write!(out, "\r").ok();
    for _ in 0..LAYOUTS.len() {
        write!(out, "\x1b[2K\r\n").ok();
    }
    for _ in 0..LAYOUTS.len() {
        write!(out, "\x1b[A").ok();
    }
    out.flush().ok();
}

fn read_byte() -> Option<u8> {
    let mut buf = [0u8; 1];
    io::stdin().lock().read_exact(&mut buf).ok()?;
    Some(buf[0])
}

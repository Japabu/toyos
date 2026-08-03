use std::io::{self, Read, Write};
use toyos::system::set_keyboard_layout;
use toyos_keymap::detect::{Detector, Step};

const CONFIG_PATH: &str = "/home/root/.config/keyboard_layout";

/// The names come from the same table the kernel selects on, so this cannot
/// offer a layout the kernel would refuse or hide one it has. There is no way
/// to ask which is *currently* active: that needs a read syscall, and the one
/// the introspection plan reserves for it (`SYS_QUERY`) is not built.
fn layouts() -> impl Iterator<Item = &'static str> {
    toyos_keymap::LAYOUTS.iter().map(|l| l.name)
}

pub fn main(args: Vec<String>) {
    match args.first().map(|s| s.as_str()) {
        Some("--load") => load(),
        Some("--list") => {
            for name in layouts() {
                println!("{name}");
            }
        }
        Some("detect") => detect(),
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
        Err(e) => {
            let available: Vec<&str> = layouts().collect();
            eprintln!(
                "locale: failed to set layout '{name}': {e}; available: {}",
                available.join(", ")
            );
        }
    }
}

fn interactive_select() {
    let names: Vec<&str> = layouts().collect();
    let mut selected: usize = 0;
    std::os::toyos::io::set_stdin_raw(true);

    draw_menu(&names, selected);

    loop {
        let Some(b) = read_byte() else { break };
        match b {
            0x0D => {
                clear_menu(names.len());
                set(names[selected]);
                break;
            }
            0x1B => {
                // Escape sequence
                let Some(b'[') = read_byte() else {
                    // Bare Esc: cancel
                    clear_menu(names.len());
                    break;
                };
                match read_byte() {
                    Some(b'A') if selected > 0 => {
                        selected -= 1;
                        draw_menu(&names, selected);
                    }
                    Some(b'B') if selected < names.len() - 1 => {
                        selected += 1;
                        draw_menu(&names, selected);
                    }
                    Some(b'3') => { read_byte(); } // Delete key (~)
                    _ => {}
                }
            }
            b'q' => {
                clear_menu(names.len());
                break;
            }
            _ => {}
        }
    }

    std::os::toyos::io::set_stdin_raw(false);
}

fn draw_menu(names: &[&str], selected: usize) {
    let mut out = io::stdout().lock();
    write!(out, "\r").ok();
    for (i, name) in names.iter().enumerate() {
        if i == selected {
            write!(out, "\x1b[7m  {name}  \x1b[0m\x1b[K\r\n").ok();
        } else {
            write!(out, "  {name}  \x1b[K\r\n").ok();
        }
    }
    // Move cursor back up to top of menu
    for _ in 0..names.len() {
        write!(out, "\x1b[A").ok();
    }
    out.flush().ok();
}

fn clear_menu(rows: usize) {
    let mut out = io::stdout().lock();
    write!(out, "\r").ok();
    for _ in 0..rows {
        write!(out, "\x1b[2K\r\n").ok();
    }
    for _ in 0..rows {
        write!(out, "\x1b[A").ok();
    }
    out.flush().ok();
}

// --- `locale detect` ---

const ESC: u8 = 0x29;
const ENTER: u8 = 0x28;

/// Ask which layout this keyboard is, by asking its owner to press keys and
/// reading which HID usage each press reports.
///
/// It reads the keyboard device rather than stdin, because stdin carries what
/// the *current* layout made of the press and the question is which layout to
/// use. `RawKeyEvent::keycode` is the pre-layout usage and always has been —
/// no new syscall — but the device is claimed exclusively, so this runs only
/// where nothing else holds it. Under the compositor or `/bin/console` it
/// says so and stops.
fn detect() {
    let keyboard = match toyos::device::Keyboard::open() {
        Ok(keyboard) => keyboard,
        Err(e) => {
            eprintln!("locale: cannot read the keyboard directly: {e}");
            eprintln!(
                "locale: the keyboard is claimed exclusively, and the compositor and \
                 /bin/console both hold it while they run. Pick one by name instead: \
                 locale <name>, or locale --list."
            );
            return;
        }
    };

    println!("Answering with the keys you see, not the ones ToyOS thinks you have.");
    println!("Escape cancels.");

    let mut detector = Detector::new();
    loop {
        match detector.step() {
            Step::Ask(ask) => {
                println!("Press the key labelled  {}", ask.legend());
                io::stdout().flush().ok();
                let Some(usage) = next_press(&keyboard) else {
                    println!("cancelled");
                    return;
                };
                ask.observe(usage);
            }
            Step::Decided(index) => {
                let name = toyos_keymap::LAYOUTS[index].name;
                println!("That is '{name}'. Enter applies it, Escape cancels.");
                io::stdout().flush().ok();
                // Enter and Escape are the same usage on every layout here, so
                // the confirmation does not depend on the answer being right.
                match next_press(&keyboard) {
                    // Runtime only: nothing is written to the config store, so a
                    // reboot is back to the default.
                    Some(ENTER) => match set_keyboard_layout(name) {
                        Ok(()) => println!("Keyboard layout set to '{name}'"),
                        Err(e) => eprintln!("locale: failed to set layout '{name}': {e}"),
                    },
                    _ => println!("cancelled"),
                }
                return;
            }
            Step::Unrecognized => {
                let left: Vec<&str> = detector.candidates().collect();
                if left.is_empty() {
                    println!("No layout here puts those keys where you pressed them.");
                } else {
                    println!("Cannot tell {} apart from what was pressed.", left.join(" and "));
                }
                println!("Unrecognized — pick one manually with: locale <name>");
                for name in layouts() {
                    println!("  {name}");
                }
                return;
            }
        }
    }
}

/// The HID usage of the next key the user presses, or `None` if they pressed
/// Escape or nothing at all.
///
/// Releases and modifiers are skipped: the user may well hold Shift to reach a
/// legend, and a release is not a press.
///
/// One event per read. The kernel fills as many whole events as the buffer
/// holds, so a larger one would take the presses queued behind this one and
/// this function would have to drop them — and a wizard that drops the answer
/// to its next question is a wizard that hangs on a user who typed ahead.
fn next_press(keyboard: &toyos::device::Keyboard) -> Option<u8> {
    const EVENT_SIZE: usize = std::mem::size_of::<toyos_abi::input::RawKeyEvent>();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut buf = [0u8; EVENT_SIZE];
    while std::time::Instant::now() < deadline {
        if keyboard.read_nonblock(&mut buf).unwrap_or(0) != EVENT_SIZE {
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }
        let (usage, modifiers) = (buf[0], buf[1]);
        if modifiers & toyos_abi::input::MOD_RELEASED != 0 || (0xE0..=0xE7).contains(&usage) {
            continue;
        }
        if usage == ESC {
            return None;
        }
        return Some(usage);
    }
    println!("locale: nothing pressed for 60s");
    None
}

fn read_byte() -> Option<u8> {
    let mut buf = [0u8; 1];
    io::stdin().lock().read_exact(&mut buf).ok()?;
    Some(buf[0])
}

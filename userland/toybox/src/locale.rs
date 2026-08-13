use std::io::{self, Read, Write};
use toyos::surface::{self, GrabError, Keys};
use toyos_keymap::detect::{Detector, Step};

/// The names come from the same table every translator selects on, so this
/// cannot offer a layout that would be refused or hide one that exists. There
/// is still no way to ask which is *currently* active: that needs a read
/// syscall, and the one `specs/plans/introspection-plan.md` reserves for it
/// (`SYS_QUERY`) is not built.
fn layouts() -> impl Iterator<Item = &'static str> {
    toyos_keymap::LAYOUTS.iter().map(|l| l.name)
}

pub fn main(args: Vec<String>) {
    match args.first().map(|s| s.as_str()) {
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

/// Record `name` as the machine's layout.
///
/// The file is the layout. Nothing here tells a translator *which* one to use:
/// it tells them the file moved, and each re-reads it — so two surfaces cannot
/// end up holding different answers to a question the user asked once.
fn set(name: &str) {
    if toyos_keymap::by_name(name).is_none() {
        let available: Vec<&str> = layouts().collect();
        eprintln!("locale: no layout named '{name}'; available: {}", available.join(", "));
        return;
    }
    if let Some(dir) = std::path::Path::new(surface::LAYOUT_CONFIG).parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if let Err(e) = std::fs::write(surface::LAYOUT_CONFIG, name) {
        eprintln!("locale: failed to save config: {e}");
        return;
    }
    // Best effort by construction: the config is already written, so a program
    // with no surface above it has still changed the layout — for the next
    // translator that starts.
    surface::notify_layout_changed();
    println!("Keyboard layout set to '{name}'");
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

/// A minute per question, which is a user reading a keycap rather than a
/// deadline anything depends on.
const ANSWER_TIMEOUT_NS: u64 = 60 * 1_000_000_000;

/// Ask which layout this keyboard is, by asking its owner to press keys and
/// reading which HID usage each press reports.
///
/// The transitions come from the surface hosting this program — the terminal,
/// the console, whatever it is — and not from the keyboard device, which is
/// claimed by that same surface. What matters is that a HID usage is what
/// arrives: stdin would carry what the *current* layout made of the press, and
/// the question is which layout to use.
fn detect() {
    let mut keys = match Keys::grab() {
        Ok(keys) => keys,
        Err(GrabError::HostGone) => {
            eprintln!(
                "locale: nothing is hosting this program's keyboard. The wizard reads key \
                 positions, which only a surface — a terminal, /bin/console, a window — can \
                 hand over. Run it from one, or pick a layout by name: locale <name>."
            );
            return;
        }
        Err(e @ GrabError::Busy) => {
            eprintln!("locale: {e}. Finish with that first, or pick one by name: locale <name>.");
            return;
        }
        Err(e) => {
            eprintln!("locale: {e}; pick a layout by name instead: locale <name>, or locale --list.");
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
                let Some(usage) = next_press(&mut keys) else {
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
                match next_press(&mut keys) {
                    Some(ENTER) => set(name),
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
fn next_press(keys: &mut Keys) -> Option<u8> {
    loop {
        let Some(event) = keys.next(ANSWER_TIMEOUT_NS) else {
            println!("locale: nothing pressed for 60s");
            return None;
        };
        if event.released() || (0xE0..=0xE7).contains(&event.keycode) {
            continue;
        }
        return (event.keycode != ESC).then_some(event.keycode);
    }
}

fn read_byte() -> Option<u8> {
    let mut buf = [0u8; 1];
    io::stdin().lock().read_exact(&mut buf).ok()?;
    Some(buf[0])
}

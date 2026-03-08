use std::os::toyos::system::set_keyboard_layout;

const CONFIG_PATH: &str = "/home/root/.config/keyboard_layout";

pub fn main(args: Vec<String>) {
    match args.first().map(|s| s.as_str()) {
        Some("--load") => load(),
        Some(name) => set(name),
        None => {
            println!("Available layouts: us, swiss-german-mac");
            println!("Usage: locale <layout> | locale --load");
        }
    }
}

fn load() {
    let data = match std::fs::read_to_string(CONFIG_PATH) {
        Ok(data) => data,
        Err(_) => return,
    };
    let name = data.trim();
    if !name.is_empty() && !set_keyboard_layout(name) {
        eprintln!("locale: unknown layout '{name}' in config");
    }
}

fn set(name: &str) {
    if set_keyboard_layout(name) {
        std::fs::write(CONFIG_PATH, name).unwrap_or_else(|e| {
            eprintln!("locale: failed to save config: {e}");
        });
        println!("Keyboard layout set to '{name}'");
    } else {
        eprintln!("locale: unknown layout '{name}'");
    }
}

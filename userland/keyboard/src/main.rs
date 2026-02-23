use std::env;

fn main() {
    let name = match env::args().nth(1) {
        Some(name) => name,
        None => {
            println!("Available layouts: us, swiss-german-mac");
            println!("Usage: keyboard <name>");
            return;
        }
    };
    if std::os::toyos::io::set_keyboard_layout(&name) {
        println!("Keyboard layout set to '{}'", name);
    } else {
        eprintln!("keyboard: unknown layout '{}'", name);
    }
}

pub fn main(args: Vec<String>) {
    let name = match args.first() {
        Some(name) => name,
        None => {
            println!("Available layouts: us, swiss-german-mac");
            println!("Usage: keyboard <name>");
            return;
        }
    };
    if toyos_abi::syscall::set_keyboard_layout(name) {
        println!("Keyboard layout set to '{}'", name);
    } else {
        eprintln!("keyboard: unknown layout '{}'", name);
    }
}

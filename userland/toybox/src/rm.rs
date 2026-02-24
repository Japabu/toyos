use std::fs;

pub fn main(args: Vec<String>) {
    if args.is_empty() {
        eprintln!("Usage: rm <file>");
        return;
    }
    for path in &args {
        match fs::remove_file(path) {
            Ok(()) => println!("{}: deleted", path),
            Err(_) => eprintln!("{}: file not found", path),
        }
    }
}

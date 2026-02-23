use std::{env, fs};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: rm <file>");
        return;
    }
    for path in &args[1..] {
        match fs::remove_file(path) {
            Ok(()) => println!("{}: deleted", path),
            Err(_) => eprintln!("{}: file not found", path),
        }
    }
}

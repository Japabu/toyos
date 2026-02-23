use std::{env, fs};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: cat <file>");
        return;
    }
    for path in &args[1..] {
        match fs::read(path) {
            Ok(data) => {
                if let Ok(text) = std::str::from_utf8(&data) {
                    print!("{}", text);
                } else {
                    eprintln!("{}: {} bytes (binary)", path, data.len());
                }
            }
            Err(_) => eprintln!("{}: file not found", path),
        }
    }
}

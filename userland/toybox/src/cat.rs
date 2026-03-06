use std::fs;

pub fn main(args: Vec<String>) {
    if args.is_empty() {
        eprintln!("Usage: cat <file>");
        return;
    }
    for path in &args {
        match fs::read(path) {
            Ok(data) => {
                if let Ok(text) = std::str::from_utf8(&data) {
                    print!("{}", text);
                    if !text.ends_with('\n') {
                        println!();
                    }
                } else {
                    eprintln!("{}: {} bytes (binary)", path, data.len());
                }
            }
            Err(_) => eprintln!("{}: file not found", path),
        }
    }
}

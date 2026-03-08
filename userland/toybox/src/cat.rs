use std::fs;
use std::io::{self, Read};

pub fn main(args: Vec<String>) {
    if args.is_empty() {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf).unwrap();
        print!("{buf}");
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
            Err(_) => {
                eprintln!("{}: file not found", path);
                std::process::exit(1);
            }
        }
    }
}

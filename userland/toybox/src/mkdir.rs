use std::fs;

pub fn main(args: Vec<String>) {
    if args.is_empty() {
        eprintln!("Usage: mkdir <directory>");
        return;
    }
    for path in &args {
        if let Err(e) = fs::create_dir(path) {
            eprintln!("mkdir: {}: {}", path, e);
        }
    }
}

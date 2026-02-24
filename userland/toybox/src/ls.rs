use std::path::PathBuf;
use std::{env, fs};

pub fn main(args: Vec<String>) {
    let path = match args.first() {
        Some(arg) => {
            if arg.starts_with('/') {
                PathBuf::from(arg)
            } else {
                env::current_dir().unwrap_or_else(|_| "/".into()).join(arg)
            }
        }
        None => env::current_dir().unwrap_or_else(|_| "/".into()),
    };

    match fs::read_dir(&path) {
        Ok(entries) => {
            let mut any = false;
            for entry in entries {
                let Ok(entry) = entry else { continue };
                any = true;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Ok(ft) = entry.file_type() {
                    if ft.is_dir() {
                        println!("  {}/", name);
                    } else if let Ok(meta) = entry.metadata() {
                        println!("  {} ({} bytes)", name, meta.len());
                    } else {
                        println!("  {}", name);
                    }
                }
            }
            if !any {
                println!("No files.");
            }
        }
        Err(e) => eprintln!("ls: {}", e),
    }
}

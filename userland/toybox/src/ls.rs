use std::path::PathBuf;
use std::{env, fs};

pub fn main(args: Vec<String>) {
    let mut show_all = false;
    let mut path_arg: Option<&str> = None;

    for arg in &args {
        if arg.starts_with('-') {
            if arg.contains('a') {
                show_all = true;
            }
        } else {
            path_arg = Some(arg);
        }
    }

    let path = match path_arg {
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
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !show_all && name.starts_with('.') {
                    continue;
                }
                any = true;
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
        Err(e) => eprintln!("ls: {}: {}", path.display(), e),
    }
}

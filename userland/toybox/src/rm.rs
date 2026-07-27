use std::fs;

pub fn main(args: Vec<String>) {
    let mut recursive = false;
    let mut paths = Vec::new();

    for arg in &args {
        match arg.as_str() {
            "-r" | "-rf" | "-fr" => recursive = true,
            _ => paths.push(arg.as_str()),
        }
    }

    if paths.is_empty() {
        eprintln!("Usage: rm [-r] <path>...");
        return;
    }

    for path in paths {
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("rm: {}: {}", path, e);
                continue;
            }
        };

        let result = if meta.is_dir() {
            if recursive {
                fs::remove_dir_all(path)
            } else {
                eprintln!("rm: {}: is a directory (use -r)", path);
                continue;
            }
        } else {
            fs::remove_file(path)
        };

        match result {
            Ok(()) => println!("{}: deleted", path),
            Err(e) => eprintln!("rm: {}: {}", path, e),
        }
    }
}

use std::io::{self, BufRead};

pub fn main(args: Vec<String>) {
    if args.is_empty() {
        eprintln!("Usage: grep <pattern> [file...]");
        return;
    }

    let pattern = &args[0];
    let files = &args[1..];

    if files.is_empty() {
        let stdin = io::stdin().lock();
        grep_lines(pattern, stdin);
    } else {
        for path in files {
            match std::fs::read_to_string(path) {
                Ok(content) => grep_lines(pattern, content.as_bytes()),
                Err(_) => eprintln!("{}: file not found", path),
            }
        }
    }
}

fn grep_lines(pattern: &str, reader: impl BufRead) {
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.contains(pattern) {
            println!("{}", line);
        }
    }
}

use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};

const HISTORY_PATH: &str = "/nvme/config/shell_history";
const HISTORY_MAX: usize = 200;

fn main() {
    let _ = env::set_current_dir("/");
    let mut history = load_history();
    std::os::toyos::io::set_stdin_raw(true);

    loop {
        let cwd = env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| "?".into());
        print!("{}> ", cwd);
        io::stdout().flush().ok();

        let Some(input) = readline(&mut history) else { break };
        let input = input.trim().to_string();
        if input.is_empty() {
            continue;
        }

        if history.last().map_or(true, |last| *last != input) {
            history.push(input.clone());
            if history.len() > HISTORY_MAX {
                history.remove(0);
            }
            save_history(&history);
        }

        let segments: Vec<&str> = input.split('|').collect();
        if segments.len() > 1 {
            run_pipeline(&segments);
        } else {
            let (input, redirect) = parse_redirect(&input);
            let (cmd, arg) = parse_cmd_arg(&input);
            match cmd.as_str() {
                "help" => print_help(),
                "clear" => print!("\x1b[2J\x1b[H"),
                "cd" => cmd_cd(&arg),
                "run" => cmd_run(&arg, redirect.as_deref()),
                _ => cmd_exec(&cmd, &arg, redirect.as_deref()),
            }
        }
    }
}

fn load_history() -> Vec<String> {
    fs::read_to_string(HISTORY_PATH)
        .map(|s| s.lines().map(String::from).collect())
        .unwrap_or_default()
}

fn save_history(history: &[String]) {
    let content = history.join("\n");
    let _ = fs::write(HISTORY_PATH, &content);
}

// --- Readline helpers ---

fn read_byte() -> Option<u8> {
    let mut buf = [0u8; 1];
    io::stdin().lock().read_exact(&mut buf).ok()?;
    Some(buf[0])
}

fn read_char() -> Option<char> {
    let b = read_byte()?;
    if b < 0x80 {
        return Some(b as char);
    }
    let expected = if b < 0xE0 { 2 } else if b < 0xF0 { 3 } else { 4 };
    let mut buf = [0u8; 4];
    buf[0] = b;
    for i in 1..expected {
        buf[i] = read_byte()?;
    }
    Some(core::str::from_utf8(&buf[..expected])
        .ok()
        .and_then(|s| s.chars().next())
        .unwrap_or('\u{FFFD}'))
}

fn echo(bytes: &[u8]) {
    let mut out = io::stdout().lock();
    out.write_all(bytes).ok();
    out.flush().ok();
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(i, _)| i)
}

// --- Readline with history ---

fn readline(history: &mut Vec<String>) -> Option<String> {
    let mut line = String::new();
    let mut cursor: usize = 0;
    let mut hist_idx = history.len();
    let mut saved_input = String::new();

    loop {
        let ch = read_char()?;
        match ch {
            '\r' => {
                echo(b"\n");
                return Some(line);
            }
            '\x08' | '\x7F' => {
                if cursor > 0 {
                    cursor -= 1;
                    let byte_pos = char_to_byte(&line, cursor);
                    line.remove(byte_pos);
                    redraw(&line, cursor, true);
                }
            }
            '\x1B' => handle_escape(
                &mut line, &mut cursor, history, &mut hist_idx, &mut saved_input,
            ),
            ch if ch >= ' ' => {
                let byte_pos = char_to_byte(&line, cursor);
                line.insert(byte_pos, ch);
                cursor += 1;
                redraw(&line, cursor, false);
            }
            _ => {}
        }
    }
}

fn handle_escape(
    line: &mut String,
    cursor: &mut usize,
    history: &[String],
    hist_idx: &mut usize,
    saved_input: &mut String,
) {
    if read_byte() != Some(b'[') {
        return;
    }
    match read_byte().unwrap_or(0) {
        b'A' => {
            if *hist_idx > 0 {
                if *hist_idx == history.len() {
                    *saved_input = line.clone();
                }
                *hist_idx -= 1;
                let entry = history[*hist_idx].clone();
                replace_line(line, cursor, &entry);
            }
        }
        b'B' => {
            if *hist_idx < history.len() {
                *hist_idx += 1;
                let new = if *hist_idx == history.len() {
                    saved_input.clone()
                } else {
                    history[*hist_idx].clone()
                };
                replace_line(line, cursor, &new);
            }
        }
        b'C' => {
            let char_count = line.chars().count();
            if *cursor < char_count {
                let byte_pos = char_to_byte(line, *cursor);
                let next_byte = char_to_byte(line, *cursor + 1);
                echo(line[byte_pos..next_byte].as_bytes());
                *cursor += 1;
            }
        }
        b'D' => {
            if *cursor > 0 {
                *cursor -= 1;
                echo(&[0x08]);
            }
        }
        b'H' => {
            if *cursor > 0 {
                let buf: Vec<u8> = vec![0x08; *cursor];
                echo(&buf);
                *cursor = 0;
            }
        }
        b'F' => {
            let char_count = line.chars().count();
            if *cursor < char_count {
                let byte_pos = char_to_byte(line, *cursor);
                echo(line[byte_pos..].as_bytes());
                *cursor = char_count;
            }
        }
        b'3' => {
            let char_count = line.chars().count();
            if read_byte() == Some(b'~') && *cursor < char_count {
                let byte_pos = char_to_byte(line, *cursor);
                line.remove(byte_pos);
                let mut buf = Vec::new();
                buf.extend_from_slice(line[byte_pos..].as_bytes());
                buf.push(b' ');
                let chars_after = line.chars().count() - *cursor;
                for _ in 0..chars_after + 1 {
                    buf.push(0x08);
                }
                echo(&buf);
            }
        }
        b'5' | b'6' => {
            read_byte();
        }
        _ => {}
    }
}

fn replace_line(line: &mut String, cursor: &mut usize, new_content: &str) {
    let old_chars = line.chars().count();
    let new_chars = new_content.chars().count();
    let mut buf = Vec::new();
    for _ in 0..*cursor {
        buf.push(0x08);
    }
    buf.extend_from_slice(new_content.as_bytes());
    if new_chars < old_chars {
        for _ in 0..(old_chars - new_chars) {
            buf.push(b' ');
        }
        for _ in 0..(old_chars - new_chars) {
            buf.push(0x08);
        }
    }
    echo(&buf);
    line.clear();
    line.push_str(new_content);
    *cursor = new_chars;
}

fn redraw(line: &str, cursor: usize, backspace: bool) {
    let char_count = line.chars().count();
    let start_byte = if backspace {
        char_to_byte(line, cursor)
    } else {
        char_to_byte(line, cursor - 1)
    };
    let mut buf = Vec::new();
    if backspace {
        buf.push(0x08);
    }
    buf.extend_from_slice(line[start_byte..].as_bytes());
    if backspace {
        buf.push(b' ');
    }
    let back = char_count - cursor + if backspace { 1 } else { 0 };
    for _ in 0..back {
        buf.push(0x08);
    }
    echo(&buf);
}

// --- Parsing helpers ---

fn parse_redirect(input: &str) -> (String, Option<String>) {
    match input.find('>') {
        Some(pos) => {
            let file = input[pos + 1..].trim().to_string();
            (input[..pos].trim().to_string(), Some(file))
        }
        None => (input.to_string(), None),
    }
}

fn parse_cmd_arg(input: &str) -> (String, String) {
    match input.find(' ') {
        Some(pos) => (input[..pos].to_string(), input[pos + 1..].trim().to_string()),
        None => (input.to_string(), String::new()),
    }
}

fn build_command(cmd: &str, arg: &str) -> Command {
    let path = if cmd.starts_with('/') { cmd.to_string() } else { format!("/initrd/{}", cmd) };
    let mut command = Command::new(&path);
    if !arg.is_empty() {
        for a in arg.split_whitespace() {
            command.arg(a);
        }
    }
    command
}

// --- Pipeline execution ---

fn run_pipeline(segments: &[&str]) {
    let mut children = Vec::new();
    let mut prev_stdout: Option<std::process::ChildStdout> = None;

    for (i, segment) in segments.iter().enumerate() {
        let is_last = i == segments.len() - 1;

        let (segment, redirect) = if is_last {
            parse_redirect(segment.trim())
        } else {
            (segment.trim().to_string(), None)
        };

        let (cmd, arg) = parse_cmd_arg(&segment);
        let mut command = build_command(&cmd, &arg);

        if let Some(stdout) = prev_stdout.take() {
            command.stdin(stdout);
        }

        if !is_last {
            command.stdout(Stdio::piped());
        } else if let Some(ref path) = redirect {
            match File::create(path) {
                Ok(file) => { command.stdout(file); }
                Err(_) => {
                    println!("cannot open: {}", path);
                    return;
                }
            }
        }

        match command.spawn() {
            Ok(mut child) => {
                if !is_last {
                    prev_stdout = child.stdout.take();
                }
                children.push(child);
            }
            Err(_) => {
                println!("{}: not found", cmd);
                break;
            }
        }
    }

    for mut child in children {
        let _ = child.wait();
    }
}

// --- Shell commands ---

fn print_help() {
    println!("Builtins:");
    println!("  cd <path>     Change directory");
    println!("  clear         Clear screen");
    println!("  run <file>    Run program by path");
    println!("  help          Show this help");
    println!();
    println!("Programs in /initrd/ are available by name.");
}

fn cmd_cd(arg: &str) {
    let target = if arg.is_empty() { "/" } else { arg };
    if env::set_current_dir(target).is_err() {
        println!("cd: {}: no such directory", target);
    }
}

fn cmd_run(arg: &str, redirect: Option<&str>) {
    if arg.is_empty() {
        println!("Usage: run <file>");
        return;
    }
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let mut command = Command::new(parts[0]);
    command.args(&parts[1..]);
    run_single(parts[0], &mut command, redirect);
}

fn cmd_exec(cmd: &str, arg: &str, redirect: Option<&str>) {
    let mut command = build_command(cmd, arg);
    run_single(cmd, &mut command, redirect);
}

fn run_single(name: &str, command: &mut Command, redirect: Option<&str>) {
    if let Some(path) = redirect {
        match File::create(path) {
            Ok(file) => { command.stdout(file); }
            Err(_) => {
                println!("cannot open: {}", path);
                return;
            }
        }
    }
    match command.status() {
        Ok(code) => {
            if !code.success() {
                println!("Process exited with code {}", code.code().unwrap_or(-1));
            }
        }
        Err(_) => println!("{}: not found", name),
    }
}

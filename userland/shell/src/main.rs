use std::env;
use std::fs;
use std::io::{self, Write};
use std::process::Command;

fn main() {
    let _ = env::set_current_dir("/");
    let mut history: Vec<String> = Vec::new();

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
        }

        let (cmd, arg) = match input.find(' ') {
            Some(pos) => (&input[..pos], input[pos + 1..].trim()),
            None => (input.as_str(), ""),
        };

        match cmd {
            "help" => print_help(),
            "clear" => print!("\x1b[2J\x1b[H"),
            "shutdown" => shutdown(),
            "pwd" => println!("{}", cwd),
            "cd" => cmd_cd(arg),
            "ls" => cmd_ls(arg),
            "cat" => cmd_cat(arg),
            "rm" => cmd_rm(arg),
            "write" => cmd_write(arg),
            "edit" => cmd_edit(arg),
            "run" => cmd_run(arg),
            _ => cmd_exec(cmd, arg),
        }
    }
}

// --- Raw syscall helpers ---

fn syscall(num: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret: u64;
    unsafe {
        std::arch::asm!(
            "syscall",
            in("rdi") num,
            in("rsi") a1,
            in("rdx") a2,
            in("r8") a3,
            in("r9") a4,
            lateout("rax") ret,
            out("rcx") _,
            out("r11") _,
        );
    }
    ret
}

fn read_raw(buf: &mut [u8]) -> usize {
    syscall(1, buf.as_mut_ptr() as u64, buf.len() as u64, 1, 0) as usize
}

fn read_byte() -> u8 {
    let mut buf = [0u8; 1];
    read_raw(&mut buf);
    buf[0]
}

fn echo(bytes: &[u8]) {
    let mut out = io::stdout().lock();
    out.write_all(bytes).ok();
    out.flush().ok();
}

// --- Readline with history ---

fn readline(history: &mut Vec<String>) -> Option<String> {
    let mut line: Vec<u8> = Vec::new();
    let mut cursor: usize = 0;
    let mut hist_idx = history.len();
    let mut saved_input = String::new();

    loop {
        let ch = read_byte();
        match ch {
            b'\n' => {
                echo(b"\n");
                return Some(String::from_utf8_lossy(&line).into_owned());
            }
            0x08 | 0x7F => {
                if cursor > 0 {
                    line.remove(cursor - 1);
                    cursor -= 1;
                    redraw(&line, cursor, true);
                }
            }
            0x1B => handle_escape(
                &mut line, &mut cursor, history, &mut hist_idx, &mut saved_input,
            ),
            ch if ch >= 0x20 => {
                line.insert(cursor, ch);
                cursor += 1;
                redraw(&line, cursor, false);
            }
            _ => {}
        }
    }
}

fn handle_escape(
    line: &mut Vec<u8>,
    cursor: &mut usize,
    history: &[String],
    hist_idx: &mut usize,
    saved_input: &mut String,
) {
    if read_byte() != b'[' {
        return;
    }
    match read_byte() {
        b'A' => {
            // Up — previous history entry
            if *hist_idx > 0 {
                if *hist_idx == history.len() {
                    *saved_input = String::from_utf8_lossy(line).into_owned();
                }
                *hist_idx -= 1;
                replace_line(line, cursor, history[*hist_idx].as_bytes());
            }
        }
        b'B' => {
            // Down — next history entry
            if *hist_idx < history.len() {
                *hist_idx += 1;
                let new = if *hist_idx == history.len() {
                    saved_input.as_bytes().to_vec()
                } else {
                    history[*hist_idx].as_bytes().to_vec()
                };
                replace_line(line, cursor, &new);
            }
        }
        b'C' => {
            // Right
            if *cursor < line.len() {
                echo(&[line[*cursor]]);
                *cursor += 1;
            }
        }
        b'D' => {
            // Left
            if *cursor > 0 {
                *cursor -= 1;
                echo(&[0x08]);
            }
        }
        b'H' => {
            // Home
            if *cursor > 0 {
                let buf: Vec<u8> = vec![0x08; *cursor];
                echo(&buf);
                *cursor = 0;
            }
        }
        b'F' => {
            // End
            if *cursor < line.len() {
                echo(&line[*cursor..]);
                *cursor = line.len();
            }
        }
        b'3' => {
            // Delete: ESC[3~
            if read_byte() == b'~' && *cursor < line.len() {
                line.remove(*cursor);
                let mut buf = Vec::new();
                buf.extend_from_slice(&line[*cursor..]);
                buf.push(b' ');
                let back = line.len() - *cursor + 1;
                for _ in 0..back {
                    buf.push(0x08);
                }
                echo(&buf);
            }
        }
        b'5' | b'6' => {
            read_byte(); // consume '~' for Page Up/Down
        }
        _ => {}
    }
}

/// Replace the visible line with new content (used for history navigation).
fn replace_line(line: &mut Vec<u8>, cursor: &mut usize, new_content: &[u8]) {
    let old_len = line.len();
    let mut buf = Vec::new();
    // Move console cursor to start of input
    for _ in 0..*cursor {
        buf.push(0x08);
    }
    // Write new content
    buf.extend_from_slice(new_content);
    // Clear remaining chars if new content is shorter
    if new_content.len() < old_len {
        for _ in 0..(old_len - new_content.len()) {
            buf.push(b' ');
        }
        for _ in 0..(old_len - new_content.len()) {
            buf.push(0x08);
        }
    }
    echo(&buf);
    line.clear();
    line.extend_from_slice(new_content);
    *cursor = new_content.len();
}

/// Redraw line after an insert or backspace at cursor position.
fn redraw(line: &[u8], cursor: usize, backspace: bool) {
    let mut buf = Vec::new();
    let start = if backspace {
        buf.push(0x08);
        cursor
    } else {
        cursor - 1
    };
    buf.extend_from_slice(&line[start..]);
    if backspace {
        buf.push(b' ');
    }
    let back = line.len() - cursor + if backspace { 1 } else { 0 };
    for _ in 0..back {
        buf.push(0x08);
    }
    echo(&buf);
}

// --- Shell commands ---

fn print_help() {
    println!("Commands:");
    println!("  ls [path]       List files");
    println!("  cat <file>      Print file contents");
    println!("  rm <file>       Delete a file");
    println!("  write <file>    Create a file");
    println!("  edit <file>     Edit a file");
    println!("  cd <path>       Change directory");
    println!("  pwd             Print working directory");
    println!("  clear           Clear screen");
    println!("  run <file>      Run an ELF program");
    println!("  shutdown        Power off");
}

fn cmd_cd(arg: &str) {
    let target = if arg.is_empty() { "/" } else { arg };
    if env::set_current_dir(target).is_err() {
        println!("cd: {}: no such directory", target);
    }
}

fn cmd_ls(arg: &str) {
    let path = if arg.is_empty() {
        env::current_dir().unwrap_or_else(|_| "/".into())
    } else if arg.starts_with('/') {
        arg.into()
    } else {
        env::current_dir().unwrap_or_else(|_| "/".into()).join(arg)
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
        Err(e) => println!("ls: {}", e),
    }
}

fn cmd_cat(arg: &str) {
    if arg.is_empty() {
        println!("Usage: cat <file>");
        return;
    }
    let path = resolve(arg);
    match fs::read(&path) {
        Ok(data) => {
            if let Ok(text) = std::str::from_utf8(&data) {
                println!("{}", text);
            } else {
                println!("{}: {} bytes (binary)", arg, data.len());
            }
        }
        Err(_) => println!("{}: file not found", arg),
    }
}

fn cmd_rm(arg: &str) {
    if arg.is_empty() {
        println!("Usage: rm <file>");
        return;
    }
    let path = resolve(arg);
    match fs::remove_file(&path) {
        Ok(()) => println!("{}: deleted", arg),
        Err(_) => println!("{}: file not found", arg),
    }
}

fn cmd_write(arg: &str) {
    if arg.is_empty() {
        println!("Usage: write <file>");
        return;
    }
    println!("Enter text (type . on a line by itself to save):");
    let text = read_text_block();
    let path = resolve(arg);
    if fs::write(&path, &text).is_ok() {
        println!("File saved.");
    } else {
        println!("Error: could not save file.");
    }
}

fn cmd_edit(arg: &str) {
    if arg.is_empty() {
        println!("Usage: edit <file>");
        return;
    }
    let path = resolve(arg);
    match fs::read(&path) {
        Ok(data) => {
            if let Ok(text) = std::str::from_utf8(&data) {
                println!("Current contents:");
                println!("{}", text);
            } else {
                println!("{}: binary file, cannot edit", arg);
                return;
            }
        }
        Err(_) => println!("(new file)"),
    }
    println!("Enter new text (type . on a line by itself to save):");
    let text = read_text_block();
    if fs::write(&path, &text).is_ok() {
        println!("File saved.");
    } else {
        println!("Error: could not save file.");
    }
}

fn read_text_block() -> String {
    let mut text = String::new();
    let mut line = String::new();
    loop {
        print!("| ");
        io::stdout().flush().ok();
        line.clear();
        if io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line.trim() == "." {
            break;
        }
        text.push_str(&line);
    }
    text
}

fn cmd_run(arg: &str) {
    if arg.is_empty() {
        println!("Usage: run <file>");
        return;
    }
    let name = arg.split_whitespace().next().unwrap_or(arg);
    run_program(name, arg);
}

fn cmd_exec(cmd: &str, arg: &str) {
    // PATH lookup: try /initrd/<cmd>
    let path = format!("/initrd/{}", cmd);
    let full = if arg.is_empty() {
        path.clone()
    } else {
        format!("{} {}", path, arg)
    };
    run_program(cmd, &full);
}

fn run_program(display_name: &str, arg: &str) {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let Some(program) = parts.first() else { return };

    let status = Command::new(program)
        .args(&parts[1..])
        .status();

    match status {
        Ok(code) => {
            if !code.success() {
                println!("Process exited with code {}", code.code().unwrap_or(-1));
            }
        }
        Err(_) => println!("{}: not found", display_name),
    }
}

/// Resolve a path relative to cwd, returning an absolute path string.
fn resolve(arg: &str) -> String {
    if arg.starts_with('/') {
        arg.to_string()
    } else {
        let cwd = env::current_dir().unwrap_or_else(|_| "/".into());
        format!("{}/{}", cwd.display(), arg)
    }
}

/// Raw syscall to power off the machine.
fn shutdown() -> ! {
    syscall(19, 0, 0, 0, 0);
    unreachable!()
}

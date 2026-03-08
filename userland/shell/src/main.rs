use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::process::{Command, ExitStatus, Stdio};

const HISTORY_PATH: &str = "/home/root/.config/shell_history";
const HISTORY_MAX: usize = 200;

static mut LAST_STATUS: i32 = 0;

fn main() {
    if env::var_os("PATH").is_none() {
        env::set_var("PATH", "/bin");
    }
    if env::var_os("HOME").is_none() {
        env::set_var("HOME", "/home/root");
    }

    let args: Vec<String> = env::args().collect();
    if args.len() >= 3 && args[1] == "-c" {
        let input = args[2..].join(" ");
        let _ = env::set_current_dir("/");
        execute_line(&input);
        std::process::exit(unsafe { LAST_STATUS });
    }

    let home = env::var("HOME").unwrap_or_else(|_| "/".into());
    let _ = env::set_current_dir(&home);
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

        execute_line(&input);
    }
}

// --- Tokenizer ---

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Pipe,
    And,        // &&
    Or,         // ||
    Semi,       // ;
    Redirect,   // >
    Append,     // >>
    Background, // &
}

fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    let mut word = String::new();

    while let Some(&ch) = chars.peek() {
        match ch {
            '\'' => {
                chars.next();
                while let Some(&c) = chars.peek() {
                    if c == '\'' { chars.next(); break; }
                    word.push(c);
                    chars.next();
                }
            }
            '"' => {
                chars.next();
                while let Some(&c) = chars.peek() {
                    if c == '"' { chars.next(); break; }
                    if c == '\\' {
                        chars.next();
                        if let Some(&escaped) = chars.peek() {
                            match escaped {
                                '"' | '\\' | '$' => { word.push(escaped); chars.next(); }
                                _ => { word.push('\\'); word.push(escaped); chars.next(); }
                            }
                        }
                    } else {
                        if c == '$' {
                            chars.next();
                            word.push_str(&expand_var(&mut chars));
                        } else {
                            word.push(c);
                            chars.next();
                        }
                    }
                }
            }
            '$' => {
                chars.next();
                word.push_str(&expand_var(&mut chars));
            }
            '|' => {
                if !word.is_empty() { tokens.push(Token::Word(std::mem::take(&mut word))); }
                chars.next();
                if chars.peek() == Some(&'|') {
                    chars.next();
                    tokens.push(Token::Or);
                } else {
                    tokens.push(Token::Pipe);
                }
            }
            '&' => {
                if !word.is_empty() { tokens.push(Token::Word(std::mem::take(&mut word))); }
                chars.next();
                if chars.peek() == Some(&'&') {
                    chars.next();
                    tokens.push(Token::And);
                } else {
                    tokens.push(Token::Background);
                }
            }
            ';' => {
                if !word.is_empty() { tokens.push(Token::Word(std::mem::take(&mut word))); }
                chars.next();
                tokens.push(Token::Semi);
            }
            '>' => {
                if !word.is_empty() { tokens.push(Token::Word(std::mem::take(&mut word))); }
                chars.next();
                if chars.peek() == Some(&'>') {
                    chars.next();
                    tokens.push(Token::Append);
                } else {
                    tokens.push(Token::Redirect);
                }
            }
            '\\' => {
                chars.next();
                if let Some(&c) = chars.peek() {
                    word.push(c);
                    chars.next();
                }
            }
            ' ' | '\t' => {
                if !word.is_empty() { tokens.push(Token::Word(std::mem::take(&mut word))); }
                chars.next();
            }
            _ => {
                word.push(ch);
                chars.next();
            }
        }
    }
    if !word.is_empty() { tokens.push(Token::Word(word)); }
    tokens
}

fn expand_var(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    if chars.peek() == Some(&'?') {
        chars.next();
        return format!("{}", unsafe { LAST_STATUS });
    }
    let braced = chars.peek() == Some(&'{');
    if braced { chars.next(); }
    let mut name = String::new();
    while let Some(&c) = chars.peek() {
        if braced {
            if c == '}' { chars.next(); break; }
            name.push(c);
            chars.next();
        } else if c.is_alphanumeric() || c == '_' {
            name.push(c);
            chars.next();
        } else {
            break;
        }
    }
    if name.is_empty() {
        return String::from("$");
    }
    env::var(&name).unwrap_or_default()
}

// --- Command structure ---

enum Redirect {
    Truncate(String),
    Append(String),
}

struct SimpleCommand {
    args: Vec<String>,
    redirect: Option<Redirect>,
}

/// Parse tokens into a single pipeline (sequence of commands connected by |)
fn parse_pipeline(tokens: &[Token]) -> Vec<SimpleCommand> {
    let mut commands = Vec::new();
    let mut current_args = Vec::new();
    let mut redirect: Option<Redirect> = None;

    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            Token::Word(w) => { current_args.push(w.clone()); }
            Token::Redirect => {
                i += 1;
                if let Some(Token::Word(path)) = tokens.get(i) {
                    redirect = Some(Redirect::Truncate(path.clone()));
                }
            }
            Token::Append => {
                i += 1;
                if let Some(Token::Word(path)) = tokens.get(i) {
                    redirect = Some(Redirect::Append(path.clone()));
                }
            }
            Token::Pipe => {
                if !current_args.is_empty() {
                    commands.push(SimpleCommand { args: std::mem::take(&mut current_args), redirect: redirect.take() });
                }
            }
            _ => break, // &&, ||, ; are handled at a higher level
        }
        i += 1;
    }
    if !current_args.is_empty() {
        commands.push(SimpleCommand { args: current_args, redirect });
    }
    commands
}

// --- Execution ---

/// Split input into command groups by &&, ||, ; (respecting quotes).
/// Returns (command_str, separator) pairs.
fn split_commands(input: &str) -> Vec<(String, Option<Token>)> {
    let mut groups = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();

    while let Some(&ch) = chars.peek() {
        match ch {
            '\'' => {
                current.push(ch); chars.next();
                while let Some(&c) = chars.peek() {
                    current.push(c); chars.next();
                    if c == '\'' { break; }
                }
            }
            '"' => {
                current.push(ch); chars.next();
                while let Some(&c) = chars.peek() {
                    current.push(c); chars.next();
                    if c == '"' { break; }
                    if c == '\\' { if let Some(&e) = chars.peek() { current.push(e); chars.next(); } }
                }
            }
            '&' => {
                chars.next();
                if chars.peek() == Some(&'&') {
                    chars.next();
                    groups.push((std::mem::take(&mut current), Some(Token::And)));
                } else {
                    current.push('&');
                }
            }
            '|' => {
                chars.next();
                if chars.peek() == Some(&'|') {
                    chars.next();
                    groups.push((std::mem::take(&mut current), Some(Token::Or)));
                } else {
                    current.push('|');
                }
            }
            ';' => {
                chars.next();
                groups.push((std::mem::take(&mut current), Some(Token::Semi)));
            }
            _ => { current.push(ch); chars.next(); }
        }
    }
    if !current.trim().is_empty() {
        groups.push((current, None));
    }
    groups
}

fn execute_line(input: &str) {
    let groups = split_commands(input);
    let mut last_ok = true;

    for (i, (cmd_str, _separator)) in groups.iter().enumerate() {
        // Check condition from previous separator
        if i > 0 {
            if let Some((_, Some(prev_sep))) = groups.get(i - 1) {
                match prev_sep {
                    Token::And => { if !last_ok { continue; } }
                    Token::Or => { if last_ok { continue; } }
                    Token::Semi => {}
                    _ => {}
                }
            }
        }

        // Tokenize NOW (after previous commands may have set env vars)
        let tokens = tokenize(cmd_str);
        if tokens.is_empty() { continue; }
        let pipeline = parse_pipeline(&tokens);
        if pipeline.is_empty() { continue; }

        last_ok = execute_pipeline(&pipeline);
    }
}

fn execute_pipeline(pipeline: &[SimpleCommand]) -> bool {
    if pipeline.len() == 1 {
        return execute_simple(&pipeline[0], None);
    }

    let mut children = Vec::new();
    let mut prev_stdout: Option<std::process::ChildStdout> = None;

    for (i, cmd) in pipeline.iter().enumerate() {
        let is_last = i == pipeline.len() - 1;
        let Some(mut command) = build_command(cmd) else {
            println!("{}: not found", cmd.args[0]);
            return false;
        };

        if let Some(stdout) = prev_stdout.take() {
            command.stdin(stdout);
        }

        if !is_last {
            command.stdout(Stdio::piped());
        } else {
            apply_redirect(&mut command, &cmd.redirect);
        }

        match command.spawn() {
            Ok(mut child) => {
                if !is_last {
                    prev_stdout = child.stdout.take();
                }
                children.push(child);
            }
            Err(_) => {
                println!("{}: not found", cmd.args[0]);
                return false;
            }
        }
    }

    let mut ok = true;
    for mut child in children {
        if let Ok(status) = child.wait() {
            if !status.success() { ok = false; }
            set_status(&status);
        }
    }
    ok
}

fn execute_simple(cmd: &SimpleCommand, piped_stdin: Option<std::process::ChildStdout>) -> bool {
    if cmd.args.is_empty() { return true; }

    // Builtins
    match cmd.args[0].as_str() {
        "cd" => {
            let target = cmd.args.get(1).map_or("/", |s| s.as_str());
            if env::set_current_dir(target).is_err() {
                println!("cd: {}: no such directory", target);
                set_status_code(1);
                return false;
            }
            set_status_code(0);
            return true;
        }
        "true" => { set_status_code(0); return true; }
        "false" => { set_status_code(1); return false; }
        "exit" => std::process::exit(cmd.args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0)),
        "clear" => { print!("\x1b[2J\x1b[H"); set_status_code(0); return true; }
        "export" => {
            for arg in &cmd.args[1..] {
                if let Some(eq) = arg.find('=') {
                    env::set_var(&arg[..eq], &arg[eq + 1..]);
                }
            }
            set_status_code(0);
            return true;
        }
        "help" => { print_help(); set_status_code(0); return true; }
        _ => {}
    }

    let Some(mut command) = build_command(cmd) else {
        println!("{}: not found", cmd.args[0]);
        set_status_code(127);
        return false;
    };

    if let Some(stdin) = piped_stdin {
        command.stdin(stdin);
    }
    apply_redirect(&mut command, &cmd.redirect);

    match command.status() {
        Ok(status) => {
            set_status(&status);
            status.success()
        }
        Err(_) => {
            println!("{}: not found", cmd.args[0]);
            set_status_code(127);
            false
        }
    }
}

fn build_command(cmd: &SimpleCommand) -> Option<Command> {
    if cmd.args.is_empty() { return None; }
    let mut command = Command::new(&cmd.args[0]);
    command.args(&cmd.args[1..]);
    Some(command)
}

fn apply_redirect(command: &mut Command, redirect: &Option<Redirect>) {
    match redirect {
        Some(Redirect::Truncate(path)) => {
            if let Ok(file) = File::create(path) {
                command.stdout(file);
            } else {
                eprintln!("cannot open: {}", path);
            }
        }
        Some(Redirect::Append(path)) => {
            if let Ok(file) = OpenOptions::new().create(true).append(true).open(path) {
                command.stdout(file);
            } else {
                eprintln!("cannot open: {}", path);
            }
        }
        None => {}
    }
}

fn set_status(status: &ExitStatus) {
    unsafe { LAST_STATUS = status.code().unwrap_or(1); }
}

fn set_status_code(code: i32) {
    unsafe { LAST_STATUS = code; }
}

fn print_help() {
    println!("Builtins: cd, clear, exit, export, help");
    println!("Operators: | (pipe), && (and), || (or), ; (sequence)");
    println!("Redirects: > (truncate), >> (append)");
    println!("Variables: $VAR, ${{VAR}}, $? (exit status)");
    println!("Quoting: 'literal', \"with $expansion\"");
    println!();
    println!("Programs in /bin/ are available by name.");
}

// --- History ---

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

fn term_echo(bytes: &[u8]) {
    let mut out = io::stdout().lock();
    out.write_all(bytes).ok();
    out.flush().ok();
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(i, _)| i)
}

fn readline(history: &mut Vec<String>) -> Option<String> {
    let mut line = String::new();
    let mut cursor: usize = 0;
    let mut hist_idx = history.len();
    let mut saved_input = String::new();

    loop {
        let ch = read_char()?;
        match ch {
            '\r' => {
                term_echo(b"\n");
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
                term_echo(line[byte_pos..next_byte].as_bytes());
                *cursor += 1;
            }
        }
        b'D' => {
            if *cursor > 0 {
                *cursor -= 1;
                term_echo(&[0x08]);
            }
        }
        b'H' => {
            if *cursor > 0 {
                let buf: Vec<u8> = vec![0x08; *cursor];
                term_echo(&buf);
                *cursor = 0;
            }
        }
        b'F' => {
            let char_count = line.chars().count();
            if *cursor < char_count {
                let byte_pos = char_to_byte(line, *cursor);
                term_echo(line[byte_pos..].as_bytes());
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
                term_echo(&buf);
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
    term_echo(&buf);
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
    term_echo(&buf);
}

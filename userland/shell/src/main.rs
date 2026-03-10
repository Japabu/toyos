use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
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
    And,           // &&
    Or,            // ||
    Semi,          // ;
    Redirect,        // >
    Append,          // >>
    StderrRedirect,  // 2>
    StderrAppend,    // 2>>
    StderrToStdout,  // 2>&1
    InputRedirect,   // <
    Background,    // &
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
            '<' => {
                if !word.is_empty() { tokens.push(Token::Word(std::mem::take(&mut word))); }
                chars.next();
                if chars.peek() == Some(&'<') {
                    chars.next();
                    // << (heredoc marker) — skip delimiter word, already pre-processed
                    // Consume remaining word on this token level
                    while chars.peek().map_or(false, |c| c.is_whitespace()) { chars.next(); }
                    let mut delim = String::new();
                    while let Some(&c) = chars.peek() {
                        if c.is_whitespace() || c == ';' || c == '&' || c == '|' { break; }
                        if c == '\'' || c == '"' {
                            chars.next();
                            while let Some(&d) = chars.peek() {
                                if d == c { chars.next(); break; }
                                delim.push(d);
                                chars.next();
                            }
                        } else {
                            delim.push(c);
                            chars.next();
                        }
                    }
                    // Heredoc delimiter consumed, nothing emitted — body is pre-processed
                } else {
                    tokens.push(Token::InputRedirect);
                }
            }
            '>' => {
                let is_stderr = word == "2";
                if is_stderr {
                    word.clear();
                } else if !word.is_empty() {
                    tokens.push(Token::Word(std::mem::take(&mut word)));
                }
                chars.next();
                if is_stderr {
                    if chars.peek() == Some(&'>') {
                        chars.next();
                        tokens.push(Token::StderrAppend);
                    } else if chars.peek() == Some(&'&') {
                        chars.next();
                        // 2>&1
                        if chars.peek() == Some(&'1') {
                            chars.next();
                            tokens.push(Token::StderrToStdout);
                        }
                    } else {
                        tokens.push(Token::StderrRedirect);
                    }
                } else if chars.peek() == Some(&'>') {
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

enum StderrRedirect {
    ToFile(String),
    AppendFile(String),
    ToStdout,
}

struct SimpleCommand {
    args: Vec<String>,
    redirect: Option<Redirect>,
    stderr: Option<StderrRedirect>,
    input_file: Option<String>,
    heredoc: Option<String>,
}

/// Parse tokens into a single pipeline (sequence of commands connected by |)
fn parse_pipeline(tokens: &[Token]) -> Vec<SimpleCommand> {
    let mut commands = Vec::new();
    let mut current_args = Vec::new();
    let mut redirect: Option<Redirect> = None;
    let mut stderr: Option<StderrRedirect> = None;
    let mut input_file: Option<String> = None;

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
            Token::StderrRedirect => {
                i += 1;
                if let Some(Token::Word(path)) = tokens.get(i) {
                    stderr = Some(StderrRedirect::ToFile(path.clone()));
                }
            }
            Token::StderrAppend => {
                i += 1;
                if let Some(Token::Word(path)) = tokens.get(i) {
                    stderr = Some(StderrRedirect::AppendFile(path.clone()));
                }
            }
            Token::StderrToStdout => {
                stderr = Some(StderrRedirect::ToStdout);
            }
            Token::InputRedirect => {
                i += 1;
                if let Some(Token::Word(path)) = tokens.get(i) {
                    input_file = Some(path.clone());
                }
            }
            Token::Pipe => {
                if !current_args.is_empty() {
                    commands.push(SimpleCommand {
                        args: std::mem::take(&mut current_args),
                        redirect: redirect.take(),
                        stderr: stderr.take(),
                        input_file: input_file.take(),
                        heredoc: None,
                    });
                }
            }
            _ => break,
        }
        i += 1;
    }
    if !current_args.is_empty() {
        commands.push(SimpleCommand { args: current_args, redirect, stderr, input_file, heredoc: None });
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

/// Extract heredoc marker from a line. Returns (line_before_heredoc, delimiter) if found.
fn find_heredoc_marker(line: &str) -> Option<(String, String)> {
    let mut chars = line.chars().peekable();
    let mut before = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while let Some(&ch) = chars.peek() {
        if ch == '\'' && !in_double_quote { in_single_quote = !in_single_quote; before.push(ch); chars.next(); continue; }
        if ch == '"' && !in_single_quote { in_double_quote = !in_double_quote; before.push(ch); chars.next(); continue; }
        if in_single_quote || in_double_quote { before.push(ch); chars.next(); continue; }

        if ch == '<' {
            chars.next();
            if chars.peek() == Some(&'<') {
                chars.next();
                // Found << — extract delimiter
                let rest: String = chars.collect();
                let rest = rest.trim();
                let delimiter = rest.trim_matches(|c| c == '\'' || c == '"');
                let delimiter = delimiter.split_whitespace().next().unwrap_or("").to_string();
                if !delimiter.is_empty() {
                    return Some((before, delimiter));
                }
                return None;
            } else {
                before.push('<');
            }
        } else {
            before.push(ch);
            chars.next();
        }
    }
    None
}

fn execute_line(input: &str) {
    // Multi-line input with heredocs: process line by line
    if input.contains('\n') {
        let lines: Vec<&str> = input.lines().collect();
        let mut i = 0;
        let mut accumulated = String::new();

        while i < lines.len() {
            let line = lines[i];
            i += 1;

            if let Some((cmd_part, delimiter)) = find_heredoc_marker(line) {
                // Collect heredoc body
                let mut body = String::new();
                while i < lines.len() {
                    if lines[i].trim() == delimiter {
                        i += 1;
                        break;
                    }
                    if !body.is_empty() { body.push('\n'); }
                    body.push_str(lines[i]);
                    i += 1;
                }
                body.push('\n');

                // Combine with any accumulated commands
                let full_cmd = if accumulated.is_empty() {
                    cmd_part
                } else {
                    let prev = std::mem::take(&mut accumulated);
                    format!("{} ; {}", prev, cmd_part)
                };
                execute_line_with_heredoc(&full_cmd, body);
            } else {
                let trimmed = line.trim();
                if trimmed.is_empty() { continue; }
                if !accumulated.is_empty() {
                    accumulated.push_str(" ; ");
                }
                accumulated.push_str(trimmed);
            }
        }

        if !accumulated.is_empty() {
            execute_line_inner(&accumulated, None);
        }
        return;
    }

    execute_line_inner(input, None);
}

fn execute_line_with_heredoc(input: &str, heredoc: String) {
    let groups = split_commands(input);
    let mut last_ok = true;
    let last_idx = groups.len().saturating_sub(1);

    for (i, (cmd_str, _separator)) in groups.iter().enumerate() {
        if i > 0 {
            if let Some((_, Some(prev_sep))) = groups.get(i - 1) {
                match prev_sep {
                    Token::And => { if !last_ok { continue; } }
                    Token::Or => { if last_ok { continue; } }
                    _ => {}
                }
            }
        }

        let tokens = tokenize(cmd_str);
        if tokens.is_empty() { continue; }
        let mut pipeline = parse_pipeline(&tokens);
        if pipeline.is_empty() { continue; }

        // Attach heredoc to the first command of the last group
        if i == last_idx {
            pipeline[0].heredoc = Some(heredoc.clone());
        }

        last_ok = execute_pipeline(&pipeline);
    }
}

fn execute_line_inner(input: &str, _heredoc: Option<String>) {
    let groups = split_commands(input);
    let mut last_ok = true;

    for (i, (cmd_str, _separator)) in groups.iter().enumerate() {
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
        let is_first = i == 0;
        let is_last = i == pipeline.len() - 1;
        let Some(mut command) = build_command(cmd) else {
            println!("{}: not found", cmd.args[0]);
            return false;
        };

        if let Some(stdout) = prev_stdout.take() {
            command.stdin(stdout);
        } else if is_first {
            if let Some(ref path) = cmd.input_file {
                if let Ok(file) = File::open(path) {
                    command.stdin(file);
                }
            } else if cmd.heredoc.is_some() {
                command.stdin(Stdio::piped());
            }
        }

        if !is_last {
            command.stdout(Stdio::piped());
        } else {
            apply_redirect(&mut command, cmd);
        }

        match command.spawn() {
            Ok(mut child) => {
                // Write heredoc data to first command's stdin
                if is_first {
                    if let Some(ref data) = cmd.heredoc {
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(data.as_bytes());
                            drop(stdin);
                        }
                    }
                }
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
    } else if let Some(ref path) = cmd.input_file {
        match File::open(path) {
            Ok(file) => { command.stdin(file); }
            Err(e) => {
                println!("{}: {}", path, e);
                set_status_code(1);
                return false;
            }
        }
    } else if cmd.heredoc.is_some() {
        command.stdin(Stdio::piped());
    }
    apply_redirect(&mut command, cmd);

    if let Some(ref data) = cmd.heredoc {
        // Spawn, write heredoc to stdin, then wait
        match command.spawn() {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(data.as_bytes());
                    drop(stdin);
                }
                match child.wait() {
                    Ok(status) => { set_status(&status); return status.success(); }
                    Err(_) => { set_status_code(1); return false; }
                }
            }
            Err(_) => {
                println!("{}: not found", cmd.args[0]);
                set_status_code(127);
                return false;
            }
        }
    }

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

fn apply_redirect(command: &mut Command, cmd: &SimpleCommand) {
    match &cmd.redirect {
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
    match &cmd.stderr {
        Some(StderrRedirect::ToStdout) => {
            // 2>&1: stderr goes where stdout goes
            match &cmd.redirect {
                Some(Redirect::Truncate(path)) => {
                    if let Ok(file) = OpenOptions::new().write(true).open(path) {
                        command.stderr(file);
                    }
                }
                Some(Redirect::Append(path)) => {
                    if let Ok(file) = OpenOptions::new().append(true).open(path) {
                        command.stderr(file);
                    }
                }
                None => {
                    // stdout is inherited or piped — stderr follows
                    command.stderr(Stdio::inherit());
                }
            }
        }
        Some(StderrRedirect::ToFile(path)) => {
            if let Ok(file) = File::create(path) {
                command.stderr(file);
            }
        }
        Some(StderrRedirect::AppendFile(path)) => {
            if let Ok(file) = OpenOptions::new().create(true).append(true).open(path) {
                command.stderr(file);
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
            '\t' => {
                handle_tab(&mut line, &mut cursor);
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

const BUILTINS: &[&str] = &["cd", "clear", "exit", "export", "help", "true", "false"];

fn handle_tab(line: &mut String, cursor: &mut usize) {
    // Find the word being completed
    let before_cursor = &line[..char_to_byte(line, *cursor)];
    let word_start = before_cursor.rfind(|c: char| c == ' ' || c == '|' || c == ';' || c == '&').map_or(0, |i| i + 1);
    let prefix = &before_cursor[word_start..];
    let is_command = !before_cursor[..word_start].contains(|c: char| !c.is_whitespace() && c != '|' && c != ';' && c != '&')
        || word_start == 0;

    let matches = if is_command && !prefix.contains('/') {
        complete_command(prefix)
    } else {
        complete_path(prefix)
    };

    if matches.is_empty() {
        return;
    }

    if matches.len() == 1 {
        let completion = &matches[0];
        let suffix = &completion[prefix.len()..];
        // Check if completed item is a directory — add / if so, else space
        let trail = if is_command && !prefix.contains('/') {
            " "
        } else if Path::new(completion).is_dir() {
            "/"
        } else {
            " "
        };
        let insert = format!("{}{}", suffix, trail);
        let byte_pos = char_to_byte(line, *cursor);
        line.insert_str(byte_pos, &insert);
        *cursor += insert.chars().count();
        redraw_full(line, *cursor);
    } else {
        // Find common prefix
        let common = common_prefix(&matches);
        if common.len() > prefix.len() {
            let suffix = &common[prefix.len()..];
            let byte_pos = char_to_byte(line, *cursor);
            line.insert_str(byte_pos, suffix);
            *cursor += suffix.chars().count();
            redraw_full(line, *cursor);
        } else {
            // Show all matches
            term_echo(b"\n");
            let mut out = String::new();
            for m in &matches {
                out.push_str(m);
                out.push_str("  ");
            }
            term_echo(out.as_bytes());
            // Redraw prompt and line
            let cwd = env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| "?".into());
            let prompt = format!("\n{}> ", cwd);
            term_echo(prompt.as_bytes());
            redraw_full(line, *cursor);
        }
    }
}

fn complete_command(prefix: &str) -> Vec<String> {
    let mut matches: Vec<String> = Vec::new();

    // Builtins
    for &b in BUILTINS {
        if b.starts_with(prefix) {
            matches.push(b.to_string());
        }
    }

    // Executables in PATH
    let path = env::var("PATH").unwrap_or_else(|_| "/bin".into());
    for dir in path.split(':') {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(prefix) && !matches.contains(&name) {
                    matches.push(name);
                }
            }
        }
    }

    matches.sort();
    matches
}

fn complete_path(prefix: &str) -> Vec<String> {
    let (dir, file_prefix) = if let Some(slash) = prefix.rfind('/') {
        let dir = if slash == 0 { "/" } else { &prefix[..slash] };
        (dir.to_string(), &prefix[slash + 1..])
    } else {
        (".".to_string(), prefix)
    };

    let mut matches = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(file_prefix) {
                let full = if prefix.contains('/') {
                    let slash = prefix.rfind('/').unwrap();
                    format!("{}{}", &prefix[..slash + 1], name)
                } else {
                    name
                };
                matches.push(full);
            }
        }
    }

    matches.sort();
    matches
}

fn common_prefix(strings: &[String]) -> String {
    if strings.is_empty() { return String::new(); }
    let first = &strings[0];
    let mut len = first.len();
    for s in &strings[1..] {
        len = len.min(s.len());
        for (i, (a, b)) in first.bytes().zip(s.bytes()).enumerate() {
            if a != b {
                len = len.min(i);
                break;
            }
        }
    }
    first[..len].to_string()
}

fn redraw_full(line: &str, cursor: usize) {
    let char_count = line.chars().count();
    let mut buf = Vec::new();
    buf.push(b'\r');
    // Re-print current directory prompt
    let cwd = env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| "?".into());
    buf.extend_from_slice(format!("{}> ", cwd).as_bytes());
    buf.extend_from_slice(line.as_bytes());
    // Clear any trailing characters from previous content
    buf.extend_from_slice(b"\x1b[K");
    // Move cursor to correct position
    let back = char_count - cursor;
    for _ in 0..back {
        buf.push(0x08);
    }
    term_echo(&buf);
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

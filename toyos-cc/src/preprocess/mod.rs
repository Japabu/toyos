mod consteval;
mod expand;

use consteval::{ConstEval, IfState, has_unbalanced_parens, split_first_word};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::{fs, process};

#[derive(Clone)]
pub(crate) enum Macro {
    Object(Vec<PPToken>),
    Function(Vec<String>, bool, Vec<PPToken>), // params, variadic, body
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PPToken {
    Ident(String),
    Number(String),
    StringLit(String),
    CharLit(String),
    Punct(String),
    Whitespace,
    Hash,
    HashHash,
    /// Internal: marks the boundary between a macro's expanded body and the
    /// following input tokens.  When the expansion loop or argument collector
    /// encounters this token it removes `name` from the `expanding` set and
    /// discards the token (never emitted to output).
    Barrier(String),
}

/// Compiler's own headers, embedded into the binary.
const BUILTIN_HEADERS: &[(&str, &str)] = &[
    ("compat.h",      include_str!("../../include/compat.h")),
    ("float.h",       include_str!("../../include/float.h")),
    ("stdalign.h",    include_str!("../../include/stdalign.h")),
    ("stdarg.h",      include_str!("../../include/stdarg.h")),
    ("stdbool.h",     include_str!("../../include/stdbool.h")),
    ("stddef.h",      include_str!("../../include/stddef.h")),
    ("stdnoreturn.h", include_str!("../../include/stdnoreturn.h")),
];

fn builtin_header(name: &str) -> Option<&'static str> {
    BUILTIN_HEADERS.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

pub struct Preprocessor {
    pub(crate) macros: HashMap<String, Macro>,
    macro_stack: HashMap<String, Vec<Option<Macro>>>,
    include_paths: Vec<PathBuf>,
    output: String,
    pub(crate) expanding: HashSet<String>,
    pub(crate) file_stack: Vec<(String, u32, Option<usize>)>, // (file, line, include_dir_idx)
    pub(crate) counter: u32,
    pragma_once_files: HashSet<String>,
    pub suppress_line_markers: bool,
    pub force_includes: Vec<PathBuf>,
    // Tracks __LINE__ / __FILE__ when explicitly redefined by user source.
    // While in this set, expand_line skips the automatic per-line update.
    user_redefined_builtins: HashSet<String>,
    // Set to true while evaluating a #if / #elif constant expression so that
    // expand_tokens can handle the `defined` operator specially.
    pub(crate) in_if_eval: bool,
}

impl Preprocessor {
    pub fn new(include_paths: Vec<PathBuf>, defines: Vec<(String, String)>, target: Option<&str>) -> Self {
        let mut pp = Self {
            macros: HashMap::new(),
            macro_stack: HashMap::new(),
            include_paths,
            output: String::new(),
            expanding: HashSet::new(),
            file_stack: Vec::new(),
            counter: 0,
            pragma_once_files: HashSet::new(),
            suppress_line_markers: false,
            force_includes: Vec::new(),
            user_redefined_builtins: HashSet::new(),
            in_if_eval: false,
        };

        // Seed macros: primary arch and OS. Everything else is derived in compat.h.
        let is_toyos = target.map_or(false, |t| t.contains("toyos"));
        let is_macos = target.map_or(cfg!(target_os = "macos"), |t| t.contains("apple") || t.contains("darwin"));
        let is_aarch64 = target.map_or(cfg!(target_arch = "aarch64"), |t| t.starts_with("aarch64"));

        if is_aarch64 {
            pp.define_object("__aarch64__", "1");
        } else {
            pp.define_object("__x86_64__", "1");
        }

        if is_toyos {
            pp.define_object("__TOYOS__", "1");
        } else if is_macos {
            pp.define_object("__APPLE__", "1");
        } else {
            pp.define_object("__linux__", "1");
        }

        // Process embedded compat.h (defines all predefs derived from the seeds above)
        pp.process_source(builtin_header("compat.h").unwrap(), "<builtin>/compat.h", None);

        // __has_include needs compiler support (filesystem check)
        pp.define_object("__has_include", "1");
        pp.define_object("__has_include_next", "1");

        for (name, val) in defines {
            pp.define_object(&name, &val);
        }
        pp
    }

    fn define_object(&mut self, name: &str, value: &str) {
        let tokens = self.tokenize_pp(value);
        self.macros.insert(name.to_string(), Macro::Object(tokens));
    }

    pub fn preprocess(&mut self, source: &str, filename: &str) -> String {
        self.output.clear();
        for path in std::mem::take(&mut self.force_includes) {
            let directive = format!("#include \"{}\"\n", path.display());
            self.process_source(&directive, filename, None);
        }
        self.process_source(source, filename, None);
        std::mem::take(&mut self.output)
    }

    // Returns true if `s` (a raw source line) ends with an identifier that is
    // a defined function-like macro with no `(` following it on the line.
    fn ends_with_fn_macro(s: &str, macros: &HashMap<String, Macro>) -> bool {
        let bytes = s.trim_end().as_bytes();
        let end = bytes.len();
        let start = bytes[..end]
            .iter()
            .rposition(|&b| !b.is_ascii_alphanumeric() && b != b'_')
            .map(|p| p + 1)
            .unwrap_or(0);
        if start >= end { return false; }
        let last_word = match std::str::from_utf8(&bytes[start..end]) {
            Ok(w) => w,
            Err(_) => return false,
        };
        matches!(macros.get(last_word), Some(Macro::Function(..)))
    }

    fn process_source(&mut self, source: &str, filename: &str, include_dir_idx: Option<usize>) {
        self.file_stack.push((filename.to_string(), 0, include_dir_idx));
        self.emit_line_marker(filename, 1);

        let lines = self.split_logical_lines(source);

        let mut if_stack: Vec<IfState> = Vec::new();

        let mut pending_line = String::new();
        let mut skip_idx: Option<usize> = None;

        for (idx, line) in lines.iter().enumerate() {
            // When a line was consumed by joining with the previous line, skip it
            // but still advance the line counter.
            if skip_idx == Some(idx) {
                skip_idx = None;
                let advance = line.matches('\n').count() as u32 + 1;
                if let Some(last) = self.file_stack.last_mut() {
                    last.1 += advance;
                }
                continue;
            }

            let advance = line.matches('\n').count() as u32 + 1;
            if let Some(last) = self.file_stack.last_mut() {
                last.1 += advance;
            }

            // If we're accumulating a multi-line expression (unbalanced parens), handle it
            if !pending_line.is_empty() {
                let trimmed_check = line.trim();
                if trimmed_check.starts_with('#') {
                    // Directive inside multi-line expression.
                    // #include produces output that must appear after pending_line,
                    // so flush pending_line first. Other directives (conditionals,
                    // #define, etc.) don't produce output and can fall through.
                    let dt = trimmed_check[1..].trim_start();
                    let dir_end = dt.find(|c: char| !c.is_ascii_alphabetic() && c != '_').unwrap_or(dt.len());
                    let dir_name = &dt[..dir_end];

                    if self.is_active(&if_stack) && (dir_name == "include" || dir_name == "include_next") {
                        let expanded = self.expand_line(&pending_line);
                        self.output.push_str(&expanded);
                        self.output.push('\n');
                        pending_line.clear();
                    }
                    // Fall through to directive processing below
                } else if !self.is_active(&if_stack) {
                    // Inside inactive #if branch — skip this line
                    continue;
                } else {
                    pending_line.push(' ');
                    pending_line.push_str(trimmed_check);
                    if !has_unbalanced_parens(&pending_line) {
                        let expanded = self.expand_line(&pending_line);
                        self.output.push_str(&expanded);
                        self.output.push('\n');
                        pending_line.clear();
                    }
                    continue;
                }
            }

            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                let directive_text = &trimmed[1..].trim_start();
                // Split directive name from rest: directive names are alphabetic,
                // so `#include<stdio.h>` correctly splits into ("include", "<stdio.h>")
                let (directive, rest) = match directive_text.find(|c: char| !c.is_ascii_alphabetic() && c != '_') {
                    Some(0) | None => split_first_word(directive_text),
                    Some(pos) => (&directive_text[..pos], directive_text[pos..].trim_start()),
                };

                // Handle conditional compilation
                if !self.is_active(&if_stack) {
                    match directive {
                        "if" | "ifdef" | "ifndef" => {
                            if_stack.push(IfState { active: false, seen_true: true, parent_active: false });
                        }
                        "elif" => {
                            if let Some(state) = if_stack.last_mut() {
                                if !state.seen_true && state.parent_active {
                                    let val = self.eval_constant_expr(rest);
                                    if val != 0 {
                                        state.active = true;
                                        state.seen_true = true;
                                    }
                                }
                            }
                        }
                        "else" => {
                            if let Some(state) = if_stack.last_mut() {
                                if !state.seen_true && state.parent_active {
                                    state.active = true;
                                    state.seen_true = true;
                                }
                            }
                        }
                        "endif" => { if_stack.pop(); }
                        _ => {}
                    }
                    continue;
                }

                match directive {
                    "include" => self.handle_include(rest, filename),
                    "include_next" => self.handle_include_next(rest, filename),
                    "define" => self.handle_define(rest),
                    "undef" => {
                        let name = rest.split_whitespace().next()
                            .expect("#undef requires a macro name");
                        self.macros.remove(name);
                        self.user_redefined_builtins.remove(name);
                    }
                    "ifdef" => {
                        let name = rest.split_whitespace().next()
                            .expect("#ifdef requires a macro name");
                        let active = self.macros.contains_key(name);
                        if_stack.push(IfState { active, seen_true: active, parent_active: true });
                    }
                    "ifndef" => {
                        let name = rest.split_whitespace().next()
                            .expect("#ifndef requires a macro name");
                        let active = !self.macros.contains_key(name);
                        if_stack.push(IfState { active, seen_true: active, parent_active: true });
                    }
                    "if" => {
                        let val = self.eval_constant_expr(rest);
                        let active = val != 0;
                        if_stack.push(IfState { active, seen_true: active, parent_active: true });
                    }
                    "elif" => {
                        if let Some(state) = if_stack.last_mut() {
                            if state.seen_true {
                                state.active = false;
                            } else {
                                let val = self.eval_constant_expr(rest);
                                if val != 0 {
                                    state.active = true;
                                    state.seen_true = true;
                                } else {
                                    state.active = false;
                                }
                            }
                        }
                    }
                    "else" => {
                        if let Some(state) = if_stack.last_mut() {
                            if state.seen_true {
                                state.active = false;
                            } else {
                                state.active = true;
                                state.seen_true = true;
                            }
                        }
                    }
                    "endif" => { if_stack.pop(); }
                    "error" => {
                        let ln = self.file_stack.last().map(|(_,l,_)| *l).unwrap_or(0);
                        eprintln!("{}:{}: #error {}", filename, ln, rest);
                        process::exit(1);
                    }
                    "warning" => {
                        let ln = self.file_stack.last().map(|(_,l,_)| *l).unwrap_or(0);
                        eprintln!("{}:{}: #warning {}", filename, ln, rest);
                    }
                    "pragma" => {
                        let rest = rest.trim();
                        if rest.starts_with("push_macro") {
                            if let Some(name) = Self::extract_pragma_macro_name(rest) {
                                let saved = self.macros.get(&name).cloned();
                                self.macro_stack.entry(name).or_default().push(saved);
                            }
                        } else if rest.starts_with("pop_macro") {
                            if let Some(name) = Self::extract_pragma_macro_name(rest) {
                                if let Some(stack) = self.macro_stack.get_mut(&name) {
                                    if let Some(saved) = stack.pop() {
                                        if let Some(mac) = saved {
                                            self.macros.insert(name, mac);
                                        } else {
                                            self.macros.remove(&name);
                                        }
                                    }
                                }
                            }
                        } else if rest == "once" || rest.starts_with("once ") || rest.starts_with("once\t") {
                            if let Some((file, _, _)) = self.file_stack.last() {
                                self.pragma_once_files.insert(file.clone());
                            }
                        }
                        // Other pragmas ignored
                    }
                    "line" => {
                        let expanded = self.expand_line(rest);
                        let s = expanded.trim();
                        let n_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
                        if let Ok(n) = s[..n_end].parse::<u32>() {
                            let after_n = s[n_end..].trim();
                            if let Some(last) = self.file_stack.last_mut() {
                                last.1 = n.saturating_sub(1);
                                if after_n.starts_with('"') && after_n.ends_with('"') && after_n.len() >= 2 {
                                    last.0 = after_n[1..after_n.len()-1].to_string();
                                }
                            }
                        }
                    }
                    "" => { /* empty # line */ }
                    _ if filename.ends_with(".S") || filename.ends_with(".s")
                        || directive.chars().next().map_or(false, |c| c.is_ascii_digit()) =>
                    {
                        // Unknown directives in assembly files are silently ignored.
                        // Numeric directive names (# N or # N "file") are GCC line markers
                        // or assembly syntax — ignore those too.
                    }
                    _ => {
                        let ln = self.file_stack.last().map(|(_,l,_)| *l).unwrap_or(0);
                        panic!("{}:{}: unknown preprocessor directive #{}", filename, ln, directive);
                    }
                }

                // Emit line marker after include to restore position
                if directive == "include" || directive == "include_next" {
                    let cur_line = self.file_stack.last().map(|(_,l,_)| *l).unwrap_or(1);
                    self.emit_line_marker(filename, cur_line);
                }
            } else if self.is_active(&if_stack) {
                // Regular line - expand macros
                // If the line ends with a function-like macro name (no following `(`),
                // and the next line starts with `(`, join them so the macro can collect
                // its arguments across the line boundary.
                let effective = if let Some(next_line) = lines.get(idx + 1) {
                    let next_trimmed = next_line.trim();
                    if next_trimmed.starts_with('(') && Self::ends_with_fn_macro(trimmed, &self.macros) {
                        let next_advance = next_line.matches('\n').count() as u32 + 1;
                        if let Some(last) = self.file_stack.last_mut() {
                            last.1 += next_advance;
                        }
                        skip_idx = Some(idx + 1);
                        format!("{} {}", trimmed, next_trimmed)
                    } else {
                        trimmed.to_string()
                    }
                } else {
                    trimmed.to_string()
                };
                if has_unbalanced_parens(&effective) {
                    pending_line = effective;
                } else {
                    let expanded = self.expand_line(&effective);
                    self.output.push_str(&expanded);
                    self.output.push('\n');
                }
            }
        }

        self.file_stack.pop();
    }

    fn is_active(&self, if_stack: &[IfState]) -> bool {
        if_stack.iter().all(|s| s.active)
    }

    fn emit_line_marker(&mut self, file: &str, line: u32) {
        if self.suppress_line_markers { return; }
        self.output.push_str(&format!("# {} \"{}\"\n", line, file));
    }

    fn split_logical_lines(&self, source: &str) -> Vec<String> {
        // Phase 1: strip comments (replace with space), handle line continuations
        // Comments are stripped before line splitting per C standard translation phases
        let mut lines = Vec::new();
        let mut current = String::new();
        let bytes = source.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let ch = bytes[i];
            if ch == b'\\' && bytes.get(i + 1) == Some(&b'\n') {
                // Line continuation: skip backslash-newline but push a newline-equivalent
                // marker so that process_source counts __LINE__ correctly.
                // We use a rare whitespace char that tokenize_pp treats like a space.
                current.push('\n');
                i += 2; // skip line continuation
            } else if ch == b'/' && bytes.get(i + 1) == Some(&b'/') {
                // Line comment: skip to end of line
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            } else if ch == b'/' && bytes.get(i + 1) == Some(&b'*') {
                // Block comment: replace with space, count newlines
                current.push(' ');
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        i += 2;
                        break;
                    }
                    if bytes[i] == b'\n' {
                        // Emit newline so line counting stays correct
                        lines.push(std::mem::take(&mut current));
                    }
                    i += 1;
                }
            } else if ch == b'"' || ch == b'\'' {
                // String/char literal: pass through without comment detection
                let quote = ch;
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if i < bytes.len() { i += 1; }
                // Push raw bytes to preserve UTF-8
                current.push_str(std::str::from_utf8(&bytes[start..i]).unwrap_or_else(|_| {
                    std::str::from_utf8(&bytes[start..start + 1]).unwrap_or("?")
                }));
            } else if ch == b'\n' {
                lines.push(std::mem::take(&mut current));
                i += 1;
            } else {
                // Push raw byte(s) preserving UTF-8 multi-byte sequences
                if ch < 0x80 {
                    current.push(ch as char);
                    i += 1;
                } else {
                    // UTF-8 multi-byte: find the end of the sequence and push as str slice
                    let start = i;
                    i += 1;
                    while i < bytes.len() && bytes[i] & 0xC0 == 0x80 { i += 1; }
                    current.push_str(std::str::from_utf8(&bytes[start..i]).unwrap_or("\u{FFFD}"));
                }
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        lines
    }

    fn extract_pragma_macro_name(rest: &str) -> Option<String> {
        // Extract name from push_macro("name") or pop_macro("name")
        let start = rest.find('"')? + 1;
        let end = rest[start..].find('"')? + start;
        Some(rest[start..end].to_string())
    }

    fn handle_include(&mut self, arg: &str, current_file: &str) {
        let arg = arg.trim();
        let (path_str, is_system) = if arg.starts_with('"') {
            let end = arg[1..].find('"').unwrap_or(arg.len() - 1);
            (&arg[1..1 + end], false)
        } else if arg.starts_with('<') {
            let end = arg[1..].find('>').unwrap_or(arg.len() - 1);
            (&arg[1..1 + end], true)
        } else {
            // Macro-expanded include - try to evaluate
            let expanded = self.expand_line(arg);
            let trimmed = expanded.trim();
            if trimmed.starts_with('"') {
                let end = trimmed[1..].find('"').unwrap_or(trimmed.len() - 1);
                let path = trimmed[1..1 + end].to_string();
                if let Some((content, resolved, idx)) = self.find_and_read(&path, current_file, false) {
                    self.process_source(&content, &resolved, idx);
                }
                return;
            }
            panic!("cannot parse #include {}", arg);
        };

        if let Some((content, resolved, idx)) = self.find_and_read(path_str, current_file, is_system) {
            if self.pragma_once_files.contains(&resolved) {
                return;
            }
            self.process_source(&content, &resolved, idx);
        } else if is_system {
            eprintln!("fatal error: cannot find system include file: {}", path_str);
            std::process::exit(1);
        } else {
            eprintln!("fatal error: cannot find include file: {}", path_str);
            std::process::exit(1);
        }
    }

    fn handle_include_next(&mut self, arg: &str, _current_file: &str) {
        let arg = arg.trim();
        // Start searching from one past the include-dir index that provided the current file
        let current_idx = self.file_stack.last().and_then(|(_, _, idx)| *idx).unwrap_or(0);
        let start_idx = current_idx + 1;

        let (path_str, is_system) = if arg.starts_with('"') {
            let end = arg[1..].find('"').unwrap_or(arg.len() - 1);
            (&arg[1..1 + end], false)
        } else if arg.starts_with('<') {
            let end = arg[1..].find('>').unwrap_or(arg.len() - 1);
            (&arg[1..1 + end], true)
        } else {
            panic!("warning: cannot parse #include_next {}", arg);
        };

        if let Some((content, resolved, idx)) = self.find_and_read_next(path_str, start_idx) {
            if self.pragma_once_files.contains(&resolved) {
                return;
            }
            self.process_source(&content, &resolved, idx);
        } else if is_system {
            panic!("warning: cannot find #include_next file: {}", path_str);
        } else {
            panic!("cannot find #include_next file: {}", path_str);
        }
    }

    fn find_and_read(&self, path: &str, current_file: &str, is_system: bool) -> Option<(String, String, Option<usize>)> {
        // Builtin headers (embedded in the binary) take priority
        if let Some(content) = builtin_header(path) {
            return Some((content.to_string(), format!("<builtin>/{path}"), None));
        }
        if !is_system {
            // Search relative to current file first (not associated with any include-path index)
            let current_dir = Path::new(current_file).parent().unwrap_or(Path::new("."));
            let candidate = current_dir.join(path);
            if let Ok(content) = fs::read_to_string(&candidate) {
                let resolved = candidate.canonicalize().unwrap_or(candidate).to_string_lossy().to_string();
                return Some((content, resolved, None));
            }
        }
        // Search include paths, tracking the index for #include_next support
        for (i, dir) in self.include_paths.iter().enumerate() {
            let candidate = dir.join(path);
            if let Ok(content) = fs::read_to_string(&candidate) {
                let resolved = candidate.canonicalize().unwrap_or(candidate).to_string_lossy().to_string();
                return Some((content, resolved, Some(i)));
            }
        }
        None
    }

    fn find_and_read_next(&self, path: &str, start_idx: usize) -> Option<(String, String, Option<usize>)> {
        // For #include_next: skip include dirs up to (and including) start_idx - 1
        for (i, dir) in self.include_paths.iter().enumerate().skip(start_idx) {
            let candidate = dir.join(path);
            if let Ok(content) = fs::read_to_string(&candidate) {
                let resolved = candidate.canonicalize().unwrap_or(candidate).to_string_lossy().to_string();
                return Some((content, resolved, Some(i)));
            }
        }
        None
    }

    fn handle_define(&mut self, rest: &str) {
        let rest = rest.trim();
        let mut chars = rest.chars().peekable();

        // Read name
        let mut name = String::new();
        while chars.peek().is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$') {
            name.push(chars.next().unwrap());
        }

        if name.is_empty() { return; }

        // Check if function-like (no space before paren)
        if chars.peek() == Some(&'(') {
            chars.next(); // skip (
            let mut params = Vec::new();
            let mut variadic = false;
            loop {
                // Skip whitespace (including \n from line continuations)
                while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) { chars.next(); }
                if chars.peek() == Some(&')') { chars.next(); break; }
                if chars.peek() == Some(&'.') {
                    // ...
                    chars.next(); chars.next(); chars.next();
                    variadic = true;
                    while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) { chars.next(); }
                    if chars.peek() == Some(&')') { chars.next(); }
                    break;
                }
                let mut param = String::new();
                while chars.peek().is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$') {
                    param.push(chars.next().unwrap());
                }
                if param == "..." {
                    variadic = true;
                    while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) { chars.next(); }
                    if chars.peek() == Some(&')') { chars.next(); }
                    break;
                }
                if !param.is_empty() {
                    params.push(param);
                }
                while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) { chars.next(); }
                if chars.peek() == Some(&',') { chars.next(); }
            }

            // Rest is the body
            let body_str: String = chars.collect();
            let body = Self::strip_ws_around_hashhash(self.tokenize_pp(body_str.trim()));
            self.warn_redefine(&name, &Macro::Function(params.clone(), variadic, body.clone()));
            self.macros.insert(name, Macro::Function(params, variadic, body));
        } else {
            // Object-like
            let body_str: String = chars.collect();
            let body = Self::strip_ws_around_hashhash(self.tokenize_pp(body_str.trim()));
            self.warn_redefine(&name, &Macro::Object(body.clone()));
            if matches!(name.as_str(), "__LINE__" | "__FILE__") {
                self.user_redefined_builtins.insert(name.clone());
            }
            self.macros.insert(name, Macro::Object(body));
        }
    }

    fn warn_redefine(&self, name: &str, new_mac: &Macro) {
        // Never warn when the user redefines a predefined builtin like __LINE__ or __FILE__.
        if matches!(name, "__LINE__" | "__FILE__") { return; }
        let Some(old_mac) = self.macros.get(name) else { return };
        let same = match (old_mac, new_mac) {
            (Macro::Object(a), Macro::Object(b)) => a == b,
            (Macro::Function(ap, av, ab), Macro::Function(bp, bv, bb)) => {
                ap == bp && av == bv && ab == bb
            }
            _ => false,
        };
        if !same {
            let (file, line) = self.file_stack.last().map(|(f, l, _)| (f.as_str(), *l)).unwrap_or(("", 0));
            eprintln!("{}:{}: warning: {} redefined", file, line, name);
        }
    }

    fn expand_line(&mut self, line: &str) -> String {
        // Update __FILE__ and __LINE__ before expansion, unless the user has
        // explicitly redefined them with their own #define.
        if let Some((file, line_num, _)) = self.file_stack.last() {
            let file_val = format!("\"{}\"", file);
            let line_val = line_num.to_string();
            if !self.user_redefined_builtins.contains("__FILE__") {
                self.define_object("__FILE__", &file_val);
            }
            if !self.user_redefined_builtins.contains("__LINE__") {
                self.define_object("__LINE__", &line_val);
            }
        }
        let tokens = self.tokenize_pp(line);
        let expanded = self.expand_tokens(&tokens);
        self.tokens_to_string(&expanded)
    }

    fn eval_constant_expr(&mut self, expr: &str) -> i64 {
        // Replace __has_include("file") / __has_include(<file>) with 0 or 1
        let with_has_include = self.replace_has_include(expr);
        // Expand macros with in_if_eval=true so `defined` is handled as a
        // keyword inside macro bodies too (handles defined-after-expansion).
        // Other predicates (__has_builtin etc.) are function-like macros from
        // compat.h that expand to 0 during normal macro expansion.
        self.in_if_eval = true;
        let expanded = self.expand_line(&with_has_include);
        self.in_if_eval = false;
        // Then evaluate
        let mut eval = ConstEval::new(&expanded);
        eval.expr()
    }


    fn replace_has_include(&self, expr: &str) -> String {
        let mut result = String::new();
        let bytes = expr.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // Check for __has_include or __has_include_next
            let is_next = i + 19 <= bytes.len() && &bytes[i..i+19] == b"__has_include_next(";
            let is_has = !is_next && i + 14 <= bytes.len() && &bytes[i..i+14] == b"__has_include(";
            if is_has || is_next {
                let start = if is_next { i + 19 } else { i + 14 };
                // find closing paren
                let mut j = start;
                let mut depth = 1;
                while j < bytes.len() && depth > 0 {
                    if bytes[j] == b'(' { depth += 1; }
                    if bytes[j] == b')' { depth -= 1; }
                    if depth > 0 { j += 1; }
                }
                let arg = std::str::from_utf8(&bytes[start..j]).unwrap_or("").trim();
                // Extract path from "file" or <file>
                let path = if arg.starts_with('"') && arg.ends_with('"') {
                    &arg[1..arg.len()-1]
                } else if arg.starts_with('<') && arg.ends_with('>') {
                    &arg[1..arg.len()-1]
                } else {
                    // Might be a macro — expand it first
                    let expanded = self.tokens_to_string(&self.expand_tokens_const(&self.tokenize_pp(arg)));
                    let trimmed = expanded.trim().to_string();
                    let path = if trimmed.starts_with('"') && trimmed.ends_with('"') {
                        trimmed[1..trimmed.len()-1].to_string()
                    } else {
                        trimmed
                    };
                    let current_file = self.file_stack.last().map(|(f,_,_)| f.as_str()).unwrap_or("");
                    let found = self.find_and_read(&path, current_file, false).is_some();
                    result.push_str(if found { "1" } else { "0" });
                    i = j + 1;
                    continue;
                };
                let current_file = self.file_stack.last().map(|(f,_,_)| f.as_str()).unwrap_or("");
                let found = self.find_and_read(path, current_file, arg.starts_with('<')).is_some();
                result.push_str(if found { "1" } else { "0" });
                i = j + 1;
            } else {
                result.push(bytes[i] as char);
                i += 1;
            }
        }
        result
    }

}

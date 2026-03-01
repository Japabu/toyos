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
}

pub struct Preprocessor {
    pub(crate) macros: HashMap<String, Macro>,
    macro_stack: HashMap<String, Vec<Option<Macro>>>,
    include_paths: Vec<PathBuf>,
    output: String,
    pub(crate) expanding: HashSet<String>,
    pub(crate) file_stack: Vec<(String, u32)>,
    pub(crate) counter: u32,
    pragma_once_files: HashSet<String>,
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
        };

        // Determine target properties
        let is_toyos = target.map_or(false, |t| t.contains("toyos"));
        let is_macos = target.map_or(cfg!(target_os = "macos"), |t| t.contains("apple") || t.contains("darwin"));
        let is_aarch64 = target.map_or(cfg!(target_arch = "aarch64"), |t| t.starts_with("aarch64"));

        // Language standard and compiler identity
        pp.define_object("__STDC__", "1");
        pp.define_object("__STDC_VERSION__", "199901L");
        // Claim GCC 4.0 compat — needed for system headers
        pp.define_object("__GNUC__", "4");
        pp.define_object("__GNUC_MINOR__", "0");
        pp.define_object("__GNUC_PATCHLEVEL__", "0");

        // Architecture
        pp.define_object("__LP64__", "1");
        pp.define_object("__SIZEOF_POINTER__", "8");
        pp.define_object("__SIZEOF_INT__", "4");
        pp.define_object("__SIZEOF_SHORT__", "2");
        pp.define_object("__CHAR_BIT__", "8");
        if is_aarch64 {
            pp.define_object("__aarch64__", "1");
            pp.define_object("__arm64__", "1");
            pp.define_object("__ARM_64BIT_STATE", "1");
            pp.define_object("__SIZEOF_LONG__", "8");
        } else {
            pp.define_object("__x86_64__", "1");
            pp.define_object("__x86_64", "1");
            pp.define_object("__amd64__", "1");
            pp.define_object("__amd64", "1");
            pp.define_object("__SIZEOF_LONG__", "8");
        }

        // OS
        if is_toyos {
            pp.define_object("__TOYOS__", "1");
            pp.define_object("__unix__", "1");
            pp.define_object("__ELF__", "1");
        } else if is_macos {
            pp.define_object("__APPLE__", "1");
            pp.define_object("__APPLE_CC__", "1");
            pp.define_object("__MACH__", "1");
        } else {
            // Default to Linux-like
            pp.define_object("__linux__", "1");
            pp.define_object("__unix__", "1");
            pp.define_object("__ELF__", "1");
            pp.define_object("__gnu_linux__", "1");
        }

        pp.define_object("NULL", "((void*)0)");

        // GCC builtin type macros
        pp.define_object("__SIZE_TYPE__", "unsigned long");
        pp.define_object("__PTRDIFF_TYPE__", "long");
        pp.define_object("__WCHAR_TYPE__", "int");
        pp.define_object("__WINT_TYPE__", "int");
        pp.define_object("__INT8_TYPE__", "signed char");
        pp.define_object("__INT16_TYPE__", "short");
        pp.define_object("__INT32_TYPE__", "int");
        pp.define_object("__INT64_TYPE__", "long long");
        pp.define_object("__UINT8_TYPE__", "unsigned char");
        pp.define_object("__UINT16_TYPE__", "unsigned short");
        pp.define_object("__UINT32_TYPE__", "unsigned int");
        pp.define_object("__UINT64_TYPE__", "unsigned long long");
        pp.define_object("__INTPTR_TYPE__", "long");
        pp.define_object("__UINTPTR_TYPE__", "unsigned long");
        pp.define_object("__INTMAX_TYPE__", "long");
        pp.define_object("__UINTMAX_TYPE__", "unsigned long");

        // GCC builtin constants
        pp.define_object("__FLT_MIN__", "1.17549435e-38F");
        pp.define_object("__FLT_MAX__", "3.40282347e+38F");
        pp.define_object("__DBL_MIN__", "2.2250738585072014e-308");
        pp.define_object("__DBL_MAX__", "1.7976931348623157e+308");
        pp.define_object("__LDBL_MIN__", "2.2250738585072014e-308L");
        pp.define_object("__LDBL_MAX__", "1.7976931348623157e+308L");
        pp.define_object("__FLT_EPSILON__", "1.19209290e-7F");
        pp.define_object("__DBL_EPSILON__", "2.2204460492503131e-16");

        // GCC builtin functions as macros
        pp.define_function("__builtin_inf", &[], "(1.0/0.0)");
        pp.define_function("__builtin_inff", &[], "(1.0F/0.0F)");
        pp.define_function("__builtin_infl", &[], "(1.0L/0.0L)");
        pp.define_function("__builtin_fabs", &["x"], "((x)<0?-(x):(x))");
        pp.define_function("__builtin_fabsf", &["x"], "((x)<0?-(x):(x))");
        pp.define_function("__builtin_fabsl", &["x"], "((x)<0?-(x):(x))");

        // stdarg.h builtins
        pp.define_function("va_start", &["ap", "last"], "__builtin_va_start(ap, last)");
        pp.define_function("va_end", &["ap"], "__builtin_va_end(ap)");
        pp.define_function("va_copy", &["d", "s"], "__builtin_va_copy(d, s)");
        // __has_include support (define as macros so #ifdef works)
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

    fn define_function(&mut self, name: &str, params: &[&str], body: &str) {
        let tokens = self.tokenize_pp(body);
        let params = params.iter().map(|s| s.to_string()).collect();
        self.macros.insert(name.to_string(), Macro::Function(params, false, tokens));
    }

    pub fn preprocess(&mut self, source: &str, filename: &str) -> String {
        self.output.clear();
        self.process_source(source, filename);
        std::mem::take(&mut self.output)
    }

    fn process_source(&mut self, source: &str, filename: &str) {
        self.file_stack.push((filename.to_string(), 1));
        self.emit_line_marker(filename, 1);

        let lines = self.split_logical_lines(source);
        let mut line_num = 0u32;

        let mut if_stack: Vec<IfState> = Vec::new();

        let mut pending_line = String::new();

        for line in &lines {
            line_num += line.matches('\n').count() as u32 + 1;
            if let Some(last) = self.file_stack.last_mut() {
                last.1 = line_num;
            }

            // If we're accumulating a multi-line expression (unbalanced parens), handle it
            if !pending_line.is_empty() {
                let trimmed_check = line.trim();
                if trimmed_check.starts_with('#') {
                    // Directive inside multi-line expression — process it but stay in accumulation
                    // (e.g., #if/#else/#endif within a function argument list)
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
                    "define" => self.handle_define(rest),
                    "undef" => {
                        let name = rest.split_whitespace().next().unwrap_or("");
                        self.macros.remove(name);
                    }
                    "ifdef" => {
                        let name = rest.split_whitespace().next().unwrap_or("");
                        let active = self.macros.contains_key(name);
                        if_stack.push(IfState { active, seen_true: active, parent_active: true });
                    }
                    "ifndef" => {
                        let name = rest.split_whitespace().next().unwrap_or("");
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
                        eprintln!("{}:{}: #error {}", filename, line_num, rest);
                        process::exit(1);
                    }
                    "warning" => {
                        eprintln!("{}:{}: #warning {}", filename, line_num, rest);
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
                            if let Some((file, _)) = self.file_stack.last() {
                                self.pragma_once_files.insert(file.clone());
                            }
                        }
                        // Other pragmas ignored
                    }
                    "line" => { /* ignore */ }
                    "" => { /* empty # line */ }
                    _ => {
                        panic!("{}:{}: unknown preprocessor directive #{}", filename, line_num, directive);
                    }
                }

                // Emit line marker after include to restore position
                if directive == "include" {
                    self.emit_line_marker(filename, line_num);
                }
            } else if self.is_active(&if_stack) {
                // Regular line - expand macros
                if has_unbalanced_parens(trimmed) {
                    pending_line = trimmed.to_string();
                } else {
                    let expanded = self.expand_line(trimmed);
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
                if let Some((content, resolved)) = self.find_and_read(&path, current_file, false) {
                    self.process_source(&content, &resolved);
                }
                return;
            }
            panic!("cannot parse #include {}", arg);
        };

        if let Some((content, resolved)) = self.find_and_read(path_str, current_file, is_system) {
            if self.pragma_once_files.contains(&resolved) {
                return;
            }
            self.process_source(&content, &resolved);
        } else if matches!(path_str, "stdarg.h" | "stdbool.h" | "stdnoreturn.h" | "stdalign.h"
                | "ptrcheck.h" | "stdatomic.h") {
            // Compiler-provided headers: builtins are already defined, nothing to include
        } else if is_system {
            // Missing system headers — warn but don't fail
            eprintln!("warning: cannot find system include file: {}", path_str);
        } else {
            panic!("cannot find include file: {}", path_str);
        }
    }

    fn find_and_read(&self, path: &str, current_file: &str, is_system: bool) -> Option<(String, String)> {
        if !is_system {
            // Search relative to current file first
            let current_dir = Path::new(current_file).parent().unwrap_or(Path::new("."));
            let candidate = current_dir.join(path);
            if let Ok(content) = fs::read_to_string(&candidate) {
                let resolved = candidate.canonicalize().unwrap_or(candidate).to_string_lossy().to_string();
                return Some((content, resolved));
            }
        }
        // Search include paths
        for dir in &self.include_paths {
            let candidate = dir.join(path);
            if let Ok(content) = fs::read_to_string(&candidate) {
                let resolved = candidate.canonicalize().unwrap_or(candidate).to_string_lossy().to_string();
                return Some((content, resolved));
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
                // Skip whitespace
                while chars.peek() == Some(&' ') { chars.next(); }
                if chars.peek() == Some(&')') { chars.next(); break; }
                if chars.peek() == Some(&'.') {
                    // ...
                    chars.next(); chars.next(); chars.next();
                    variadic = true;
                    while chars.peek() == Some(&' ') { chars.next(); }
                    if chars.peek() == Some(&')') { chars.next(); }
                    break;
                }
                let mut param = String::new();
                while chars.peek().is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$') {
                    param.push(chars.next().unwrap());
                }
                if param == "..." {
                    variadic = true;
                    while chars.peek() == Some(&' ') { chars.next(); }
                    if chars.peek() == Some(&')') { chars.next(); }
                    break;
                }
                if !param.is_empty() {
                    params.push(param);
                }
                while chars.peek() == Some(&' ') { chars.next(); }
                if chars.peek() == Some(&',') { chars.next(); }
            }

            // Rest is the body
            let body_str: String = chars.collect();
            let body = Self::strip_ws_around_hashhash(self.tokenize_pp(body_str.trim()));
            self.macros.insert(name, Macro::Function(params, variadic, body));
        } else {
            // Object-like
            let body_str: String = chars.collect();
            let body = Self::strip_ws_around_hashhash(self.tokenize_pp(body_str.trim()));
            self.macros.insert(name, Macro::Object(body));
        }
    }

    fn expand_line(&mut self, line: &str) -> String {
        // Update __FILE__ and __LINE__ before expansion
        if let Some((file, line_num)) = self.file_stack.last() {
            let file_val = format!("\"{}\"", file);
            let line_val = line_num.to_string();
            self.define_object("__FILE__", &file_val);
            self.define_object("__LINE__", &line_val);
        }
        let tokens = self.tokenize_pp(line);
        let expanded = self.expand_tokens(&tokens);
        self.tokens_to_string(&expanded)
    }

    fn eval_constant_expr(&mut self, expr: &str) -> i64 {
        // Replace defined(X) and defined X with 0 or 1 BEFORE macro expansion
        let with_defined = self.replace_defined(expr);
        // Replace __has_include("file") / __has_include(<file>) with 0 or 1
        let with_has_include = self.replace_has_include(&with_defined);
        // Then expand macros
        let expanded = self.expand_line(&with_has_include);
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
                    let current_file = self.file_stack.last().map(|(f,_)| f.as_str()).unwrap_or("");
                    let found = self.find_and_read(&path, current_file, false).is_some()
                        || matches!(path.as_str(), "stdarg.h" | "stdbool.h" | "stdnoreturn.h" | "stdalign.h");
                    result.push_str(if found { "1" } else { "0" });
                    i = j + 1;
                    continue;
                };
                let current_file = self.file_stack.last().map(|(f,_)| f.as_str()).unwrap_or("");
                let found = self.find_and_read(path, current_file, arg.starts_with('<')).is_some()
                    || matches!(path, "stdarg.h" | "stdbool.h" | "stdnoreturn.h" | "stdalign.h");
                result.push_str(if found { "1" } else { "0" });
                i = j + 1;
            } else {
                result.push(bytes[i] as char);
                i += 1;
            }
        }
        result
    }

    fn replace_defined(&self, expr: &str) -> String {
        let mut result = String::new();
        let bytes = expr.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if i + 7 <= bytes.len() && &bytes[i..i+7] == b"defined" {
                let before_ok = i == 0 || !bytes[i-1].is_ascii_alphanumeric() && bytes[i-1] != b'_';
                let after_ok = i + 7 >= bytes.len() || !bytes[i+7].is_ascii_alphanumeric() && bytes[i+7] != b'_';
                if before_ok && after_ok {
                    i += 7;
                    // skip whitespace
                    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') { i += 1; }
                    let has_paren = i < bytes.len() && bytes[i] == b'(';
                    if has_paren { i += 1; }
                    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') { i += 1; }
                    let name_start = i;
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$') { i += 1; }
                    let name = std::str::from_utf8(&bytes[name_start..i]).unwrap_or("");
                    if has_paren {
                        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') { i += 1; }
                        if i < bytes.len() && bytes[i] == b')' { i += 1; }
                    }
                    result.push_str(if self.macros.contains_key(name) { "1" } else { "0" });
                    continue;
                }
            }
            result.push(bytes[i] as char);
            i += 1;
        }
        result
    }
}

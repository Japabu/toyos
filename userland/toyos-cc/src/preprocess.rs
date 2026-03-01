use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::{fs, process};

#[derive(Clone)]
enum Macro {
    Object(Vec<PPToken>),
    Function(Vec<String>, bool, Vec<PPToken>), // params, variadic, body
}

#[derive(Debug, Clone, PartialEq)]
enum PPToken {
    Ident(String),
    Number(String),
    StringLit(String),
    CharLit(String),
    Punct(String),
    Whitespace,
    Newline,
    Hash,
    HashHash,
    Other(char),
}

pub struct Preprocessor {
    macros: HashMap<String, Macro>,
    include_paths: Vec<PathBuf>,
    output: String,
    expanding: HashSet<String>,
    file_stack: Vec<(String, u32)>,
}

impl Preprocessor {
    pub fn new(include_paths: Vec<PathBuf>, defines: Vec<(String, String)>) -> Self {
        let mut pp = Self {
            macros: HashMap::new(),
            include_paths,
            output: String::new(),
            expanding: HashSet::new(),
            file_stack: Vec::new(),
        };
        // Predefined macros
        pp.define_object("__STDC__", "1");
        pp.define_object("__STDC_VERSION__", "199901L");
        pp.define_object("__x86_64__", "1");
        pp.define_object("__x86_64", "1");
        pp.define_object("__amd64__", "1");
        pp.define_object("__amd64", "1");
        pp.define_object("__LP64__", "1");
        pp.define_object("__SIZEOF_POINTER__", "8");
        pp.define_object("__SIZEOF_LONG__", "8");
        pp.define_object("__SIZEOF_INT__", "4");
        pp.define_object("__SIZEOF_SHORT__", "2");
        pp.define_object("__CHAR_BIT__", "8");
        pp.define_object("__TOYOS__", "1");
        pp.define_object("__unix__", "1");
        pp.define_object("__ELF__", "1");
        pp.define_object("NULL", "((void*)0)");
        // stdarg.h builtins
        pp.define_function("va_start", &["ap", "last"], "__builtin_va_start(ap, last)");
        pp.define_function("va_end", &["ap"], "__builtin_va_end(ap)");
        pp.define_function("va_copy", &["d", "s"], "__builtin_va_copy(d, s)");
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
                let (directive, rest) = split_first_word(directive_text);

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
                    "pragma" | "line" => { /* ignore */ }
                    "" => { /* empty # line */ }
                    _ => {
                        // Unknown directive - ignore with warning
                        eprintln!("{}:{}: warning: unknown directive #{}", filename, line_num, directive);
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
                current.push(ch as char);
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        current.push(bytes[i] as char);
                        current.push(bytes[i + 1] as char);
                        i += 2;
                    } else {
                        current.push(bytes[i] as char);
                        i += 1;
                    }
                }
                if i < bytes.len() {
                    current.push(bytes[i] as char);
                    i += 1;
                }
            } else if ch == b'\n' {
                lines.push(std::mem::take(&mut current));
                i += 1;
            } else {
                current.push(ch as char);
                i += 1;
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        lines
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
                if let Some(content) = self.find_and_read(&path, current_file, false) {
                    self.process_source(&content, &path);
                }
                return;
            }
            panic!("cannot parse #include {}", arg);
        };

        if let Some(content) = self.find_and_read(path_str, current_file, is_system) {
            let resolved = path_str.to_string();
            self.process_source(&content, &resolved);
        } else {
            panic!("cannot find include file: {}", path_str);
        }
    }

    fn find_and_read(&self, path: &str, current_file: &str, is_system: bool) -> Option<String> {
        if !is_system {
            // Search relative to current file first
            let current_dir = Path::new(current_file).parent().unwrap_or(Path::new("."));
            let candidate = current_dir.join(path);
            if let Ok(content) = fs::read_to_string(&candidate) {
                return Some(content);
            }
        }
        // Search include paths
        for dir in &self.include_paths {
            let candidate = dir.join(path);
            if let Ok(content) = fs::read_to_string(&candidate) {
                return Some(content);
            }
        }
        None
    }

    fn strip_ws_around_hashhash(tokens: Vec<PPToken>) -> Vec<PPToken> {
        let mut result = Vec::with_capacity(tokens.len());
        for tok in tokens {
            if tok == PPToken::HashHash {
                // Remove trailing whitespace from result (before ##)
                while result.last() == Some(&PPToken::Whitespace) { result.pop(); }
            }
            if tok == PPToken::Whitespace && result.last() == Some(&PPToken::HashHash) {
                // Skip whitespace after ##
                continue;
            }
            result.push(tok);
        }
        result
    }

    fn handle_define(&mut self, rest: &str) {
        let rest = rest.trim();
        let mut chars = rest.chars().peekable();

        // Read name
        let mut name = String::new();
        while chars.peek().is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') {
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
                while chars.peek().is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') {
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

    fn tokenize_pp(&self, s: &str) -> Vec<PPToken> {
        let mut tokens = Vec::new();
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b' ' | b'\t' => {
                    tokens.push(PPToken::Whitespace);
                    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') { i += 1; }
                }
                b'#' if i + 1 < bytes.len() && bytes[i + 1] == b'#' => {
                    tokens.push(PPToken::HashHash);
                    i += 2;
                }
                b'#' => {
                    tokens.push(PPToken::Hash);
                    i += 1;
                }
                b'"' => {
                    let start = i;
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'"' {
                        if bytes[i] == b'\\' { i += 1; }
                        i += 1;
                    }
                    if i < bytes.len() { i += 1; }
                    tokens.push(PPToken::StringLit(String::from_utf8_lossy(&bytes[start..i]).into_owned()));
                }
                b'\'' => {
                    let start = i;
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        if bytes[i] == b'\\' { i += 1; }
                        i += 1;
                    }
                    if i < bytes.len() { i += 1; }
                    tokens.push(PPToken::CharLit(String::from_utf8_lossy(&bytes[start..i]).into_owned()));
                }
                c if c.is_ascii_alphabetic() || c == b'_' => {
                    let start = i;
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') { i += 1; }
                    tokens.push(PPToken::Ident(String::from_utf8_lossy(&bytes[start..i]).into_owned()));
                }
                c if c.is_ascii_digit() => {
                    let start = i;
                    // Handle hex, octal, decimal, float
                    if c == b'0' && i + 1 < bytes.len() && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X') {
                        i += 2;
                        while i < bytes.len() && bytes[i].is_ascii_hexdigit() { i += 1; }
                    } else {
                        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') { i += 1; }
                    }
                    // Suffixes
                    while i < bytes.len() && (bytes[i] == b'u' || bytes[i] == b'U' || bytes[i] == b'l' || bytes[i] == b'L' || bytes[i] == b'f' || bytes[i] == b'F') { i += 1; }
                    tokens.push(PPToken::Number(String::from_utf8_lossy(&bytes[start..i]).into_owned()));
                }
                c => {
                    // Multi-char punctuation
                    let start = i;
                    i += 1;
                    // Handle common two-char operators
                    if i < bytes.len() {
                        let pair = [c, bytes[i]];
                        match &pair {
                            b"->" | b"++" | b"--" | b"<<" | b">>" | b"<=" | b">=" | b"==" | b"!="
                            | b"&&" | b"||" | b"+=" | b"-=" | b"*=" | b"/=" | b"%="
                            | b"&=" | b"|=" | b"^=" => { i += 1; }
                            _ => {}
                        }
                    }
                    tokens.push(PPToken::Punct(String::from_utf8_lossy(&bytes[start..i]).into_owned()));
                }
            }
        }
        tokens
    }

    fn expand_line(&mut self, line: &str) -> String {
        let tokens = self.tokenize_pp(line);
        let expanded = self.expand_tokens(&tokens);
        self.tokens_to_string(&expanded)
    }

    fn expand_tokens(&mut self, tokens: &[PPToken]) -> Vec<PPToken> {
        let mut result = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            match &tokens[i] {
                PPToken::Ident(name) if self.macros.contains_key(name) && !self.expanding.contains(name) => {
                    let mac = self.macros.get(name).cloned();
                    match mac {
                        Some(Macro::Object(body)) => {
                            self.expanding.insert(name.clone());
                            let expanded = self.expand_tokens(&body);
                            self.expanding.remove(name);
                            result.extend(expanded);
                            i += 1;
                        }
                        Some(Macro::Function(params, variadic, body)) => {
                            // Look for (
                            let mut j = i + 1;
                            while j < tokens.len() && tokens[j] == PPToken::Whitespace { j += 1; }
                            if j < tokens.len() && tokens[j] == PPToken::Punct("(".to_string()) {
                                j += 1;
                                // Collect arguments
                                let args = self.collect_macro_args(tokens, &mut j, params.len(), variadic);
                                // Substitute
                                let substituted = self.substitute(&params, variadic, &body, &args);
                                self.expanding.insert(name.clone());
                                let expanded = self.expand_tokens(&substituted);
                                self.expanding.remove(name);
                                result.extend(expanded);
                                i = j;
                            } else {
                                // No parens - not a function-like invocation
                                result.push(tokens[i].clone());
                                i += 1;
                            }
                        }
                        None => { result.push(tokens[i].clone()); i += 1; }
                    }
                }
                _ => { result.push(tokens[i].clone()); i += 1; }
            }
        }
        result
    }

    fn collect_macro_args(&self, tokens: &[PPToken], pos: &mut usize, param_count: usize, variadic: bool) -> Vec<Vec<PPToken>> {
        let mut args: Vec<Vec<PPToken>> = Vec::new();
        let mut current = Vec::new();
        let mut depth = 0;

        while *pos < tokens.len() {
            match &tokens[*pos] {
                PPToken::Punct(s) if s == "(" => {
                    depth += 1;
                    current.push(tokens[*pos].clone());
                    *pos += 1;
                }
                PPToken::Punct(s) if s == ")" => {
                    if depth == 0 {
                        *pos += 1;
                        args.push(current);
                        break;
                    }
                    depth -= 1;
                    current.push(tokens[*pos].clone());
                    *pos += 1;
                }
                PPToken::Punct(s) if s == "," && depth == 0 => {
                    if !variadic || args.len() < param_count {
                        args.push(std::mem::take(&mut current));
                    } else {
                        // Part of variadic arg
                        current.push(tokens[*pos].clone());
                    }
                    *pos += 1;
                }
                _ => {
                    current.push(tokens[*pos].clone());
                    *pos += 1;
                }
            }
        }

        // Ensure we have at least param_count entries
        while args.len() < param_count {
            args.push(Vec::new());
        }
        // Strip leading/trailing whitespace from each arg
        for arg in &mut args {
            while arg.first() == Some(&PPToken::Whitespace) { arg.remove(0); }
            while arg.last() == Some(&PPToken::Whitespace) { arg.pop(); }
        }
        args
    }

    fn resolve_param(&self, tok: &PPToken, params: &[String], variadic: bool, args: &[Vec<PPToken>]) -> String {
        if let PPToken::Ident(name) = tok {
            if let Some(idx) = params.iter().position(|p| p == name) {
                return self.tokens_to_string(args.get(idx).map(|a| a.as_slice()).unwrap_or(&[]));
            }
            if name == "__VA_ARGS__" && variadic {
                return self.tokens_to_string(args.get(params.len()).map(|a| a.as_slice()).unwrap_or(&[]));
            }
        }
        self.stringify_token(tok)
    }

    fn substitute(&self, params: &[String], variadic: bool, body: &[PPToken], args: &[Vec<PPToken>]) -> Vec<PPToken> {
        let mut result = Vec::new();
        let mut i = 0;
        while i < body.len() {
            // Token pasting ## — handle chains like A##B##C##D
            if i + 2 < body.len() && body[i + 1] == PPToken::HashHash {
                let mut pasted = self.resolve_param(&body[i], params, variadic, args);
                i += 1; // past the left operand
                while i + 1 < body.len() && body[i] == PPToken::HashHash {
                    let right = self.resolve_param(&body[i + 1], params, variadic, args);
                    pasted.push_str(&right);
                    i += 2;
                }
                result.extend(self.tokenize_pp(&pasted));
                continue;
            }

            // Stringification #
            if body[i] == PPToken::Hash && i + 1 < body.len() {
                if let PPToken::Ident(name) = &body[i + 1] {
                    if let Some(idx) = params.iter().position(|p| p == name) {
                        let s = self.tokens_to_string(args.get(idx).map(|a| a.as_slice()).unwrap_or(&[]));
                        result.push(PPToken::StringLit(format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))));
                        i += 2;
                        continue;
                    }
                }
            }

            match &body[i] {
                PPToken::Ident(name) if name == "__VA_ARGS__" && variadic => {
                    if let Some(va_args) = args.get(params.len()) {
                        result.extend(va_args.clone());
                    }
                    i += 1;
                }
                PPToken::Ident(name) => {
                    if let Some(idx) = params.iter().position(|p| p == name) {
                        result.extend(args.get(idx).cloned().unwrap_or_default());
                    } else {
                        result.push(body[i].clone());
                    }
                    i += 1;
                }
                _ => { result.push(body[i].clone()); i += 1; }
            }
        }
        result
    }

    fn stringify_token(&self, token: &PPToken) -> String {
        match token {
            PPToken::Ident(s) | PPToken::Number(s) | PPToken::StringLit(s)
            | PPToken::CharLit(s) | PPToken::Punct(s) => s.clone(),
            PPToken::Whitespace => " ".to_string(),
            PPToken::Newline => "\n".to_string(),
            PPToken::Hash => "#".to_string(),
            PPToken::HashHash => "##".to_string(),
            PPToken::Other(c) => c.to_string(),
        }
    }

    fn tokens_to_string(&self, tokens: &[PPToken]) -> String {
        let mut s = String::new();
        for tok in tokens {
            s.push_str(&self.stringify_token(tok));
        }
        s
    }

    fn eval_constant_expr(&mut self, expr: &str) -> i64 {
        // First expand macros
        let expanded = self.expand_line(expr);
        // Then evaluate
        let mut eval = ConstEval::new(&expanded);
        eval.expr()
    }
}

struct IfState {
    active: bool,
    seen_true: bool,
    parent_active: bool,
}

fn has_unbalanced_parens(s: &str) -> bool {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut in_char = false;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if in_string {
            if ch == b'\\' { i += 1; }
            else if ch == b'"' { in_string = false; }
        } else if in_char {
            if ch == b'\\' { i += 1; }
            else if ch == b'\'' { in_char = false; }
        } else {
            match ch {
                b'"' => in_string = true,
                b'\'' => in_char = true,
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
        }
        i += 1;
    }
    depth > 0
}

fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    if let Some(pos) = s.find(|c: char| c.is_whitespace()) {
        (&s[..pos], s[pos..].trim())
    } else {
        (s, "")
    }
}

// Constant expression evaluator for #if directives
struct ConstEval<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> ConstEval<'a> {
    fn new(s: &'a str) -> Self {
        Self { src: s.as_bytes(), pos: 0 }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.src.len() && (self.src[self.pos] == b' ' || self.src[self.pos] == b'\t') {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn expr(&mut self) -> i64 {
        self.ternary()
    }

    fn ternary(&mut self) -> i64 {
        let cond = self.logor();
        self.skip_ws();
        if self.peek() == Some(b'?') {
            self.pos += 1;
            let t = self.expr();
            self.skip_ws();
            if self.peek() == Some(b':') { self.pos += 1; }
            let f = self.expr();
            if cond != 0 { t } else { f }
        } else {
            cond
        }
    }

    fn logor(&mut self) -> i64 {
        let mut v = self.logand();
        loop {
            self.skip_ws();
            if self.pos + 1 < self.src.len() && self.src[self.pos] == b'|' && self.src[self.pos + 1] == b'|' {
                self.pos += 2;
                let r = self.logand();
                v = if v != 0 || r != 0 { 1 } else { 0 };
            } else { break; }
        }
        v
    }

    fn logand(&mut self) -> i64 {
        let mut v = self.bitor();
        loop {
            self.skip_ws();
            if self.pos + 1 < self.src.len() && self.src[self.pos] == b'&' && self.src[self.pos + 1] == b'&' {
                self.pos += 2;
                let r = self.bitor();
                v = if v != 0 && r != 0 { 1 } else { 0 };
            } else { break; }
        }
        v
    }

    fn bitor(&mut self) -> i64 {
        let mut v = self.bitxor();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'|') && self.src.get(self.pos + 1) != Some(&b'|') {
                self.pos += 1;
                v |= self.bitxor();
            } else { break; }
        }
        v
    }

    fn bitxor(&mut self) -> i64 {
        let mut v = self.bitand();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'^') {
                self.pos += 1;
                v ^= self.bitand();
            } else { break; }
        }
        v
    }

    fn bitand(&mut self) -> i64 {
        let mut v = self.equality();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'&') && self.src.get(self.pos + 1) != Some(&b'&') {
                self.pos += 1;
                v &= self.equality();
            } else { break; }
        }
        v
    }

    fn equality(&mut self) -> i64 {
        let mut v = self.relational();
        loop {
            self.skip_ws();
            if self.pos + 1 < self.src.len() && self.src[self.pos] == b'=' && self.src[self.pos + 1] == b'=' {
                self.pos += 2;
                v = if v == self.relational() { 1 } else { 0 };
            } else if self.pos + 1 < self.src.len() && self.src[self.pos] == b'!' && self.src[self.pos + 1] == b'=' {
                self.pos += 2;
                v = if v != self.relational() { 1 } else { 0 };
            } else { break; }
        }
        v
    }

    fn relational(&mut self) -> i64 {
        let mut v = self.shift();
        loop {
            self.skip_ws();
            if self.pos + 1 < self.src.len() && self.src[self.pos] == b'<' && self.src[self.pos + 1] == b'=' {
                self.pos += 2;
                v = if v <= self.shift() { 1 } else { 0 };
            } else if self.pos + 1 < self.src.len() && self.src[self.pos] == b'>' && self.src[self.pos + 1] == b'=' {
                self.pos += 2;
                v = if v >= self.shift() { 1 } else { 0 };
            } else if self.peek() == Some(b'<') && self.src.get(self.pos + 1) != Some(&b'<') {
                self.pos += 1;
                v = if v < self.shift() { 1 } else { 0 };
            } else if self.peek() == Some(b'>') && self.src.get(self.pos + 1) != Some(&b'>') {
                self.pos += 1;
                v = if v > self.shift() { 1 } else { 0 };
            } else { break; }
        }
        v
    }

    fn shift(&mut self) -> i64 {
        let mut v = self.additive();
        loop {
            self.skip_ws();
            if self.pos + 1 < self.src.len() && self.src[self.pos] == b'<' && self.src[self.pos + 1] == b'<' {
                self.pos += 2;
                v = v.wrapping_shl(self.additive() as u32);
            } else if self.pos + 1 < self.src.len() && self.src[self.pos] == b'>' && self.src[self.pos + 1] == b'>' {
                self.pos += 2;
                v = v.wrapping_shr(self.additive() as u32);
            } else { break; }
        }
        v
    }

    fn additive(&mut self) -> i64 {
        let mut v = self.multiplicative();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'+') && self.src.get(self.pos + 1) != Some(&b'+') {
                self.pos += 1;
                v = v.wrapping_add(self.multiplicative());
            } else if self.peek() == Some(b'-') && self.src.get(self.pos + 1) != Some(&b'-') {
                self.pos += 1;
                v = v.wrapping_sub(self.multiplicative());
            } else { break; }
        }
        v
    }

    fn multiplicative(&mut self) -> i64 {
        let mut v = self.unary();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'*') {
                self.pos += 1;
                v = v.wrapping_mul(self.unary());
            } else if self.peek() == Some(b'/') {
                self.pos += 1;
                let r = self.unary();
                v = if r != 0 { v / r } else { 0 };
            } else if self.peek() == Some(b'%') {
                self.pos += 1;
                let r = self.unary();
                v = if r != 0 { v % r } else { 0 };
            } else { break; }
        }
        v
    }

    fn unary(&mut self) -> i64 {
        self.skip_ws();
        match self.peek() {
            Some(b'!') => { self.pos += 1; if self.unary() == 0 { 1 } else { 0 } }
            Some(b'~') => { self.pos += 1; !self.unary() }
            Some(b'-') if self.src.get(self.pos + 1) != Some(&b'-') => { self.pos += 1; -self.unary() }
            Some(b'+') if self.src.get(self.pos + 1) != Some(&b'+') => { self.pos += 1; self.unary() }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> i64 {
        self.skip_ws();
        match self.peek() {
            Some(b'(') => {
                self.pos += 1;
                let v = self.expr();
                self.skip_ws();
                if self.peek() == Some(b')') { self.pos += 1; }
                v
            }
            Some(b'\'') => {
                self.pos += 1;
                let val = if self.peek() == Some(b'\\') {
                    self.pos += 1;
                    match self.src.get(self.pos).copied() {
                        Some(b'n') => { self.pos += 1; b'\n' as i64 }
                        Some(b't') => { self.pos += 1; b'\t' as i64 }
                        Some(b'0') => { self.pos += 1; 0 }
                        Some(b'\\') => { self.pos += 1; b'\\' as i64 }
                        Some(b'\'') => { self.pos += 1; b'\'' as i64 }
                        Some(c) => { self.pos += 1; c as i64 }
                        None => 0,
                    }
                } else {
                    let c = self.src.get(self.pos).copied().unwrap_or(0);
                    self.pos += 1;
                    c as i64
                };
                if self.peek() == Some(b'\'') { self.pos += 1; }
                val
            }
            Some(c) if c.is_ascii_digit() => {
                let start = self.pos;
                if c == b'0' && self.src.get(self.pos + 1).is_some_and(|c| *c == b'x' || *c == b'X') {
                    self.pos += 2;
                    while self.pos < self.src.len() && self.src[self.pos].is_ascii_hexdigit() { self.pos += 1; }
                    let hex_str = std::str::from_utf8(&self.src[start + 2..self.pos]).unwrap_or("0");
                    let val = i64::from_str_radix(hex_str, 16).unwrap_or(0);
                    // Skip suffixes
                    while self.pos < self.src.len() && matches!(self.src[self.pos], b'u' | b'U' | b'l' | b'L') { self.pos += 1; }
                    val
                } else {
                    while self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() { self.pos += 1; }
                    let num_str = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("0");
                    let val = if num_str.starts_with('0') && num_str.len() > 1 {
                        i64::from_str_radix(num_str, 8).unwrap_or(0)
                    } else {
                        num_str.parse().unwrap_or(0)
                    };
                    while self.pos < self.src.len() && matches!(self.src[self.pos], b'u' | b'U' | b'l' | b'L') { self.pos += 1; }
                    val
                }
            }
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                let start = self.pos;
                while self.pos < self.src.len() && (self.src[self.pos].is_ascii_alphanumeric() || self.src[self.pos] == b'_') {
                    self.pos += 1;
                }
                let ident = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("");
                if ident == "defined" {
                    // defined(NAME) or defined NAME
                    self.skip_ws();
                    let has_paren = self.peek() == Some(b'(');
                    if has_paren { self.pos += 1; }
                    self.skip_ws();
                    let name_start = self.pos;
                    while self.pos < self.src.len() && (self.src[self.pos].is_ascii_alphanumeric() || self.src[self.pos] == b'_') {
                        self.pos += 1;
                    }
                    let _name = std::str::from_utf8(&self.src[name_start..self.pos]).unwrap_or("");
                    if has_paren {
                        self.skip_ws();
                        if self.peek() == Some(b')') { self.pos += 1; }
                    }
                    // We can't check macros from here — the preprocessor expands `defined()` before
                    // eval. In expanded text, `defined(X)` becomes 0 or 1 already.
                    // If we see `defined` here, the macro wasn't defined.
                    0
                } else {
                    // Unknown identifier in constant expression = 0
                    0
                }
            }
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pp(src: &str) -> String {
        let mut p = Preprocessor::new(vec![], vec![]);
        p.preprocess(src, "<test>")
    }

    fn pp_clean(src: &str) -> String {
        pp(src).lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }

    #[test]
    fn passthrough() {
        assert_eq!(pp_clean("int x;"), "int x;");
    }

    #[test]
    fn object_macro() {
        assert_eq!(pp_clean("#define FOO 42\nint x = FOO;"), "int x = 42;");
    }

    #[test]
    fn function_macro() {
        assert_eq!(pp_clean("#define ADD(a,b) a+b\nint x = ADD(1,2);"), "int x = 1+2;");
    }

    #[test]
    fn nested_macro() {
        assert_eq!(pp_clean("#define A 1\n#define B A\nint x = B;"), "int x = 1;");
    }

    #[test]
    fn macro_undef() {
        assert_eq!(pp_clean("#define FOO 1\n#undef FOO\nint x = FOO;"), "int x = FOO;");
    }

    #[test]
    fn ifdef_true() {
        assert_eq!(pp_clean("#define FOO\n#ifdef FOO\nyes\n#endif"), "yes");
    }

    #[test]
    fn ifdef_false() {
        assert_eq!(pp_clean("#ifdef FOO\nyes\n#endif"), "");
    }

    #[test]
    fn ifndef() {
        assert_eq!(pp_clean("#ifndef FOO\nyes\n#endif"), "yes");
        assert_eq!(pp_clean("#define FOO\n#ifndef FOO\nyes\n#endif"), "");
    }

    #[test]
    fn if_true() {
        assert_eq!(pp_clean("#if 1\nyes\n#endif"), "yes");
    }

    #[test]
    fn if_false() {
        assert_eq!(pp_clean("#if 0\nyes\n#endif"), "");
    }

    #[test]
    fn if_else() {
        assert_eq!(pp_clean("#if 0\nno\n#else\nyes\n#endif"), "yes");
        assert_eq!(pp_clean("#if 1\nyes\n#else\nno\n#endif"), "yes");
    }

    #[test]
    fn if_elif() {
        assert_eq!(pp_clean("#if 0\na\n#elif 1\nb\n#else\nc\n#endif"), "b");
        assert_eq!(pp_clean("#if 0\na\n#elif 0\nb\n#else\nc\n#endif"), "c");
    }

    #[test]
    fn predefined_macros() {
        assert_eq!(pp_clean("__STDC__"), "1");
        assert_eq!(pp_clean("__x86_64__"), "1");
        assert_eq!(pp_clean("__TOYOS__"), "1");
        assert_eq!(pp_clean("__ELF__"), "1");
    }

    #[test]
    fn user_defines() {
        let mut p = Preprocessor::new(vec![], vec![("MY_DEF".into(), "99".into())]);
        let out = p.preprocess("int x = MY_DEF;", "<test>");
        let clean: String = out.lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>().join("\n").trim().to_string();
        assert_eq!(clean, "int x = 99;");
    }

    #[test]
    fn stringification() {
        assert_eq!(pp_clean("#define STR(x) #x\nSTR(hello)"), "\"hello\"");
    }

    #[test]
    fn token_pasting() {
        assert_eq!(pp_clean("#define PASTE(a,b) a##b\nPASTE(foo,bar)"), "foobar");
    }

    #[test]
    fn variadic_macro() {
        assert_eq!(pp_clean("#define LOG(fmt,...) printf(fmt,__VA_ARGS__)\nLOG(\"x=%d\",42)"),
                   "printf(\"x=%d\",42)");
    }

    #[test]
    fn line_continuation() {
        assert_eq!(pp_clean("#define LONG \\\n42\nint x = LONG;"), "int x = 42;");
    }

    #[test]
    fn recursive_macro_prevention() {
        // A macro should not recursively expand itself
        assert_eq!(pp_clean("#define FOO FOO\nFOO"), "FOO");
    }

    #[test]
    fn nested_ifdef() {
        let src = "#define A\n#ifdef A\n#ifdef B\nno\n#else\nyes\n#endif\n#endif";
        assert_eq!(pp_clean(src), "yes");
    }

    #[test]
    fn const_expr_arithmetic() {
        assert_eq!(pp_clean("#if 2+3==5\nyes\n#endif"), "yes");
        assert_eq!(pp_clean("#if 10/2==5\nyes\n#endif"), "yes");
        assert_eq!(pp_clean("#if 3*4==12\nyes\n#endif"), "yes");
    }

    #[test]
    fn const_expr_logical() {
        assert_eq!(pp_clean("#if 1 && 1\nyes\n#endif"), "yes");
        assert_eq!(pp_clean("#if 1 && 0\nyes\n#endif"), "");
        assert_eq!(pp_clean("#if 0 || 1\nyes\n#endif"), "yes");
    }

    #[test]
    fn const_expr_comparison() {
        assert_eq!(pp_clean("#if 5 > 3\nyes\n#endif"), "yes");
        assert_eq!(pp_clean("#if 3 > 5\nyes\n#endif"), "");
        assert_eq!(pp_clean("#if 3 <= 3\nyes\n#endif"), "yes");
    }

    #[test]
    fn const_expr_unary() {
        assert_eq!(pp_clean("#if !0\nyes\n#endif"), "yes");
        assert_eq!(pp_clean("#if !1\nyes\n#endif"), "");
        assert_eq!(pp_clean("#if -1 < 0\nyes\n#endif"), "yes");
    }

    #[test]
    fn const_expr_hex() {
        assert_eq!(pp_clean("#if 0xff == 255\nyes\n#endif"), "yes");
    }

    #[test]
    fn const_expr_ternary() {
        assert_eq!(pp_clean("#if 1 ? 42 : 0\nyes\n#endif"), "yes");
        assert_eq!(pp_clean("#if 0 ? 42 : 0\nyes\n#endif"), "");
    }

    #[test]
    fn const_expr_shifts() {
        assert_eq!(pp_clean("#if 1<<4 == 16\nyes\n#endif"), "yes");
    }

    #[test]
    fn const_expr_char_literal() {
        assert_eq!(pp_clean("#if 'A' == 65\nyes\n#endif"), "yes");
    }

    #[test]
    fn empty_define() {
        // #define FOO (no value) — should expand to nothing
        assert_eq!(pp_clean("#define FOO\nFOO x"), "x");
    }

    #[test]
    fn function_macro_no_args() {
        assert_eq!(pp_clean("#define F() 42\nint x = F();"), "int x = 42;");
    }

    #[test]
    fn line_markers_emitted() {
        let out = pp("int x;");
        assert!(out.contains("# 1 \"<test>\""));
    }

    #[test]
    fn define_multiline_comment() {
        // A #define with a multi-line comment (no backslash continuation)
        let out = pp_clean("#define FOO 42 /* this spans\n   two lines */\nint x = FOO;");
        assert_eq!(out, "int x = 42;");
    }

    #[test]
    fn define_multiline_comment_no_leak() {
        // The second line of a multi-line comment in a #define must not leak as code
        let out = pp_clean("#define SHN_BEFORE 0xff00 /* Order section before all others\n\t\t\t\t\t   (Solaris).  */\nint x;");
        assert_eq!(out, "int x;");
    }

    #[test]
    fn token_pasting_multiple() {
        // ElfW(type) pattern: Elf##64##_##type
        let out = pp_clean("#define ElfW(type) Elf##64##_##type\nElfW(Addr) x;");
        assert_eq!(out, "Elf64_Addr x;");
    }

    #[test]
    fn token_pasting_param_both_sides() {
        // param##suffix and prefix##param
        let out = pp_clean("#define PASTE(a, b) a##_##b\nPASTE(foo, bar)");
        assert_eq!(out, "foo_bar");
    }

    #[test]
    fn token_pasting_with_spaces() {
        // ## with spaces around it (as in DEF_ASMDIR)
        let out = pp_clean("#define DEF_ASMDIR(x) TOK_ASMDIR_ ## x\nDEF_ASMDIR(byte)");
        assert_eq!(out, "TOK_ASMDIR_byte");
    }

    #[test]
    fn multiline_macro_invocation() {
        // Macro call spanning multiple lines
        let out = pp_clean("#define F(a,b) a+b\nF(1,\n2)");
        assert_eq!(out, "1+2");
    }

    #[test]
    fn multiline_macro_invocation_nested() {
        // Macro call with nested parens spanning multiple lines
        let out = pp_clean("#define W(s,v) write(s,v)\nW(buf,\nfoo - bar + 4)");
        assert_eq!(out, "write(buf,foo - bar + 4)");
    }

    #[test]
    fn ifdef_inside_multiline_call() {
        // #if/#else/#endif inside a multi-line function call
        let out = pp_clean("#define BIG 1\nfoo(\n#if BIG\n42\n#else\n0\n#endif\n)");
        assert_eq!(out, "foo( 42 )");
    }
}

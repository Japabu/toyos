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
    Hash,
    HashHash,
}

pub struct Preprocessor {
    macros: HashMap<String, Macro>,
    include_paths: Vec<PathBuf>,
    output: String,
    expanding: HashSet<String>,
    file_stack: Vec<(String, u32)>,
}

impl Preprocessor {
    pub fn new(include_paths: Vec<PathBuf>, defines: Vec<(String, String)>, target: Option<&str>) -> Self {
        let mut pp = Self {
            macros: HashMap::new(),
            include_paths,
            output: String::new(),
            expanding: HashSet::new(),
            file_stack: Vec::new(),
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
                            // Check if expansion ends with a function-like macro name
                            // whose arguments come from subsequent tokens in the stream
                            if let Some(PPToken::Ident(last_name)) = expanded.last() {
                                if matches!(self.macros.get(last_name), Some(Macro::Function(..)))
                                    && !self.expanding.contains(last_name)
                                {
                                    let mut k = i + 1;
                                    while k < tokens.len() && tokens[k] == PPToken::Whitespace { k += 1; }
                                    if k < tokens.len() && tokens[k] == PPToken::Punct("(".to_string()) {
                                        // Merge: emit everything except last token, then
                                        // process func_name + remaining tokens together
                                        let mut exp = expanded;
                                        let func_tok = exp.pop().unwrap();
                                        result.extend(exp);
                                        let mut merged = vec![func_tok];
                                        merged.extend_from_slice(&tokens[i+1..]);
                                        let re_expanded = self.expand_tokens(&merged);
                                        result.extend(re_expanded);
                                        return result;
                                    }
                                }
                            }
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
                                // Check if expansion ends with a function-like macro name
                                // whose arguments come from subsequent tokens
                                if let Some(PPToken::Ident(last_name)) = expanded.last() {
                                    if matches!(self.macros.get(last_name), Some(Macro::Function(..)))
                                        && !self.expanding.contains(last_name)
                                    {
                                        let mut k = j;
                                        while k < tokens.len() && tokens[k] == PPToken::Whitespace { k += 1; }
                                        if k < tokens.len() && tokens[k] == PPToken::Punct("(".to_string()) {
                                            let mut exp = expanded;
                                            let func_tok = exp.pop().unwrap();
                                            result.extend(exp);
                                            let mut merged = vec![func_tok];
                                            merged.extend_from_slice(&tokens[j..]);
                                            let re_expanded = self.expand_tokens(&merged);
                                            result.extend(re_expanded);
                                            return result;
                                        }
                                    }
                                }
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
            PPToken::Hash => "#".to_string(),
            PPToken::HashHash => "##".to_string(),
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
        // Replace defined(X) and defined X with 0 or 1 BEFORE macro expansion
        let with_defined = self.replace_defined(expr);
        // Then expand macros
        let expanded = self.expand_line(&with_defined);
        // Then evaluate
        let mut eval = ConstEval::new(&expanded);
        eval.expr()
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
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') { i += 1; }
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

use super::{Macro, PPToken, Preprocessor};

impl Preprocessor {
    pub(crate) fn strip_ws_around_hashhash(tokens: Vec<PPToken>) -> Vec<PPToken> {
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

    pub(crate) fn tokenize_pp(&self, s: &str) -> Vec<PPToken> {
        let mut tokens = Vec::new();
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b' ' | b'\t' | b'\n' | b'\r' => {
                    tokens.push(PPToken::Whitespace);
                    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') { i += 1; }
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
                c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                    let start = i;
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$') { i += 1; }
                    tokens.push(PPToken::Ident(String::from_utf8_lossy(&bytes[start..i]).into_owned()));
                }
                c if c.is_ascii_digit() => {
                    let start = i;
                    if c == b'0' && i + 1 < bytes.len() && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X') {
                        // Hex: 0x...
                        i += 2;
                        while i < bytes.len() && bytes[i].is_ascii_hexdigit() { i += 1; }
                        // Hex float: 0x1.2p3
                        if i < bytes.len() && bytes[i] == b'.' {
                            i += 1;
                            while i < bytes.len() && bytes[i].is_ascii_hexdigit() { i += 1; }
                        }
                        if i < bytes.len() && (bytes[i] == b'p' || bytes[i] == b'P') {
                            i += 1;
                            if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') { i += 1; }
                            while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
                        }
                    } else if c == b'0' && i + 1 < bytes.len() && (bytes[i + 1] == b'b' || bytes[i + 1] == b'B') {
                        // Binary: 0b...
                        i += 2;
                        while i < bytes.len() && (bytes[i] == b'0' || bytes[i] == b'1') { i += 1; }
                    } else {
                        // Decimal/octal
                        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') { i += 1; }
                        // Scientific notation: e+/-
                        if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                            i += 1;
                            if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') { i += 1; }
                            while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
                        }
                    }
                    // Suffixes
                    while i < bytes.len() && (bytes[i] == b'u' || bytes[i] == b'U' || bytes[i] == b'l' || bytes[i] == b'L' || bytes[i] == b'f' || bytes[i] == b'F') { i += 1; }
                    tokens.push(PPToken::Number(String::from_utf8_lossy(&bytes[start..i]).into_owned()));
                }
                c => {
                    // Multi-char punctuation
                    let start = i;
                    i += 1;
                    // Handle ... (ellipsis) before two-char operators
                    if c == b'.' && i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1] == b'.' {
                        i += 2;
                    } else if i < bytes.len() {
                        // Handle common two-char operators
                        let pair = [c, bytes[i]];
                        match &pair {
                            b"->" | b"++" | b"--" | b"<<" | b">>" | b"<=" | b">=" | b"==" | b"!="
                            | b"&&" | b"||" | b"+=" | b"-=" | b"*=" | b"/=" | b"%="
                            | b"&=" | b"|=" | b"^=" | b"##" => { i += 1; }
                            _ => {}
                        }
                    }
                    tokens.push(PPToken::Punct(String::from_utf8_lossy(&bytes[start..i]).into_owned()));
                }
            }
        }
        tokens
    }

    pub(crate) fn expand_tokens(&mut self, tokens: &[PPToken]) -> Vec<PPToken> {
        let mut result = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            match &tokens[i] {
                PPToken::Ident(name) if name == "__COUNTER__" => {
                    let val = self.counter;
                    self.counter += 1;
                    result.push(PPToken::Number(val.to_string()));
                    i += 1;
                    continue;
                }
                // Macro is currently being expanded (recursion guard) — blue-paint the token
                // so it passes through unchanged and is never re-expanded during rescanning.
                // This implements the C99 §6.10.3.4 hide-set rule without per-token metadata.
                PPToken::Ident(name) if self.macros.contains_key(name.trim_start_matches('\x01'))
                    && self.expanding.contains(name.trim_start_matches('\x01')) =>
                {
                    let clean = name.trim_start_matches('\x01');
                    result.push(PPToken::Ident(format!("\x01{}", clean)));
                    i += 1;
                }
                PPToken::Ident(name) if self.macros.contains_key(name) && !self.expanding.contains(name) => {
                    let mac = self.macros.get(name).cloned();
                    match mac {
                        Some(Macro::Object(body)) => {
                            let name = name.clone();
                            // Process ## in object macro body via substitute (no params/args)
                            let substituted = self.substitute(&[], false, &body, &[], &[]);
                            // If the body opens more parens than it closes, the closing parens
                            // must come from the outer token stream (e.g. `#define h g(~`).
                            // Splice body + remaining tokens so inner function macros can
                            // collect their full argument lists.
                            let open_depth: i32 = substituted.iter().map(|t| match t {
                                PPToken::Punct(s) if s == "(" => 1,
                                PPToken::Punct(s) if s == ")" => -1,
                                _ => 0,
                            }).sum();
                            if open_depth > 0 {
                                self.expanding.insert(name.clone());
                                let mut merged = substituted;
                                merged.extend_from_slice(&tokens[i + 1..]);
                                let re_expanded = self.expand_tokens(&merged);
                                self.expanding.remove(&name);
                                result.extend(re_expanded);
                                return result;
                            }
                            self.expanding.insert(name.clone());
                            let expanded = self.expand_tokens(&substituted);
                            self.expanding.remove(&name);
                            // Check if expansion ends with a function-like macro name
                            // whose arguments come from subsequent tokens in the stream
                            if let Some(PPToken::Ident(last_name)) = expanded.last() {
                                if matches!(self.macros.get(last_name), Some(Macro::Function(..)))
                                    && !self.expanding.contains(last_name)
                                {
                                    let mut k = i + 1;
                                    while k < tokens.len() && tokens[k] == PPToken::Whitespace { k += 1; }
                                    if k < tokens.len() && tokens[k] == PPToken::Punct("(".to_string()) {
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
                            let mut j = i + 1;
                            while j < tokens.len() && tokens[j] == PPToken::Whitespace { j += 1; }
                            if j < tokens.len() && tokens[j] == PPToken::Punct("(".to_string()) {
                                j += 1;
                                let raw_args = self.collect_macro_args(tokens, &mut j, params.len(), variadic);
                                // Pre-expand args only when the parameter appears in a normal
                                // (non-#/##) position. Args used solely in # or ## contexts
                                // must NOT be pre-expanded (avoids side effects like __COUNTER__).
                                let mut expanded_args: Vec<Vec<PPToken>> = Vec::with_capacity(raw_args.len());
                                for (idx, param) in params.iter().enumerate() {
                                    let arg = raw_args.get(idx).map(|a| a.as_slice()).unwrap_or(&[]);
                                    expanded_args.push(if Self::param_in_normal_pos(&body, param) {
                                        self.expand_tokens(arg)
                                    } else {
                                        arg.to_vec()
                                    });
                                }
                                if variadic {
                                    let va = raw_args.get(params.len()).map(|a| a.as_slice()).unwrap_or(&[]);
                                    expanded_args.push(if Self::param_in_normal_pos(&body, "__VA_ARGS__") {
                                        self.expand_tokens(va)
                                    } else {
                                        va.to_vec()
                                    });
                                }
                                let name = name.clone();
                                let substituted = self.substitute(&params, variadic, &body, &raw_args, &expanded_args);
                                self.expanding.insert(name.clone());
                                let expanded = self.expand_tokens(&substituted);
                                self.expanding.remove(&name);
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
                                // No parens — not a function-like invocation
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

    // Returns true if the parameter named `param_name` appears in at least one position
    // in `body` that is NOT a # (stringify) or ## (paste) context.
    // Only args that appear in normal context need to be pre-expanded.
    fn param_in_normal_pos(body: &[PPToken], param_name: &str) -> bool {
        let mut i = 0;
        while i < body.len() {
            if matches!(&body[i], PPToken::Ident(n) if n == param_name) {
                // Check if preceded by # (stringify) — skip whitespace backwards
                let mut j = i;
                let preceded_by_hash = loop {
                    if j == 0 { break false; }
                    j -= 1;
                    match &body[j] {
                        PPToken::Whitespace => continue,
                        PPToken::Hash => break true,
                        _ => break false,
                    }
                };
                let prev_hashhash = i > 0 && body[i - 1] == PPToken::HashHash;
                let next_hashhash = i + 1 < body.len() && body[i + 1] == PPToken::HashHash;
                if !preceded_by_hash && !prev_hashhash && !next_hashhash {
                    return true;
                }
            }
            i += 1;
        }
        false
    }

    // Resolve a body token to its string form using raw (unexpanded) args — for ## pasting.
    fn resolve_raw(&self, tok: &PPToken, params: &[String], variadic: bool, raw_args: &[Vec<PPToken>]) -> String {
        if let PPToken::Ident(name) = tok {
            if let Some(idx) = params.iter().position(|p| p == name) {
                return self.tokens_to_string(raw_args.get(idx).map(|a| a.as_slice()).unwrap_or(&[]));
            }
            if name == "__VA_ARGS__" && variadic {
                return self.tokens_to_string(raw_args.get(params.len()).map(|a| a.as_slice()).unwrap_or(&[]));
            }
        }
        self.stringify_token(tok)
    }

    // Stringify a raw argument token sequence per C99 # operator rules:
    // escape \ and " only inside string/char literal tokens; bare \ tokens pass through.
    fn stringify_arg(&self, raw_arg: &[PPToken]) -> PPToken {
        // Trim leading/trailing whitespace
        let start = raw_arg.iter().position(|t| t != &PPToken::Whitespace).unwrap_or(0);
        let end = raw_arg.iter().rposition(|t| t != &PPToken::Whitespace).map(|p| p + 1).unwrap_or(0);
        let tokens = if start < end { &raw_arg[start..end] } else { &[] };

        let mut out = String::new();
        let mut prev_ws = false;
        for tok in tokens {
            match tok {
                PPToken::Whitespace => {
                    if !prev_ws && !out.is_empty() {
                        out.push(' ');
                        prev_ws = true;
                    }
                }
                PPToken::StringLit(s) => {
                    prev_ws = false;
                    // The outer " quotes must be escaped as \" since they appear inside
                    // the new string literal. The content's \ must be doubled to \\.
                    let inner = &s[1..s.len().saturating_sub(1)];
                    out.push('\\'); out.push('"');
                    for ch in inner.chars() {
                        if ch == '\\' { out.push('\\'); }
                        out.push(ch);
                    }
                    out.push('\\'); out.push('"');
                }
                PPToken::CharLit(s) => {
                    prev_ws = false;
                    // \ inside char literals must be doubled.
                    let inner = &s[1..s.len().saturating_sub(1)];
                    out.push('\'');
                    for ch in inner.chars() {
                        if ch == '\\' { out.push('\\'); }
                        out.push(ch);
                    }
                    out.push('\'');
                }
                tok => {
                    prev_ws = false;
                    out.push_str(&self.stringify_token(tok));
                }
            }
        }
        PPToken::StringLit(format!("\"{}\"", out))
    }

    // Substitute params into a macro body.
    // raw_args: unexpanded — used for # stringification and ## pasting.
    // expanded_args: pre-expanded — used for normal parameter substitution.
    fn substitute(&self, params: &[String], variadic: bool, body: &[PPToken],
                  raw_args: &[Vec<PPToken>], expanded_args: &[Vec<PPToken>]) -> Vec<PPToken> {
        let mut result = Vec::new();
        let mut i = 0;
        while i < body.len() {
            // GNU comma-eating: , ## __VA_ARGS__ with empty VA_ARGS → emit nothing.
            // This extension suppresses the trailing comma when the variadic arg is empty.
            if variadic
                && i + 2 < body.len()
                && body[i] == PPToken::Punct(",".to_string())
                && body[i + 1] == PPToken::HashHash
                && matches!(&body[i + 2], PPToken::Ident(n) if n == "__VA_ARGS__")
            {
                let va = raw_args.get(params.len()).map(|a| a.as_slice()).unwrap_or(&[]);
                if self.tokens_to_string(va).trim().is_empty() {
                    i += 3;
                    continue;
                }
            }

            // Token pasting ## — use raw args, handle chains like A##B##C##D
            if i + 1 < body.len() && body[i + 1] == PPToken::HashHash {
                let mut pasted = self.resolve_raw(&body[i], params, variadic, raw_args);
                i += 1; // past the left operand
                while i + 1 < body.len() && body[i] == PPToken::HashHash {
                    let right = self.resolve_raw(&body[i + 1], params, variadic, raw_args);
                    pasted.push_str(&right);
                    i += 2;
                }
                result.extend(self.tokenize_pp(&pasted));
                continue;
            }

            // Stringification # — skip whitespace between # and param name, use raw args
            if body[i] == PPToken::Hash {
                let mut j = i + 1;
                while j < body.len() && body[j] == PPToken::Whitespace { j += 1; }
                if j < body.len() {
                    let param_idx = if let PPToken::Ident(name) = &body[j] {
                        if name == "__VA_ARGS__" && variadic {
                            Some(params.len())
                        } else {
                            params.iter().position(|p| p == name)
                        }
                    } else {
                        None
                    };
                    if let Some(idx) = param_idx {
                        let raw = raw_args.get(idx).map(|a| a.as_slice()).unwrap_or(&[]);
                        result.push(self.stringify_arg(raw));
                        i = j + 1;
                        continue;
                    }
                }
                // # not followed by a parameter — emit as-is
                result.push(body[i].clone());
                i += 1;
                continue;
            }

            // Normal substitution — use pre-expanded args
            match &body[i] {
                PPToken::Ident(name) if name == "__VA_ARGS__" && variadic => {
                    if let Some(va_args) = expanded_args.get(params.len()) {
                        result.extend(va_args.clone());
                    }
                    i += 1;
                }
                PPToken::Ident(name) => {
                    if let Some(idx) = params.iter().position(|p| p == name) {
                        result.extend(expanded_args.get(idx).cloned().unwrap_or_default());
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

    pub(crate) fn stringify_token(&self, token: &PPToken) -> String {
        match token {
            PPToken::Ident(s) => s.trim_start_matches('\x01').to_string(),
            PPToken::Number(s) | PPToken::StringLit(s) | PPToken::CharLit(s) | PPToken::Punct(s) => s.clone(),
            PPToken::Whitespace => " ".to_string(),
            PPToken::Hash => "#".to_string(),
            PPToken::HashHash => "##".to_string(),
        }
    }

    pub(crate) fn tokens_to_string(&self, tokens: &[PPToken]) -> String {
        let mut s = String::new();
        let mut prev_ts = String::new();
        for tok in tokens {
            let ts = self.stringify_token(tok);
            if ts.is_empty() { continue; }
            // Skip duplicate whitespace
            if ts == " " && s.ends_with(' ') { prev_ts = ts; continue; }
            if !prev_ts.is_empty() && Self::needs_space_between(&prev_ts, &ts) {
                s.push(' ');
            }
            s.push_str(&ts);
            prev_ts = ts;
        }
        s
    }

    fn needs_space_between(prev: &str, next: &str) -> bool {
        let last = match prev.as_bytes().last() { Some(&b) => b, None => return false };
        let first = match next.as_bytes().first() { Some(&b) => b, None => return false };
        // Adjacent identifiers/numbers: always merge
        if (last.is_ascii_alphanumeric() || last == b'_') && (first.is_ascii_alphanumeric() || first == b'_') {
            return true;
        }
        // pp-number: token ending in e/E/p/P before sign +/- would extend exponent
        if matches!(last, b'e' | b'E' | b'p' | b'P') && matches!(first, b'+' | b'-') {
            return true;
        }
        // + sequences: no space if prev already ends with ++ (then +++  re-tokenizes identically)
        if last == b'+' && first == b'+' {
            return !(prev.len() >= 2 && prev.as_bytes()[prev.len() - 2] == b'+');
        }
        // - sequences: same logic
        if last == b'-' && first == b'-' {
            return !(prev.len() >= 2 && prev.as_bytes()[prev.len() - 2] == b'-');
        }
        // -> : space only if prev ends with single - (not --)
        if last == b'-' && first == b'>' {
            return !(prev.len() >= 2 && prev.as_bytes()[prev.len() - 2] == b'-');
        }
        if last == b'<' && first == b'<' { return true; }
        if last == b'>' && first == b'>' { return true; }
        if last == b'&' && first == b'&' { return true; }
        if last == b'|' && first == b'|' { return true; }
        if last == b'/' && (first == b'/' || first == b'*') { return true; }
        false
    }

    pub(crate) fn expand_tokens_const(&self, tokens: &[PPToken]) -> Vec<PPToken> {
        // Const version for use in has_include — no counter increment
        let mut result = Vec::new();
        for tok in tokens {
            match tok {
                PPToken::Ident(name) if self.macros.contains_key(name) && !self.expanding.contains(name) => {
                    if let Some(Macro::Object(body)) = self.macros.get(name) {
                        result.extend(body.clone());
                    } else {
                        result.push(tok.clone());
                    }
                }
                _ => result.push(tok.clone()),
            }
        }
        result
    }
}

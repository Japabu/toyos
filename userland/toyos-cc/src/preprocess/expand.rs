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

    pub(crate) fn stringify_token(&self, token: &PPToken) -> String {
        match token {
            PPToken::Ident(s) | PPToken::Number(s) | PPToken::StringLit(s)
            | PPToken::CharLit(s) | PPToken::Punct(s) => s.clone(),
            PPToken::Whitespace => " ".to_string(),
            PPToken::Hash => "#".to_string(),
            PPToken::HashHash => "##".to_string(),
        }
    }

    pub(crate) fn tokens_to_string(&self, tokens: &[PPToken]) -> String {
        let mut s = String::new();
        for tok in tokens {
            let tok_str = self.stringify_token(tok);
            if !s.is_empty() && !tok_str.is_empty() {
                let last = s.as_bytes()[s.len() - 1];
                let first = tok_str.as_bytes()[0];
                // Insert space to prevent token merging during re-lexing
                let need_space =
                    (last.is_ascii_alphanumeric() || last == b'_') && (first.is_ascii_alphanumeric() || first == b'_')
                    || (last == b'+' && first == b'+')
                    || (last == b'-' && (first == b'-' || first == b'>'))
                    || (last == b'<' && first == b'<')
                    || (last == b'>' && first == b'>')
                    || (last == b'&' && first == b'&')
                    || (last == b'|' && first == b'|')
                    || (last == b'/' && (first == b'/' || first == b'*'));
                if need_space { s.push(' '); }
            }
            s.push_str(&tok_str);
        }
        s
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

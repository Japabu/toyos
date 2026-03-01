pub(crate) struct IfState {
    pub active: bool,
    pub seen_true: bool,
    pub parent_active: bool,
}

pub(crate) fn has_unbalanced_parens(s: &str) -> bool {
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

pub(crate) fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    if let Some(pos) = s.find(|c: char| c.is_whitespace()) {
        (&s[..pos], s[pos..].trim())
    } else {
        (s, "")
    }
}

// Constant expression evaluator for #if directives
pub(crate) struct ConstEval<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> ConstEval<'a> {
    pub fn new(s: &'a str) -> Self {
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

    pub fn expr(&mut self) -> i64 {
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
                    while self.pos < self.src.len() && matches!(self.src[self.pos], b'u' | b'U' | b'l' | b'L') { self.pos += 1; }
                    val
                } else if c == b'0' && self.src.get(self.pos + 1).is_some_and(|c| *c == b'b' || *c == b'B') {
                    self.pos += 2;
                    while self.pos < self.src.len() && (self.src[self.pos] == b'0' || self.src[self.pos] == b'1') { self.pos += 1; }
                    let bin_str = std::str::from_utf8(&self.src[start + 2..self.pos]).unwrap_or("0");
                    let val = i64::from_str_radix(bin_str, 2).unwrap_or(0);
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
            Some(c) if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                let start = self.pos;
                while self.pos < self.src.len() && (self.src[self.pos].is_ascii_alphanumeric() || self.src[self.pos] == b'_' || self.src[self.pos] == b'$') {
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
                    while self.pos < self.src.len() && (self.src[self.pos].is_ascii_alphanumeric() || self.src[self.pos] == b'_' || self.src[self.pos] == b'$') {
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

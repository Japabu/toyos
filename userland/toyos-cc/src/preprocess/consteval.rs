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

// A value in a preprocessor constant expression.
// Arithmetic follows C99 intmax_t / uintmax_t rules:
// - unsigned flag is set when a literal has a U suffix, or when a hex/octal
//   literal does not fit in i64 (GCC extension, matches TCC behaviour).
// - If either operand of a binary arithmetic/compare op is unsigned, the
//   result is unsigned and arithmetic uses wrapping u64 semantics.
#[derive(Clone, Copy)]
struct Val {
    bits: u64,
    unsigned: bool,
}

impl Val {
    fn signed(v: i64) -> Self { Val { bits: v as u64, unsigned: false } }
    fn unsigned(v: u64) -> Self { Val { bits: v, unsigned: true } }
    fn zero() -> Self { Val::signed(0) }
    fn one() -> Self { Val::signed(1) }

    fn is_nonzero(self) -> bool { self.bits != 0 }

    fn as_i64(self) -> i64 { self.bits as i64 }

    fn neg(self) -> Self {
        Val { bits: 0u64.wrapping_sub(self.bits), unsigned: self.unsigned }
    }
    fn bitnot(self) -> Self {
        Val { bits: !self.bits, unsigned: self.unsigned }
    }

    fn make_unsigned(a: Val, b: Val) -> (u64, u64, bool) {
        (a.bits, b.bits, a.unsigned || b.unsigned)
    }

    fn add(a: Val, b: Val) -> Val {
        let (av, bv, u) = Self::make_unsigned(a, b);
        Val { bits: av.wrapping_add(bv), unsigned: u }
    }
    fn sub(a: Val, b: Val) -> Val {
        let (av, bv, u) = Self::make_unsigned(a, b);
        Val { bits: av.wrapping_sub(bv), unsigned: u }
    }
    fn mul(a: Val, b: Val) -> Val {
        let (av, bv, u) = Self::make_unsigned(a, b);
        Val { bits: av.wrapping_mul(bv), unsigned: u }
    }
    fn div(a: Val, b: Val) -> Val {
        if b.bits == 0 { return Val::zero(); }
        let (av, bv, u) = Self::make_unsigned(a, b);
        if u {
            Val { bits: av / bv, unsigned: true }
        } else {
            Val::signed((av as i64).wrapping_div(bv as i64))
        }
    }
    fn rem(a: Val, b: Val) -> Val {
        if b.bits == 0 { return Val::zero(); }
        let (av, bv, u) = Self::make_unsigned(a, b);
        if u {
            Val { bits: av % bv, unsigned: true }
        } else {
            Val::signed((av as i64).wrapping_rem(bv as i64))
        }
    }
    fn shl(a: Val, b: Val) -> Val {
        Val { bits: a.bits.wrapping_shl(b.bits as u32), unsigned: a.unsigned }
    }
    fn shr(a: Val, b: Val) -> Val {
        if a.unsigned {
            Val { bits: a.bits >> (b.bits as u32 & 63), unsigned: true }
        } else {
            Val::signed((a.bits as i64).wrapping_shr(b.bits as u32))
        }
    }
    fn bitor(a: Val, b: Val) -> Val {
        let (av, bv, u) = Self::make_unsigned(a, b);
        Val { bits: av | bv, unsigned: u }
    }
    fn bitxor(a: Val, b: Val) -> Val {
        let (av, bv, u) = Self::make_unsigned(a, b);
        Val { bits: av ^ bv, unsigned: u }
    }
    fn bitand(a: Val, b: Val) -> Val {
        let (av, bv, u) = Self::make_unsigned(a, b);
        Val { bits: av & bv, unsigned: u }
    }

    fn lt(a: Val, b: Val) -> Val {
        let res = if a.unsigned || b.unsigned { a.bits < b.bits } else { (a.bits as i64) < (b.bits as i64) };
        if res { Val::one() } else { Val::zero() }
    }
    fn gt(a: Val, b: Val) -> Val {
        let res = if a.unsigned || b.unsigned { a.bits > b.bits } else { (a.bits as i64) > (b.bits as i64) };
        if res { Val::one() } else { Val::zero() }
    }
    fn le(a: Val, b: Val) -> Val {
        let res = if a.unsigned || b.unsigned { a.bits <= b.bits } else { (a.bits as i64) <= (b.bits as i64) };
        if res { Val::one() } else { Val::zero() }
    }
    fn ge(a: Val, b: Val) -> Val {
        let res = if a.unsigned || b.unsigned { a.bits >= b.bits } else { (a.bits as i64) >= (b.bits as i64) };
        if res { Val::one() } else { Val::zero() }
    }
    fn eq(a: Val, b: Val) -> Val {
        if a.bits == b.bits { Val::one() } else { Val::zero() }
    }
    fn ne(a: Val, b: Val) -> Val {
        if a.bits != b.bits { Val::one() } else { Val::zero() }
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
        self.ternary().as_i64()
    }

    fn ternary(&mut self) -> Val {
        let cond = self.logor();
        self.skip_ws();
        if self.peek() == Some(b'?') {
            self.pos += 1;
            let t = self.ternary();
            self.skip_ws();
            if self.peek() == Some(b':') { self.pos += 1; }
            let f = self.ternary();
            if cond.is_nonzero() { t } else { f }
        } else {
            cond
        }
    }

    fn logor(&mut self) -> Val {
        let mut v = self.logand();
        loop {
            self.skip_ws();
            if self.pos + 1 < self.src.len() && self.src[self.pos] == b'|' && self.src[self.pos + 1] == b'|' {
                self.pos += 2;
                let r = self.logand();
                v = if v.is_nonzero() || r.is_nonzero() { Val::one() } else { Val::zero() };
            } else { break; }
        }
        v
    }

    fn logand(&mut self) -> Val {
        let mut v = self.bitor();
        loop {
            self.skip_ws();
            if self.pos + 1 < self.src.len() && self.src[self.pos] == b'&' && self.src[self.pos + 1] == b'&' {
                self.pos += 2;
                let r = self.bitor();
                v = if v.is_nonzero() && r.is_nonzero() { Val::one() } else { Val::zero() };
            } else { break; }
        }
        v
    }

    fn bitor(&mut self) -> Val {
        let mut v = self.bitxor();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'|') && self.src.get(self.pos + 1) != Some(&b'|') {
                self.pos += 1;
                v = Val::bitor(v, self.bitxor());
            } else { break; }
        }
        v
    }

    fn bitxor(&mut self) -> Val {
        let mut v = self.bitand();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'^') {
                self.pos += 1;
                v = Val::bitxor(v, self.bitand());
            } else { break; }
        }
        v
    }

    fn bitand(&mut self) -> Val {
        let mut v = self.equality();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'&') && self.src.get(self.pos + 1) != Some(&b'&') {
                self.pos += 1;
                v = Val::bitand(v, self.equality());
            } else { break; }
        }
        v
    }

    fn equality(&mut self) -> Val {
        let mut v = self.relational();
        loop {
            self.skip_ws();
            if self.pos + 1 < self.src.len() && self.src[self.pos] == b'=' && self.src[self.pos + 1] == b'=' {
                self.pos += 2;
                v = Val::eq(v, self.relational());
            } else if self.pos + 1 < self.src.len() && self.src[self.pos] == b'!' && self.src[self.pos + 1] == b'=' {
                self.pos += 2;
                v = Val::ne(v, self.relational());
            } else { break; }
        }
        v
    }

    fn relational(&mut self) -> Val {
        let mut v = self.shift();
        loop {
            self.skip_ws();
            if self.pos + 1 < self.src.len() && self.src[self.pos] == b'<' && self.src[self.pos + 1] == b'=' {
                self.pos += 2;
                v = Val::le(v, self.shift());
            } else if self.pos + 1 < self.src.len() && self.src[self.pos] == b'>' && self.src[self.pos + 1] == b'=' {
                self.pos += 2;
                v = Val::ge(v, self.shift());
            } else if self.peek() == Some(b'<') && self.src.get(self.pos + 1) != Some(&b'<') {
                self.pos += 1;
                v = Val::lt(v, self.shift());
            } else if self.peek() == Some(b'>') && self.src.get(self.pos + 1) != Some(&b'>') {
                self.pos += 1;
                v = Val::gt(v, self.shift());
            } else { break; }
        }
        v
    }

    fn shift(&mut self) -> Val {
        let mut v = self.additive();
        loop {
            self.skip_ws();
            if self.pos + 1 < self.src.len() && self.src[self.pos] == b'<' && self.src[self.pos + 1] == b'<' {
                self.pos += 2;
                v = Val::shl(v, self.additive());
            } else if self.pos + 1 < self.src.len() && self.src[self.pos] == b'>' && self.src[self.pos + 1] == b'>' {
                self.pos += 2;
                v = Val::shr(v, self.additive());
            } else { break; }
        }
        v
    }

    fn additive(&mut self) -> Val {
        let mut v = self.multiplicative();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'+') && self.src.get(self.pos + 1) != Some(&b'+') {
                self.pos += 1;
                v = Val::add(v, self.multiplicative());
            } else if self.peek() == Some(b'-') && self.src.get(self.pos + 1) != Some(&b'-') {
                self.pos += 1;
                v = Val::sub(v, self.multiplicative());
            } else { break; }
        }
        v
    }

    fn multiplicative(&mut self) -> Val {
        let mut v = self.unary();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'*') {
                self.pos += 1;
                v = Val::mul(v, self.unary());
            } else if self.peek() == Some(b'/') {
                self.pos += 1;
                v = Val::div(v, self.unary());
            } else if self.peek() == Some(b'%') {
                self.pos += 1;
                v = Val::rem(v, self.unary());
            } else { break; }
        }
        v
    }

    fn unary(&mut self) -> Val {
        self.skip_ws();
        match self.peek() {
            Some(b'!') => { self.pos += 1; if self.unary().is_nonzero() { Val::zero() } else { Val::one() } }
            Some(b'~') => { self.pos += 1; self.unary().bitnot() }
            Some(b'-') if self.src.get(self.pos + 1) != Some(&b'-') => { self.pos += 1; self.unary().neg() }
            Some(b'+') if self.src.get(self.pos + 1) != Some(&b'+') => { self.pos += 1; self.unary() }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Val {
        self.skip_ws();
        match self.peek() {
            Some(b'(') => {
                self.pos += 1;
                let v = self.ternary();
                self.skip_ws();
                if self.peek() == Some(b')') { self.pos += 1; }
                v
            }
            Some(b'\'') => {
                self.pos += 1;
                let val = if self.peek() == Some(b'\\') {
                    self.pos += 1;
                    match self.src.get(self.pos).copied() {
                        Some(b'n') => { self.pos += 1; b'\n' as u64 }
                        Some(b't') => { self.pos += 1; b'\t' as u64 }
                        Some(b'0') => { self.pos += 1; 0 }
                        Some(b'\\') => { self.pos += 1; b'\\' as u64 }
                        Some(b'\'') => { self.pos += 1; b'\'' as u64 }
                        Some(c) => { self.pos += 1; c as u64 }
                        None => 0,
                    }
                } else {
                    let c = self.src.get(self.pos).copied().unwrap_or(0);
                    self.pos += 1;
                    c as u64
                };
                if self.peek() == Some(b'\'') { self.pos += 1; }
                Val::signed(val as i64)
            }
            Some(c) if c.is_ascii_digit() => {
                self.parse_number()
            }
            Some(c) if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                let start = self.pos;
                while self.pos < self.src.len() && (self.src[self.pos].is_ascii_alphanumeric() || self.src[self.pos] == b'_' || self.src[self.pos] == b'$') {
                    self.pos += 1;
                }
                let ident = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("");
                if ident == "defined" {
                    // By the time ConstEval runs, expand_tokens has already replaced
                    // `defined(X)` with 0 or 1 when in_if_eval mode. If we see `defined`
                    // here it means it appeared in a context where it wasn't processed
                    // (e.g. disabled #if branch). Handle it defensively.
                    self.skip_ws();
                    let has_paren = self.peek() == Some(b'(');
                    if has_paren { self.pos += 1; }
                    self.skip_ws();
                    while self.pos < self.src.len() && (self.src[self.pos].is_ascii_alphanumeric() || self.src[self.pos] == b'_') {
                        self.pos += 1;
                    }
                    if has_paren {
                        self.skip_ws();
                        if self.peek() == Some(b')') { self.pos += 1; }
                    }
                    Val::zero()
                } else {
                    // Unknown identifier = 0
                    Val::zero()
                }
            }
            _ => Val::zero(),
        }
    }

    fn parse_number(&mut self) -> Val {
        let start = self.pos;
        let c = self.src[self.pos];

        let bits: u64;
        let mut is_unsigned = false;

        if c == b'0' && self.src.get(self.pos + 1).is_some_and(|c| *c == b'x' || *c == b'X') {
            // Hexadecimal
            self.pos += 2;
            let hex_start = self.pos;
            while self.pos < self.src.len() && self.src[self.pos].is_ascii_hexdigit() { self.pos += 1; }
            let hex_str = std::str::from_utf8(&self.src[hex_start..self.pos]).unwrap_or("0");
            // Parse as u64; if it doesn't fit in i64 treat as unsigned (GCC rule)
            let v = u64::from_str_radix(hex_str, 16).unwrap_or(0);
            bits = v;
            if v > i64::MAX as u64 { is_unsigned = true; }
        } else if c == b'0' && self.src.get(self.pos + 1).is_some_and(|c| *c == b'b' || *c == b'B') {
            // Binary
            self.pos += 2;
            let bin_start = self.pos;
            while self.pos < self.src.len() && (self.src[self.pos] == b'0' || self.src[self.pos] == b'1') { self.pos += 1; }
            let bin_str = std::str::from_utf8(&self.src[bin_start..self.pos]).unwrap_or("0");
            bits = u64::from_str_radix(bin_str, 2).unwrap_or(0);
        } else if c == b'0' && self.pos + 1 < self.src.len() && self.src[self.pos + 1].is_ascii_digit() {
            // Octal
            self.pos += 1;
            let oct_start = self.pos;
            while self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() { self.pos += 1; }
            let oct_str = std::str::from_utf8(&self.src[oct_start..self.pos]).unwrap_or("0");
            let v = u64::from_str_radix(oct_str, 8).unwrap_or(0);
            bits = v;
            if v > i64::MAX as u64 { is_unsigned = true; }
        } else {
            // Decimal
            while self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() { self.pos += 1; }
            let num_str = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("0");
            // Parse as u64; if it overflows i64, treat as unsigned
            let v: u64 = num_str.parse().unwrap_or(0);
            bits = v;
            if v > i64::MAX as u64 { is_unsigned = true; }
        }

        // Consume type suffixes: u/U makes it unsigned; l/L are ignored
        while self.pos < self.src.len() {
            match self.src[self.pos] {
                b'u' | b'U' => { is_unsigned = true; self.pos += 1; }
                b'l' | b'L' => { self.pos += 1; }
                b'f' | b'F' => { self.pos += 1; } // floating suffix, shouldn't appear but be safe
                _ => break,
            }
        }

        Val { bits, unsigned: is_unsigned }
    }
}

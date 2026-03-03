use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub struct SourceLoc {
    pub file: String,
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub loc: SourceLoc,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Literals
    IntLit(i128),
    UIntLit(u128),
    FloatLit(f64, bool), // (value, is_float_suffix)
    CharLit(i8),
    StringLit(Vec<u8>),
    WideStringLit(Vec<u8>),

    // Identifier
    Ident(String),

    // Keywords
    Auto, Break, Case, Char, Const, Continue, Default, Do, Double, Else,
    Enum, Extern, Float, For, Goto, If, Int, Long, Register, Return,
    Short, Signed, Sizeof, Static, Struct, Switch, Typedef, Union,
    Unsigned, Void, Volatile, While, Restrict, Inline, Bool,
    // GNU extensions
    Typeof, Asm, Extension, Builtin(String),
    Alignof, Alignas,
    Int128,
    // C99
    VaArg,

    // Punctuation
    LParen, RParen, LBrace, RBrace, LBracket, RBracket,
    Semi, Comma, Dot, Arrow, Ellipsis,
    Plus, Minus, Star, Slash, Percent,
    Amp, Pipe, Caret, Tilde, Bang,
    Shl, Shr,
    Lt, Gt, Le, Ge, EqEq, Ne,
    AmpAmp, PipePipe,
    Eq, PlusEq, MinusEq, StarEq, SlashEq, PercentEq,
    AmpEq, PipeEq, CaretEq, ShlEq, ShrEq,
    PlusPlus, MinusMinus,
    Question, Colon,
    Hash, HashHash,

    Eof,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenKind::IntLit(v) => write!(f, "{v}"),
            TokenKind::UIntLit(v) => write!(f, "{v}u"),
            TokenKind::FloatLit(v, _) => write!(f, "{v}"),
            TokenKind::CharLit(v) => write!(f, "'{}'", *v as u8 as char),
            TokenKind::StringLit(_) => write!(f, "<string>"),
            TokenKind::Ident(s) => write!(f, "{s}"),
            TokenKind::LParen => write!(f, "("),
            TokenKind::RParen => write!(f, ")"),
            TokenKind::LBrace => write!(f, "{{"),
            TokenKind::RBrace => write!(f, "}}"),
            TokenKind::LBracket => write!(f, "["),
            TokenKind::RBracket => write!(f, "]"),
            TokenKind::Semi => write!(f, ";"),
            TokenKind::Comma => write!(f, ","),
            TokenKind::Eq => write!(f, "="),
            TokenKind::Eof => write!(f, "<eof>"),
            other => write!(f, "{other:?}"),
        }
    }
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    file: String,
    line: u32,
    col: u32,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str, file: &str) -> Self {
        Self { src: src.as_bytes(), pos: 0, file: file.to_string(), line: 1, col: 1 }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.src.get(self.pos + 1).copied()
    }

    fn advance(&mut self) -> u8 {
        let ch = self.src[self.pos];
        self.pos += 1;
        if ch == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        ch
    }

    fn loc(&self) -> SourceLoc {
        SourceLoc { file: self.file.clone(), line: self.line, col: self.col }
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => { self.advance(); }
                Some(b'/') if self.peek2() == Some(b'/') => {
                    while self.peek().is_some_and(|c| c != b'\n') { self.advance(); }
                }
                Some(b'/') if self.peek2() == Some(b'*') => {
                    self.advance(); self.advance();
                    loop {
                        match self.peek() {
                            None => break,
                            Some(b'*') if self.peek2() == Some(b'/') => {
                                self.advance(); self.advance(); break;
                            }
                            _ => { self.advance(); }
                        }
                    }
                }
                // Handle line continuations
                Some(b'\\') if self.peek2() == Some(b'\n') => {
                    self.advance(); self.advance();
                }
                _ => break,
            }
        }
    }

    fn read_ident(&mut self) -> String {
        let mut ident = String::new();
        loop {
            if self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$') {
                ident.push(self.advance() as char);
            } else if self.peek() == Some(b'\\') && self.peek2() == Some(b'\n') {
                // Line continuation mid-identifier
                self.advance(); self.advance();
            } else {
                break;
            }
        }
        ident
    }

    fn read_number(&mut self) -> TokenKind {
        let start = self.pos;
        let mut is_float = false;
        let mut is_hex = false;
        let mut is_binary = false;

        // Hex prefix
        if self.peek() == Some(b'0') && self.peek2().is_some_and(|c| c == b'x' || c == b'X') {
            self.advance(); self.advance();
            is_hex = true;
            while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) { self.advance(); }
            // Hex float: 0x1.2p3
            if self.peek() == Some(b'.') {
                is_float = true;
                self.advance();
                while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) { self.advance(); }
            }
            if self.peek().is_some_and(|c| c == b'p' || c == b'P') {
                is_float = true;
                self.advance();
                if self.peek().is_some_and(|c| c == b'+' || c == b'-') { self.advance(); }
                while self.peek().is_some_and(|c| c.is_ascii_digit()) { self.advance(); }
            }
        }
        // Binary prefix
        else if self.peek() == Some(b'0') && self.peek2().is_some_and(|c| c == b'b' || c == b'B') {
            self.advance(); self.advance();
            is_binary = true;
            while self.peek().is_some_and(|c| c == b'0' || c == b'1') { self.advance(); }
        }
        // Octal or decimal
        else {
            while self.peek().is_some_and(|c| c.is_ascii_digit()) { self.advance(); }
            if self.peek() == Some(b'.') {
                is_float = true;
                self.advance();
                while self.peek().is_some_and(|c| c.is_ascii_digit()) { self.advance(); }
            }
            if self.peek().is_some_and(|c| c == b'e' || c == b'E') {
                is_float = true;
                self.advance();
                if self.peek().is_some_and(|c| c == b'+' || c == b'-') { self.advance(); }
                while self.peek().is_some_and(|c| c.is_ascii_digit()) { self.advance(); }
            }
        }

        let text = String::from_utf8_lossy(&self.src[start..self.pos]).into_owned();

        // Suffixes
        let mut unsigned = false;
        let mut float_suffix = false;
        loop {
            match self.peek() {
                Some(b'u' | b'U') => { unsigned = true; self.advance(); }
                Some(b'l' | b'L') => { self.advance(); }
                Some(b'f' | b'F') => { float_suffix = true; self.advance(); }
                _ => break,
            }
        }

        if is_float || float_suffix {
            let v: f64 = if is_hex {
                // Parse hex float like 0x1.921fb6p+1
                parse_hex_float(&text)
            } else {
                text.parse().expect("lexer produced invalid float literal")
            };
            let v = if float_suffix { (v as f32) as f64 } else { v };
            return TokenKind::FloatLit(v, float_suffix);
        }

        let value = if is_hex {
            let hex_str = &text[2..]; // skip "0x"
            u128::from_str_radix(hex_str, 16).expect("lexer produced invalid hex literal")
        } else if is_binary {
            let bin_str = &text[2..]; // skip "0b"
            u128::from_str_radix(bin_str, 2).expect("lexer produced invalid binary literal")
        } else if text.starts_with('0') && text.len() > 1 {
            u128::from_str_radix(&text, 8).expect("lexer produced invalid octal literal")
        } else {
            text.parse::<u128>().expect("lexer produced invalid decimal literal")
        };

        if unsigned {
            TokenKind::UIntLit(value)
        } else {
            TokenKind::IntLit(value as i128)
        }
    }

    fn read_char_lit(&mut self) -> i8 {
        self.advance(); // skip opening '
        let val = if self.peek() == Some(b'\\') {
            self.advance();
            self.read_escape() as i8
        } else {
            let c = self.advance();
            c as i8
        };
        if self.peek() == Some(b'\'') { self.advance(); }
        val
    }

    fn read_string_lit(&mut self) -> Vec<u8> {
        self.advance(); // skip opening "
        let mut buf = Vec::new();
        loop {
            match self.peek() {
                None | Some(b'"') => { if self.peek().is_some() { self.advance(); } break; }
                Some(b'\\') => {
                    self.advance();
                    if self.peek() == Some(b'u') || self.peek() == Some(b'U') {
                        let digits = if self.advance() == b'U' { 8 } else { 4 };
                        let cp = self.read_hex_digits(digits);
                        Self::encode_utf8(cp, &mut buf);
                    } else {
                        buf.push(self.read_escape());
                    }
                }
                Some(_) => buf.push(self.advance()),
            }
        }
        buf
    }

    fn read_hex_digits(&mut self, count: usize) -> u32 {
        let mut val = 0u32;
        for _ in 0..count {
            if self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                let d = self.advance();
                let nibble = if d >= b'a' { (d - b'a' + 10) as u32 }
                    else if d >= b'A' { (d - b'A' + 10) as u32 }
                    else { (d - b'0') as u32 };
                val = val * 16 + nibble;
            }
        }
        val
    }

    fn encode_utf8(cp: u32, buf: &mut Vec<u8>) {
        if cp < 0x80 {
            buf.push(cp as u8);
        } else if cp < 0x800 {
            buf.push((0xC0 | (cp >> 6)) as u8);
            buf.push((0x80 | (cp & 0x3F)) as u8);
        } else if cp < 0x10000 {
            buf.push((0xE0 | (cp >> 12)) as u8);
            buf.push((0x80 | ((cp >> 6) & 0x3F)) as u8);
            buf.push((0x80 | (cp & 0x3F)) as u8);
        } else {
            buf.push((0xF0 | (cp >> 18)) as u8);
            buf.push((0x80 | ((cp >> 12) & 0x3F)) as u8);
            buf.push((0x80 | ((cp >> 6) & 0x3F)) as u8);
            buf.push((0x80 | (cp & 0x3F)) as u8);
        }
    }

    fn read_escape(&mut self) -> u8 {
        match self.advance() {
            b'n' => b'\n',
            b't' => b'\t',
            b'r' => b'\r',
            c @ b'0'..=b'7' => {
                let mut val = c - b'0';
                for _ in 0..2 {
                    if self.peek().is_some_and(|c| c >= b'0' && c <= b'7') {
                        val = val.wrapping_mul(8).wrapping_add(self.advance() - b'0');
                    }
                }
                val
            }
            b'x' => {
                let mut val = 0u8;
                while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                    let d = self.advance();
                    let nibble = if d >= b'a' { d - b'a' + 10 }
                        else if d >= b'A' { d - b'A' + 10 }
                        else { d - b'0' };
                    val = val.wrapping_mul(16).wrapping_add(nibble);
                }
                val
            }
            b'\\' => b'\\',
            b'\'' => b'\'',
            b'"' => b'"',
            b'a' => 7,
            b'b' => 8,
            b'f' => 12,
            b'v' => 11,
            b'?' => b'?',
            c => c,
        }
    }

    /// Try to process a preprocessor line directive (`# <line> "file" ...`).
    /// The `#` must already be consumed. Returns true if a directive was found.
    fn try_skip_line_directive(&mut self) -> bool {
        self.skip_whitespace_and_comments();
        if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
            return false;
        }
        let mut line_str = String::new();
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            line_str.push(self.advance() as char);
        }
        if let Ok(line) = line_str.parse::<u32>() {
            self.line = line;
        }
        self.skip_whitespace_and_comments();
        if self.peek() == Some(b'"') {
            self.advance();
            let mut fname = String::new();
            while self.peek().is_some_and(|c| c != b'"') {
                fname.push(self.advance() as char);
            }
            if self.peek() == Some(b'"') { self.advance(); }
            self.file = fname;
        }
        while self.peek().is_some_and(|c| c != b'\n') { self.advance(); }
        true
    }

    /// Read a string literal with adjacent string concatenation,
    /// skipping any line directives between concatenated strings.
    fn read_string_with_concat(&mut self) -> TokenKind {
        let mut s = self.read_string_lit();
        loop {
            self.skip_whitespace_and_comments();
            // Skip line directives that appear between adjacent strings
            if self.peek() == Some(b'#') && self.col == 1 {
                let saved_pos = self.pos;
                let saved_line = self.line;
                let saved_col = self.col;
                let saved_file = self.file.clone();
                self.advance();
                if self.try_skip_line_directive() {
                    continue;
                }
                self.pos = saved_pos;
                self.line = saved_line;
                self.col = saved_col;
                self.file = saved_file;
            }
            if self.peek() == Some(b'"') {
                s.extend(self.read_string_lit());
            } else {
                break;
            }
        }
        TokenKind::StringLit(s)
    }

    /// Classify an identifier as a keyword, builtin, or plain identifier.
    fn classify_ident(&mut self, ident: String) -> TokenKind {
        match ident.as_str() {
            "auto" => TokenKind::Auto,
            "break" => TokenKind::Break,
            "case" => TokenKind::Case,
            "char" => TokenKind::Char,
            "const" => TokenKind::Const,
            "continue" => TokenKind::Continue,
            "default" => TokenKind::Default,
            "do" => TokenKind::Do,
            "double" => TokenKind::Double,
            "else" => TokenKind::Else,
            "enum" => TokenKind::Enum,
            "extern" => TokenKind::Extern,
            "float" => TokenKind::Float,
            "for" => TokenKind::For,
            "goto" => TokenKind::Goto,
            "if" => TokenKind::If,
            "int" => TokenKind::Int,
            "long" => TokenKind::Long,
            "register" => TokenKind::Register,
            "return" => TokenKind::Return,
            "short" => TokenKind::Short,
            "signed" => TokenKind::Signed,
            "sizeof" => TokenKind::Sizeof,
            "static" => TokenKind::Static,
            "struct" => TokenKind::Struct,
            "switch" => TokenKind::Switch,
            "typedef" => TokenKind::Typedef,
            "union" => TokenKind::Union,
            "unsigned" => TokenKind::Unsigned,
            "void" => TokenKind::Void,
            "volatile" => TokenKind::Volatile,
            "while" => TokenKind::While,
            "restrict" | "__restrict" | "__restrict__" => TokenKind::Restrict,
            "inline" | "__inline" | "__inline__" => TokenKind::Inline,
            "_Bool" => TokenKind::Bool,
            "typeof" | "__typeof" | "__typeof__" => TokenKind::Typeof,
            "__asm" | "__asm__" | "asm" => TokenKind::Asm,
            "__extension__" => TokenKind::Extension,
            "__builtin_va_arg" | "va_arg" => TokenKind::VaArg,
            "__alignof" | "__alignof__" | "_Alignof" => TokenKind::Alignof,
            "_Alignas" => TokenKind::Alignas,
            "__int128" | "__int128_t" => TokenKind::Int128,
            "_Float16" => TokenKind::Float, // treat as float
            "L" if self.peek() == Some(b'"') => {
                let s = self.read_string_lit();
                TokenKind::WideStringLit(s)
            }
            "L" if self.peek() == Some(b'\'') => {
                TokenKind::CharLit(self.read_char_lit())
            }
            "_Generic" => TokenKind::Builtin("_Generic".into()),
            "__builtin_offsetof" | "__builtin_expect" | "__builtin_constant_p"
            | "__builtin_choose_expr" | "__builtin_types_compatible_p"
            | "__builtin_frame_address" | "__builtin_return_address"
            | "__builtin_unreachable" | "__builtin_va_end"
            | "__builtin_va_start" | "__builtin_va_copy" => TokenKind::Builtin(ident),
            _ => TokenKind::Ident(ident),
        }
    }

    pub fn tokenize(mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            let loc = self.loc();
            let kind = match self.peek() {
                None => { tokens.push(Token { kind: TokenKind::Eof, loc }); break; }

                Some(b'#') if loc.col == 1 => {
                    self.advance();
                    if self.try_skip_line_directive() { continue; }
                    TokenKind::Hash
                }

                Some(b'\'') => TokenKind::CharLit(self.read_char_lit()),
                Some(b'"') => self.read_string_with_concat(),

                Some(c) if c.is_ascii_digit() => self.read_number(),
                Some(b'.') if self.peek2().is_some_and(|c| c.is_ascii_digit()) => self.read_number(),

                Some(c) if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                    let ident = self.read_ident();
                    self.classify_ident(ident)
                }

                // Punctuation
                Some(b'(') => { self.advance(); TokenKind::LParen }
                Some(b')') => { self.advance(); TokenKind::RParen }
                Some(b'{') => { self.advance(); TokenKind::LBrace }
                Some(b'}') => { self.advance(); TokenKind::RBrace }
                Some(b'[') => { self.advance(); TokenKind::LBracket }
                Some(b']') => { self.advance(); TokenKind::RBracket }
                Some(b';') => { self.advance(); TokenKind::Semi }
                Some(b',') => { self.advance(); TokenKind::Comma }
                Some(b'~') => { self.advance(); TokenKind::Tilde }
                Some(b'?') => { self.advance(); TokenKind::Question }
                Some(b':') => { self.advance(); TokenKind::Colon }

                Some(b'.') => {
                    self.advance();
                    if self.peek() == Some(b'.') && self.peek2() == Some(b'.') {
                        self.advance(); self.advance();
                        TokenKind::Ellipsis
                    } else {
                        TokenKind::Dot
                    }
                }

                Some(b'+') => {
                    self.advance();
                    match self.peek() {
                        Some(b'+') => { self.advance(); TokenKind::PlusPlus }
                        Some(b'=') => { self.advance(); TokenKind::PlusEq }
                        _ => TokenKind::Plus,
                    }
                }
                Some(b'-') => {
                    self.advance();
                    match self.peek() {
                        Some(b'-') => { self.advance(); TokenKind::MinusMinus }
                        Some(b'=') => { self.advance(); TokenKind::MinusEq }
                        Some(b'>') => { self.advance(); TokenKind::Arrow }
                        _ => TokenKind::Minus,
                    }
                }
                Some(b'*') => { self.advance(); if self.peek() == Some(b'=') { self.advance(); TokenKind::StarEq } else { TokenKind::Star } }
                Some(b'/') => { self.advance(); if self.peek() == Some(b'=') { self.advance(); TokenKind::SlashEq } else { TokenKind::Slash } }
                Some(b'%') => { self.advance(); if self.peek() == Some(b'=') { self.advance(); TokenKind::PercentEq } else { TokenKind::Percent } }
                Some(b'&') => {
                    self.advance();
                    match self.peek() {
                        Some(b'&') => { self.advance(); TokenKind::AmpAmp }
                        Some(b'=') => { self.advance(); TokenKind::AmpEq }
                        _ => TokenKind::Amp,
                    }
                }
                Some(b'|') => {
                    self.advance();
                    match self.peek() {
                        Some(b'|') => { self.advance(); TokenKind::PipePipe }
                        Some(b'=') => { self.advance(); TokenKind::PipeEq }
                        _ => TokenKind::Pipe,
                    }
                }
                Some(b'^') => { self.advance(); if self.peek() == Some(b'=') { self.advance(); TokenKind::CaretEq } else { TokenKind::Caret } }
                Some(b'!') => { self.advance(); if self.peek() == Some(b'=') { self.advance(); TokenKind::Ne } else { TokenKind::Bang } }
                Some(b'=') => { self.advance(); if self.peek() == Some(b'=') { self.advance(); TokenKind::EqEq } else { TokenKind::Eq } }
                Some(b'<') => {
                    self.advance();
                    match self.peek() {
                        Some(b'<') => { self.advance(); if self.peek() == Some(b'=') { self.advance(); TokenKind::ShlEq } else { TokenKind::Shl } }
                        Some(b'=') => { self.advance(); TokenKind::Le }
                        _ => TokenKind::Lt,
                    }
                }
                Some(b'>') => {
                    self.advance();
                    match self.peek() {
                        Some(b'>') => { self.advance(); if self.peek() == Some(b'=') { self.advance(); TokenKind::ShrEq } else { TokenKind::Shr } }
                        Some(b'=') => { self.advance(); TokenKind::Ge }
                        _ => TokenKind::Gt,
                    }
                }
                Some(b'#') => {
                    self.advance();
                    if self.peek() == Some(b'#') { self.advance(); TokenKind::HashHash } else { TokenKind::Hash }
                }

                Some(c) => {
                    self.advance();
                    panic!("unexpected character '{}' (0x{:02x})", c as char, c);
                }
            };
            tokens.push(Token { kind, loc });
        }
        tokens
    }
}

fn parse_hex_float(text: &str) -> f64 {
    // Parse C hex float: 0x1.921fb6p+1
    let s = &text[2..]; // skip "0x"
    let (mantissa_str, exp_str) = if let Some(p) = s.find(|c: char| c == 'p' || c == 'P') {
        (&s[..p], &s[p+1..])
    } else {
        (s, "0")
    };
    let exp: i32 = exp_str.parse().unwrap_or(0);
    let (int_part, frac_part) = if let Some(dot) = mantissa_str.find('.') {
        (&mantissa_str[..dot], &mantissa_str[dot+1..])
    } else {
        (mantissa_str, "")
    };
    // Handle arbitrarily large hex mantissas by parsing up to 16 significant digits
    // and adjusting the exponent for any remaining digits.
    let int_val = if int_part.len() <= 16 {
        u64::from_str_radix(int_part, 16).unwrap_or(0) as f64
    } else {
        let significant = u64::from_str_radix(&int_part[..16], 16).unwrap_or(0) as f64;
        let extra_digits = int_part.len() - 16;
        significant * 16f64.powi(extra_digits as i32)
    };
    let frac_val = if frac_part.is_empty() {
        0.0
    } else if frac_part.len() <= 16 {
        let frac_int = u64::from_str_radix(frac_part, 16).unwrap_or(0) as f64;
        frac_int / 16f64.powi(frac_part.len() as i32)
    } else {
        let frac_int = u64::from_str_radix(&frac_part[..16], 16).unwrap_or(0) as f64;
        frac_int / 16f64.powi(16)
    };
    (int_val + frac_val) * 2f64.powi(exp)
}

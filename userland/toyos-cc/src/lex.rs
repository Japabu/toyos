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
    FloatLit(f64),
    CharLit(i8),
    StringLit(Vec<u8>),

    // Identifier
    Ident(String),

    // Keywords
    Auto, Break, Case, Char, Const, Continue, Default, Do, Double, Else,
    Enum, Extern, Float, For, Goto, If, Int, Long, Register, Return,
    Short, Signed, Sizeof, Static, Struct, Switch, Typedef, Union,
    Unsigned, Void, Volatile, While, Restrict, Inline, Bool,
    // GNU extensions
    Typeof, Asm, Attribute, Extension, Builtin(String),
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
            TokenKind::FloatLit(v) => write!(f, "{v}"),
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
            if self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') {
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

        // Hex prefix
        if self.peek() == Some(b'0') && self.peek2().is_some_and(|c| c == b'x' || c == b'X') {
            self.advance(); self.advance();
            is_hex = true;
            while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) { self.advance(); }
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
        let mut long_count = 0u8;
        let mut float_suffix = false;
        loop {
            match self.peek() {
                Some(b'u' | b'U') => { unsigned = true; self.advance(); }
                Some(b'l' | b'L') => { long_count += 1; self.advance(); }
                Some(b'f' | b'F') => { float_suffix = true; self.advance(); }
                _ => break,
            }
        }

        if is_float || float_suffix {
            let v: f64 = text.parse().unwrap_or(0.0);
            return TokenKind::FloatLit(v);
        }

        let value = if is_hex {
            let hex_str = &text[2..]; // skip "0x"
            u128::from_str_radix(hex_str, 16).unwrap_or(0)
        } else if text.starts_with('0') && text.len() > 1 {
            u128::from_str_radix(&text, 8).unwrap_or(0)
        } else {
            text.parse::<u128>().unwrap_or(0)
        };

        if unsigned || long_count >= 2 {
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
                Some(b'\\') => { self.advance(); buf.push(self.read_escape()); }
                Some(_) => buf.push(self.advance()),
            }
        }
        buf
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

    pub fn tokenize(mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            let loc = self.loc();
            let kind = match self.peek() {
                None => { tokens.push(Token { kind: TokenKind::Eof, loc }); break; }

                // Handle #line directives from preprocessor
                Some(b'#') if loc.col == 1 => {
                    self.advance();
                    self.skip_whitespace_and_comments();
                    // Check for # <line> "file" preprocessor line markers
                    if self.peek().is_some_and(|c| c.is_ascii_digit()) {
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
                        continue;
                    }
                    // Otherwise just # token
                    TokenKind::Hash
                }

                Some(b'\'') => TokenKind::CharLit(self.read_char_lit()),
                Some(b'"') => {
                    let mut s = self.read_string_lit();
                    // Adjacent string concatenation (skip line directives between strings)
                    loop {
                        self.skip_whitespace_and_comments();
                        // Skip # <num> "file" line directives that appear between strings
                        if self.peek() == Some(b'#') && self.col == 1 {
                            let saved = self.pos;
                            let saved_line = self.line;
                            let saved_col = self.col;
                            let saved_file = self.file.clone();
                            self.advance(); // skip #
                            self.skip_whitespace_and_comments();
                            if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                                // Line directive — process it
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
                                continue; // retry concatenation after directive
                            } else {
                                // Not a line directive — restore position
                                self.pos = saved;
                                self.line = saved_line;
                                self.col = saved_col;
                                self.file = saved_file;
                            }
                        }
                        if self.peek() == Some(b'"') {
                            s.extend(self.read_string_lit());
                        } else {
                            break;
                        }
                    }
                    TokenKind::StringLit(s)
                }

                Some(c) if c.is_ascii_digit() => self.read_number(),
                Some(b'.') if self.peek2().is_some_and(|c| c.is_ascii_digit()) => self.read_number(),

                Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                    let ident = self.read_ident();
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
                        "__attribute" | "__attribute__" => TokenKind::Attribute,
                        "__extension__" => TokenKind::Extension,
                        "__builtin_va_arg" | "va_arg" => TokenKind::VaArg,
                        "__alignof" | "__alignof__" | "_Alignof" => TokenKind::Alignof,
                        "_Alignas" => TokenKind::Alignas,
                        "__int128" | "__int128_t" => TokenKind::Int128,
                        "_Float16" => TokenKind::Float, // treat as float
                        // L"..." wide strings - treat as regular strings for now
                        "L" if self.peek() == Some(b'"') => {
                            let s = self.read_string_lit();
                            TokenKind::StringLit(s)
                        }
                        "L" if self.peek() == Some(b'\'') => {
                            TokenKind::CharLit(self.read_char_lit())
                        }
                        "__builtin_offsetof" | "__builtin_expect" | "__builtin_constant_p"
                        | "__builtin_choose_expr" | "__builtin_types_compatible_p"
                        | "__builtin_frame_address" | "__builtin_return_address"
                        | "__builtin_unreachable" | "__builtin_va_end"
                        | "__builtin_va_start" | "__builtin_va_copy" => TokenKind::Builtin(ident),
                        _ => TokenKind::Ident(ident),
                    }
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
                    eprintln!("warning: unexpected character '{}' (0x{:02x})", c as char, c);
                    continue;
                }
            };
            tokens.push(Token { kind, loc });
        }
        tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        Lexer::new(src, "<test>").tokenize().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn empty_input() {
        assert_eq!(kinds(""), vec![TokenKind::Eof]);
    }

    #[test]
    fn integer_literals() {
        assert_eq!(kinds("42")[0], TokenKind::IntLit(42));
        assert_eq!(kinds("0")[0], TokenKind::IntLit(0));
        assert_eq!(kinds("0xff")[0], TokenKind::IntLit(255));
        assert_eq!(kinds("0xFF")[0], TokenKind::IntLit(255));
        assert_eq!(kinds("077")[0], TokenKind::IntLit(63)); // octal
        assert_eq!(kinds("42u")[0], TokenKind::UIntLit(42));
        assert_eq!(kinds("42ULL")[0], TokenKind::UIntLit(42));
        assert_eq!(kinds("42L")[0], TokenKind::IntLit(42));
    }

    #[test]
    fn float_literals() {
        assert!(matches!(kinds("3.14")[0], TokenKind::FloatLit(v) if (v - 3.14).abs() < 0.001));
        assert!(matches!(kinds("1e10")[0], TokenKind::FloatLit(_)));
        assert!(matches!(kinds("1.0f")[0], TokenKind::FloatLit(_)));
        assert!(matches!(kinds(".5")[0], TokenKind::FloatLit(v) if (v - 0.5).abs() < 0.001));
    }

    #[test]
    fn char_literals() {
        assert_eq!(kinds("'a'")[0], TokenKind::CharLit(b'a' as i8));
        assert_eq!(kinds("'\\n'")[0], TokenKind::CharLit(b'\n' as i8));
        assert_eq!(kinds("'\\0'")[0], TokenKind::CharLit(0));
        assert_eq!(kinds("'\\t'")[0], TokenKind::CharLit(b'\t' as i8));
        assert_eq!(kinds("'\\\\'")[0], TokenKind::CharLit(b'\\' as i8));
    }

    #[test]
    fn string_literals() {
        assert_eq!(kinds("\"hello\"")[0], TokenKind::StringLit(b"hello".to_vec()));
        assert_eq!(kinds("\"\\n\"")[0], TokenKind::StringLit(b"\n".to_vec()));
        // Adjacent string concatenation
        assert_eq!(kinds("\"ab\" \"cd\"")[0], TokenKind::StringLit(b"abcd".to_vec()));
    }

    #[test]
    fn keywords() {
        assert_eq!(kinds("int")[0], TokenKind::Int);
        assert_eq!(kinds("return")[0], TokenKind::Return);
        assert_eq!(kinds("void")[0], TokenKind::Void);
        assert_eq!(kinds("if")[0], TokenKind::If);
        assert_eq!(kinds("else")[0], TokenKind::Else);
        assert_eq!(kinds("while")[0], TokenKind::While);
        assert_eq!(kinds("for")[0], TokenKind::For);
        assert_eq!(kinds("struct")[0], TokenKind::Struct);
        assert_eq!(kinds("typedef")[0], TokenKind::Typedef);
        assert_eq!(kinds("sizeof")[0], TokenKind::Sizeof);
        assert_eq!(kinds("unsigned")[0], TokenKind::Unsigned);
        assert_eq!(kinds("signed")[0], TokenKind::Signed);
        assert_eq!(kinds("static")[0], TokenKind::Static);
        assert_eq!(kinds("extern")[0], TokenKind::Extern);
    }

    #[test]
    fn gnu_keywords() {
        assert_eq!(kinds("typeof")[0], TokenKind::Typeof);
        assert_eq!(kinds("__typeof__")[0], TokenKind::Typeof);
        assert_eq!(kinds("__attribute__")[0], TokenKind::Attribute);
        assert_eq!(kinds("__extension__")[0], TokenKind::Extension);
        assert_eq!(kinds("__asm__")[0], TokenKind::Asm);
        assert_eq!(kinds("__builtin_va_arg")[0], TokenKind::VaArg);
        assert_eq!(kinds("__int128")[0], TokenKind::Int128);
    }

    #[test]
    fn identifiers() {
        assert_eq!(kinds("foo")[0], TokenKind::Ident("foo".into()));
        assert_eq!(kinds("_bar")[0], TokenKind::Ident("_bar".into()));
        assert_eq!(kinds("x123")[0], TokenKind::Ident("x123".into()));
    }

    #[test]
    fn punctuation() {
        assert_eq!(kinds("(")[0], TokenKind::LParen);
        assert_eq!(kinds(")")[0], TokenKind::RParen);
        assert_eq!(kinds("{")[0], TokenKind::LBrace);
        assert_eq!(kinds("}")[0], TokenKind::RBrace);
        assert_eq!(kinds(";")[0], TokenKind::Semi);
        assert_eq!(kinds(",")[0], TokenKind::Comma);
        assert_eq!(kinds("->")[0], TokenKind::Arrow);
        assert_eq!(kinds("...")[0], TokenKind::Ellipsis);
        assert_eq!(kinds(".")[0], TokenKind::Dot);
    }

    #[test]
    fn operators() {
        assert_eq!(kinds("+")[0], TokenKind::Plus);
        assert_eq!(kinds("-")[0], TokenKind::Minus);
        assert_eq!(kinds("*")[0], TokenKind::Star);
        assert_eq!(kinds("/")[0], TokenKind::Slash);
        assert_eq!(kinds("%")[0], TokenKind::Percent);
        assert_eq!(kinds("++")[0], TokenKind::PlusPlus);
        assert_eq!(kinds("--")[0], TokenKind::MinusMinus);
        assert_eq!(kinds("==")[0], TokenKind::EqEq);
        assert_eq!(kinds("!=")[0], TokenKind::Ne);
        assert_eq!(kinds("<=")[0], TokenKind::Le);
        assert_eq!(kinds(">=")[0], TokenKind::Ge);
        assert_eq!(kinds("<<")[0], TokenKind::Shl);
        assert_eq!(kinds(">>")[0], TokenKind::Shr);
        assert_eq!(kinds("&&")[0], TokenKind::AmpAmp);
        assert_eq!(kinds("||")[0], TokenKind::PipePipe);
        assert_eq!(kinds("+=")[0], TokenKind::PlusEq);
        assert_eq!(kinds("-=")[0], TokenKind::MinusEq);
        assert_eq!(kinds("*=")[0], TokenKind::StarEq);
        assert_eq!(kinds("<<=")[0], TokenKind::ShlEq);
        assert_eq!(kinds(">>=")[0], TokenKind::ShrEq);
    }

    #[test]
    fn comments() {
        let toks = kinds("42 // this is a comment\n7");
        assert_eq!(toks[0], TokenKind::IntLit(42));
        assert_eq!(toks[1], TokenKind::IntLit(7));

        let toks = kinds("42 /* comment */ 7");
        assert_eq!(toks[0], TokenKind::IntLit(42));
        assert_eq!(toks[1], TokenKind::IntLit(7));
    }

    #[test]
    fn line_continuations() {
        let toks = kinds("in\\\nt");
        assert_eq!(toks[0], TokenKind::Int);
    }

    #[test]
    fn full_function() {
        let toks = kinds("int main(void) { return 42; }");
        assert_eq!(toks[0], TokenKind::Int);
        assert_eq!(toks[1], TokenKind::Ident("main".into()));
        assert_eq!(toks[2], TokenKind::LParen);
        assert_eq!(toks[3], TokenKind::Void);
        assert_eq!(toks[4], TokenKind::RParen);
        assert_eq!(toks[5], TokenKind::LBrace);
        assert_eq!(toks[6], TokenKind::Return);
        assert_eq!(toks[7], TokenKind::IntLit(42));
        assert_eq!(toks[8], TokenKind::Semi);
        assert_eq!(toks[9], TokenKind::RBrace);
        assert_eq!(toks[10], TokenKind::Eof);
    }

    #[test]
    fn hex_escape_in_string() {
        assert_eq!(kinds("\"\\x41\"")[0], TokenKind::StringLit(b"A".to_vec()));
    }

    #[test]
    fn wide_string() {
        assert_eq!(kinds("L\"hello\"")[0], TokenKind::StringLit(b"hello".to_vec()));
    }

    #[test]
    fn source_locations() {
        let tokens = Lexer::new("int\nmain", "<test>").tokenize();
        assert_eq!(tokens[0].loc.line, 1);
        assert_eq!(tokens[1].loc.line, 2);
    }

    #[test]
    fn string_concat_across_line_directive() {
        // Line directives between adjacent strings must not break concatenation
        let toks = kinds("\"hello\" \n# 5 \"other.c\"\n\"world\"");
        assert_eq!(toks[0], TokenKind::StringLit(b"helloworld".to_vec()));
    }

    #[test]
    fn octal_escape_in_string() {
        let toks = kinds("\"\\0\\12\\101\"");
        assert_eq!(toks[0], TokenKind::StringLit(vec![0, 10, 65])); // NUL, newline, 'A'
    }
}

mod decl;
mod expr;
mod stmt;

use crate::ast::*;
use crate::lex::{Token, TokenKind};
use crate::types::TypeEnv;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    pub type_env: TypeEnv,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0, type_env: TypeEnv::new() }
    }

    fn peek(&self) -> &TokenKind {
        self.tokens.get(self.pos).map(|t| &t.kind).unwrap_or(&TokenKind::Eof)
    }

    fn peek2(&self) -> &TokenKind {
        self.tokens.get(self.pos + 1).map(|t| &t.kind).unwrap_or(&TokenKind::Eof)
    }

    fn advance(&mut self) -> &TokenKind {
        let tok = &self.tokens[self.pos].kind;
        self.pos += 1;
        tok
    }

    fn expect(&mut self, kind: &TokenKind) {
        if self.peek() == kind {
            self.advance();
        } else {
            let loc = &self.tokens.get(self.pos).map(|t| &t.loc);
            let context: Vec<_> = self.tokens[self.pos.saturating_sub(5)..std::cmp::min(self.pos+5, self.tokens.len())]
                .iter().map(|t| format!("{:?}@{:?}", t.kind, t.loc)).collect();
            panic!("expected {:?}, got {:?} at {:?}\ncontext: {}", kind, self.peek(), loc, context.join(", "));
        }
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.peek() == kind { self.advance(); true } else { false }
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), TokenKind::Eof)
    }

    fn ident(&mut self) -> String {
        match self.peek().clone() {
            TokenKind::Ident(s) => { self.advance(); s }
            other => panic!("expected identifier, got {:?}", other),
        }
    }

    pub fn parse(mut self) -> (TranslationUnit, TypeEnv) {
        let mut tu = Vec::new();
        while !self.at_eof() {
            if self.eat(&TokenKind::Semi) { continue; }
            if matches!(self.peek(), TokenKind::Extension) { self.advance(); }
            tu.push(self.external_decl());
        }
        let env = self.type_env;
        (tu, env)
    }
}

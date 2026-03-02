use crate::ast::*;
use crate::lex::TokenKind;
use super::Parser;

impl Parser {
    pub(super) fn stmt(&mut self) -> Statement {
        match self.peek() {
            TokenKind::LBrace => self.compound_stmt(),
            TokenKind::If => self.if_stmt(),
            TokenKind::While => self.while_stmt(),
            TokenKind::Do => self.do_while_stmt(),
            TokenKind::For => self.for_stmt(),
            TokenKind::Switch => self.switch_stmt(),
            TokenKind::Case => {
                self.advance();
                let val = self.conditional_expr();
                let is_range = self.eat(&TokenKind::Ellipsis);
                let hi = if is_range { Some(self.conditional_expr()) } else { None };
                self.expect(&TokenKind::Colon);
                let body = self.case_body();
                if let Some(hi) = hi {
                    Statement::CaseRange(Box::new(val), Box::new(hi), Box::new(body))
                } else {
                    Statement::Case(Box::new(val), Box::new(body))
                }
            }
            TokenKind::Default => {
                self.advance();
                self.expect(&TokenKind::Colon);
                let body = self.case_body();
                Statement::Default(Box::new(body))
            }
            TokenKind::Return => {
                self.advance();
                let val = if self.peek() == &TokenKind::Semi {
                    None
                } else {
                    Some(self.expr())
                };
                self.expect(&TokenKind::Semi);
                Statement::Return(val)
            }
            TokenKind::Break => { self.advance(); self.expect(&TokenKind::Semi); Statement::Break }
            TokenKind::Continue => { self.advance(); self.expect(&TokenKind::Semi); Statement::Continue }
            TokenKind::Goto => {
                self.advance();
                if self.peek() == &TokenKind::Star {
                    // Computed goto: goto *expr;
                    self.advance();
                    let _e = self.expr();
                    self.expect(&TokenKind::Semi);
                    // Treat as no-op for now (computed gotos are a GCC extension)
                    Statement::Expr(None)
                } else {
                    let label = self.ident();
                    self.expect(&TokenKind::Semi);
                    Statement::Goto(label)
                }
            }
            TokenKind::Asm => self.asm_stmt(),
            TokenKind::Semi => { self.advance(); Statement::Expr(None) }
            // Check for labeled statement
            TokenKind::Ident(_) if matches!(self.peek2(), TokenKind::Colon) => {
                let label = self.ident();
                self.advance(); // :
                let body = self.stmt();
                Statement::Label(label, Box::new(body))
            }
            _ => {
                let e = self.expr();
                self.expect(&TokenKind::Semi);
                Statement::Expr(Some(e))
            }
        }
    }

    /// Collect all statements after a case/default label until the next case/default/end of block.
    fn case_body(&mut self) -> Statement {
        let mut items = Vec::new();
        loop {
            match self.peek() {
                TokenKind::Case | TokenKind::Default | TokenKind::RBrace => break,
                _ => {}
            }
            if matches!(self.peek(), TokenKind::Extension) { self.advance(); }
            if self.is_declaration_start() {
                items.push(BlockItem::Decl(self.local_declaration()));
            } else {
                items.push(BlockItem::Stmt(self.stmt()));
            }
        }
        if items.len() == 1 {
            if let Some(BlockItem::Stmt(_)) = items.first() {
                if let BlockItem::Stmt(s) = items.pop().unwrap() { return s; }
            }
        }
        Statement::Compound(items)
    }

    pub(super) fn compound_stmt(&mut self) -> Statement {
        self.expect(&TokenKind::LBrace);
        let mut items = Vec::new();
        while self.peek() != &TokenKind::RBrace {
            if matches!(self.peek(), TokenKind::Extension) { self.advance(); }
            if self.is_declaration_start() {
                items.push(BlockItem::Decl(self.local_declaration()));
            } else {
                items.push(BlockItem::Stmt(self.stmt()));
            }
        }
        self.expect(&TokenKind::RBrace);
        Statement::Compound(items)
    }

    pub(super) fn local_declaration(&mut self) -> Declaration {
        let specifiers = self.decl_specifiers();
        let mut declarators = Vec::new();

        if self.peek() != &TokenKind::Semi {
            loop {
                let d = self.declarator();
                let init = if self.eat(&TokenKind::Eq) { Some(self.initializer()) } else { None };
                declarators.push(InitDeclarator { declarator: d, initializer: init });
                self.skip_asm_label();
                if !self.eat(&TokenKind::Comma) { break; }
            }
        }
        self.expect(&TokenKind::Semi);

        // Register typedefs
        let is_typedef = specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Typedef)));
        if is_typedef {
            for id in &declarators {
                if let Some(name) = self.declarator_name(&id.declarator) {
                    self.type_env.typedefs.insert(name, crate::types::CType::Int(true));
                }
            }
        }

        Declaration { specifiers, declarators }
    }

    fn if_stmt(&mut self) -> Statement {
        self.advance(); // if
        self.expect(&TokenKind::LParen);
        let cond = self.expr();
        self.expect(&TokenKind::RParen);
        let then = self.stmt();
        let else_ = if self.eat(&TokenKind::Else) { Some(Box::new(self.stmt())) } else { None };
        Statement::If(Box::new(cond), Box::new(then), else_)
    }

    fn while_stmt(&mut self) -> Statement {
        self.advance(); // while
        self.expect(&TokenKind::LParen);
        let cond = self.expr();
        self.expect(&TokenKind::RParen);
        let body = self.stmt();
        Statement::While(Box::new(cond), Box::new(body))
    }

    fn do_while_stmt(&mut self) -> Statement {
        self.advance(); // do
        let body = self.stmt();
        self.expect(&TokenKind::While);
        self.expect(&TokenKind::LParen);
        let cond = self.expr();
        self.expect(&TokenKind::RParen);
        self.expect(&TokenKind::Semi);
        Statement::DoWhile(Box::new(body), Box::new(cond))
    }

    fn for_stmt(&mut self) -> Statement {
        self.advance(); // for
        self.expect(&TokenKind::LParen);
        let init = if self.peek() == &TokenKind::Semi {
            self.advance();
            None
        } else if self.is_declaration_start() {
            let decl = self.local_declaration();
            Some(Box::new(ForInit::Decl(decl)))
        } else {
            let e = self.expr();
            self.expect(&TokenKind::Semi);
            Some(Box::new(ForInit::Expr(e)))
        };
        let cond = if self.peek() == &TokenKind::Semi { None } else { Some(Box::new(self.expr())) };
        self.expect(&TokenKind::Semi);
        let step = if self.peek() == &TokenKind::RParen { None } else { Some(Box::new(self.expr())) };
        self.expect(&TokenKind::RParen);
        let body = self.stmt();
        Statement::For(init, cond, step, Box::new(body))
    }

    fn switch_stmt(&mut self) -> Statement {
        self.advance(); // switch
        self.expect(&TokenKind::LParen);
        let e = self.expr();
        self.expect(&TokenKind::RParen);
        let body = self.stmt();
        Statement::Switch(Box::new(e), Box::new(body))
    }

    fn asm_stmt(&mut self) -> Statement {
        self.advance(); // asm
        let volatile = if matches!(self.peek(), TokenKind::Volatile) { self.advance(); true } else { false };
        self.expect(&TokenKind::LParen);

        // Parse template string
        let template = if let TokenKind::StringLit(s) = self.peek().clone() {
            self.advance();
            String::from_utf8_lossy(&s).into_owned()
        } else {
            String::new()
        };

        let mut outputs = Vec::new();
        let mut inputs = Vec::new();
        let mut clobbers = Vec::new();

        if self.eat(&TokenKind::Colon) {
            // Outputs
            outputs = self.parse_asm_operands();
            if self.eat(&TokenKind::Colon) {
                inputs = self.parse_asm_operands();
                if self.eat(&TokenKind::Colon) {
                    // Clobbers
                    while let TokenKind::StringLit(s) = self.peek().clone() {
                        self.advance();
                        clobbers.push(String::from_utf8_lossy(&s).into_owned());
                        if !self.eat(&TokenKind::Comma) { break; }
                    }
                }
            }
        }

        self.expect(&TokenKind::RParen);
        self.expect(&TokenKind::Semi);

        Statement::Asm(AsmStmt { volatile, template, outputs, inputs, clobbers })
    }

    fn parse_asm_operands(&mut self) -> Vec<AsmOperand> {
        let mut ops = Vec::new();
        while let TokenKind::StringLit(_) = self.peek() {
            if let TokenKind::StringLit(constraint) = self.peek().clone() {
                self.advance();
                let constraint = String::from_utf8_lossy(&constraint).into_owned();
                self.expect(&TokenKind::LParen);
                let e = self.expr();
                self.expect(&TokenKind::RParen);
                ops.push(AsmOperand { constraint, expr: e });
            }
            if !self.eat(&TokenKind::Comma) { break; }
        }
        ops
    }
}

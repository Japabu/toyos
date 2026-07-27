use crate::ast::*;
use crate::lex::TokenKind;
use super::Parser;

impl Parser {
    // Expressions — operator precedence climbing
    pub(super) fn expr(&mut self) -> Expr {
        let e = self.assignment_expr();
        if self.peek() == &TokenKind::Comma {
            self.advance();
            let right = self.expr();
            Expr::Comma(Box::new(e), Box::new(right))
        } else {
            e
        }
    }

    pub(super) fn assignment_expr(&mut self) -> Expr {
        let lhs = self.conditional_expr();
        let op = match self.peek() {
            TokenKind::Eq => AssignOp::Assign,
            TokenKind::PlusEq => AssignOp::AddAssign,
            TokenKind::MinusEq => AssignOp::SubAssign,
            TokenKind::StarEq => AssignOp::MulAssign,
            TokenKind::SlashEq => AssignOp::DivAssign,
            TokenKind::PercentEq => AssignOp::ModAssign,
            TokenKind::ShlEq => AssignOp::ShlAssign,
            TokenKind::ShrEq => AssignOp::ShrAssign,
            TokenKind::AmpEq => AssignOp::AndAssign,
            TokenKind::CaretEq => AssignOp::XorAssign,
            TokenKind::PipeEq => AssignOp::OrAssign,
            _ => return lhs,
        };
        self.advance();
        let rhs = self.assignment_expr();
        Expr::Assign(op, Box::new(lhs), Box::new(rhs))
    }

    pub(super) fn conditional_expr(&mut self) -> Expr {
        let e = self.logor_expr();
        if self.eat(&TokenKind::Question) {
            let then = self.expr();
            self.expect(&TokenKind::Colon);
            let else_ = self.conditional_expr();
            Expr::Conditional(Box::new(e), Box::new(then), Box::new(else_))
        } else {
            e
        }
    }

    fn logor_expr(&mut self) -> Expr {
        let mut e = self.logand_expr();
        while self.peek() == &TokenKind::PipePipe {
            self.advance();
            let r = self.logand_expr();
            e = Expr::Binary(BinOp::LogOr, Box::new(e), Box::new(r));
        }
        e
    }

    fn logand_expr(&mut self) -> Expr {
        let mut e = self.bitor_expr();
        while self.peek() == &TokenKind::AmpAmp {
            self.advance();
            let r = self.bitor_expr();
            e = Expr::Binary(BinOp::LogAnd, Box::new(e), Box::new(r));
        }
        e
    }

    fn bitor_expr(&mut self) -> Expr {
        let mut e = self.bitxor_expr();
        while self.peek() == &TokenKind::Pipe {
            self.advance();
            let r = self.bitxor_expr();
            e = Expr::Binary(BinOp::BitOr, Box::new(e), Box::new(r));
        }
        e
    }

    fn bitxor_expr(&mut self) -> Expr {
        let mut e = self.bitand_expr();
        while self.peek() == &TokenKind::Caret {
            self.advance();
            let r = self.bitand_expr();
            e = Expr::Binary(BinOp::BitXor, Box::new(e), Box::new(r));
        }
        e
    }

    fn bitand_expr(&mut self) -> Expr {
        let mut e = self.equality_expr();
        while self.peek() == &TokenKind::Amp {
            self.advance();
            let r = self.equality_expr();
            e = Expr::Binary(BinOp::BitAnd, Box::new(e), Box::new(r));
        }
        e
    }

    fn equality_expr(&mut self) -> Expr {
        let mut e = self.relational_expr();
        loop {
            let op = match self.peek() {
                TokenKind::EqEq => BinOp::Eq,
                TokenKind::Ne => BinOp::Ne,
                _ => break,
            };
            self.advance();
            let r = self.relational_expr();
            e = Expr::Binary(op, Box::new(e), Box::new(r));
        }
        e
    }

    fn relational_expr(&mut self) -> Expr {
        let mut e = self.shift_expr();
        loop {
            let op = match self.peek() {
                TokenKind::Lt => BinOp::Lt,
                TokenKind::Gt => BinOp::Gt,
                TokenKind::Le => BinOp::Le,
                TokenKind::Ge => BinOp::Ge,
                _ => break,
            };
            self.advance();
            let r = self.shift_expr();
            e = Expr::Binary(op, Box::new(e), Box::new(r));
        }
        e
    }

    fn shift_expr(&mut self) -> Expr {
        let mut e = self.additive_expr();
        loop {
            let op = match self.peek() {
                TokenKind::Shl => BinOp::Shl,
                TokenKind::Shr => BinOp::Shr,
                _ => break,
            };
            self.advance();
            let r = self.additive_expr();
            e = Expr::Binary(op, Box::new(e), Box::new(r));
        }
        e
    }

    fn additive_expr(&mut self) -> Expr {
        let mut e = self.multiplicative_expr();
        loop {
            let op = match self.peek() {
                TokenKind::Plus => BinOp::Add,
                TokenKind::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let r = self.multiplicative_expr();
            e = Expr::Binary(op, Box::new(e), Box::new(r));
        }
        e
    }

    fn multiplicative_expr(&mut self) -> Expr {
        let mut e = self.cast_expr();
        loop {
            let op = match self.peek() {
                TokenKind::Star => BinOp::Mul,
                TokenKind::Slash => BinOp::Div,
                TokenKind::Percent => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let r = self.cast_expr();
            e = Expr::Binary(op, Box::new(e), Box::new(r));
        }
        e
    }

    fn cast_expr(&mut self) -> Expr {
        // Check for (type-name) cast
        if self.peek() == &TokenKind::LParen && self.is_cast() {
            self.advance();
            let tn = self.type_name();
            self.expect(&TokenKind::RParen);

            // Compound literal: (type){initializer-list}
            if self.peek() == &TokenKind::LBrace {
                self.advance();
                let mut items = Vec::new();
                while self.peek() != &TokenKind::RBrace {
                    let mut designators = Vec::new();
                    loop {
                        if self.peek() == &TokenKind::Dot {
                            self.advance();
                            let field = self.ident();
                            designators.push(Designator::Field(field));
                        } else if self.peek() == &TokenKind::LBracket {
                            self.advance();
                            let idx = self.conditional_expr();
                            self.expect(&TokenKind::RBracket);
                            designators.push(Designator::Index(Box::new(idx)));
                        } else {
                            break;
                        }
                    }
                    if !designators.is_empty() {
                        self.expect(&TokenKind::Eq);
                    }
                    let init = self.initializer();
                    let inner_expr = match init {
                        Initializer::Expr(e) => e,
                        Initializer::List(items) => {
                            // Nested brace initializer inside compound literal
                            Expr::CompoundLiteral(Box::new(TypeName { specifiers: Vec::new(), declarator: None }), items)
                        }
                    };
                    items.push(InitializerItem {
                        designators,
                        initializer: Initializer::Expr(inner_expr),
                    });
                    if !self.eat(&TokenKind::Comma) { break; }
                }
                self.expect(&TokenKind::RBrace);
                return Expr::CompoundLiteral(Box::new(tn), items);
            }

            let e = self.cast_expr();
            return Expr::Cast(Box::new(tn), Box::new(e));
        }
        self.unary_expr()
    }

    fn is_cast(&self) -> bool {
        // Save position and check if ( type-name ) follows
        // Look at what follows the (
        let i = self.pos + 1; // skip the (
        // Check if the first token is a type
        if i < self.tokens.len() {
            match &self.tokens[i].kind {
                t if self.is_type_keyword(t) => {}
                TokenKind::Ident(name) if self.type_env.is_typedef(name) => {}
                _ => return false,
            }
        } else {
            return false;
        }
        true
    }

    fn unary_expr(&mut self) -> Expr {
        match self.peek().clone() {
            TokenKind::PlusPlus => {
                self.advance();
                let e = self.unary_expr();
                Expr::Unary(UnaryOp::PreInc, Box::new(e))
            }
            TokenKind::MinusMinus => {
                self.advance();
                let e = self.unary_expr();
                Expr::Unary(UnaryOp::PreDec, Box::new(e))
            }
            TokenKind::AmpAmp => {
                // GCC label address: &&label
                self.advance();
                let label = self.ident();
                Expr::Unary(UnaryOp::AddrOf, Box::new(Expr::Ident(label)))
            }
            TokenKind::Amp => {
                self.advance();
                let e = self.cast_expr();
                Expr::Unary(UnaryOp::AddrOf, Box::new(e))
            }
            TokenKind::Star => {
                self.advance();
                let e = self.cast_expr();
                Expr::Unary(UnaryOp::Deref, Box::new(e))
            }
            TokenKind::Plus => {
                self.advance();
                self.cast_expr() // unary + is identity
            }
            TokenKind::Minus => {
                self.advance();
                let e = self.cast_expr();
                Expr::Unary(UnaryOp::Neg, Box::new(e))
            }
            TokenKind::Tilde => {
                self.advance();
                let e = self.cast_expr();
                Expr::Unary(UnaryOp::BitNot, Box::new(e))
            }
            TokenKind::Bang => {
                self.advance();
                let e = self.cast_expr();
                Expr::Unary(UnaryOp::LogNot, Box::new(e))
            }
            TokenKind::Sizeof => {
                self.advance();
                if self.peek() == &TokenKind::LParen && self.is_sizeof_type() {
                    self.advance();
                    let tn = self.type_name();
                    self.expect(&TokenKind::RParen);
                    Expr::Sizeof(Box::new(SizeofArg::Type(tn)))
                } else {
                    let e = self.unary_expr();
                    Expr::Sizeof(Box::new(SizeofArg::Expr(e)))
                }
            }
            TokenKind::Alignof => {
                self.advance();
                self.expect(&TokenKind::LParen);
                let tn = self.type_name();
                self.expect(&TokenKind::RParen);
                Expr::Alignof(Box::new(tn))
            }
            TokenKind::VaArg => {
                self.advance();
                self.expect(&TokenKind::LParen);
                let e = self.assignment_expr();
                self.expect(&TokenKind::Comma);
                let tn = self.type_name();
                self.expect(&TokenKind::RParen);
                Expr::VaArg(Box::new(e), Box::new(tn))
            }
            TokenKind::Extension => {
                self.advance();
                self.cast_expr()
            }
            _ => self.postfix_expr(),
        }
    }

    fn is_sizeof_type(&self) -> bool {
        let next = self.pos + 1;
        if next >= self.tokens.len() { return false; }
        match &self.tokens[next].kind {
            t if self.is_type_keyword(t) => true,
            TokenKind::Ident(name) => self.type_env.is_typedef(name),
            _ => false,
        }
    }

    fn postfix_expr(&mut self) -> Expr {
        let mut e = self.primary_expr();
        loop {
            match self.peek() {
                TokenKind::LBracket => {
                    self.advance();
                    let idx = self.expr();
                    self.expect(&TokenKind::RBracket);
                    e = Expr::Index(Box::new(e), Box::new(idx));
                }
                TokenKind::LParen => {
                    self.advance();
                    let mut args = Vec::new();
                    while self.peek() != &TokenKind::RParen {
                        args.push(self.assignment_expr());
                        if !self.eat(&TokenKind::Comma) { break; }
                    }
                    self.expect(&TokenKind::RParen);
                    e = Expr::Call(Box::new(e), args);
                }
                TokenKind::Dot => {
                    self.advance();
                    let field = self.ident();
                    e = Expr::Member(Box::new(e), field);
                }
                TokenKind::Arrow => {
                    self.advance();
                    let field = self.ident();
                    e = Expr::Arrow(Box::new(e), field);
                }
                TokenKind::PlusPlus => {
                    self.advance();
                    e = Expr::PostUnary(PostOp::PostInc, Box::new(e));
                }
                TokenKind::MinusMinus => {
                    self.advance();
                    e = Expr::PostUnary(PostOp::PostDec, Box::new(e));
                }
                _ => break,
            }
        }
        e
    }

    fn primary_expr(&mut self) -> Expr {
        match self.peek().clone() {
            TokenKind::IntLit(v) => { self.advance(); Expr::IntLit(v) }
            TokenKind::UIntLit(v) => { self.advance(); Expr::UIntLit(v) }
            TokenKind::FloatLit(v, is_f32) => { self.advance(); Expr::FloatLit(v, is_f32) }
            TokenKind::CharLit(v) => { self.advance(); Expr::CharLit(v) }
            TokenKind::StringLit(s) => { self.advance(); Expr::StringLit(s) }
            TokenKind::WideStringLit(s) => { self.advance(); Expr::WideStringLit(s) }
            TokenKind::Ident(s) => { self.advance(); Expr::Ident(s) }
            TokenKind::LParen => {
                self.advance();
                // Statement expression ({ ... })
                if self.peek() == &TokenKind::LBrace {
                    self.advance();
                    let mut items = Vec::new();
                    while self.peek() != &TokenKind::RBrace {
                        if self.is_declaration_start() {
                            items.push(BlockItem::Decl(self.local_declaration()));
                        } else {
                            items.push(BlockItem::Stmt(self.stmt()));
                        }
                    }
                    self.expect(&TokenKind::RBrace);
                    self.expect(&TokenKind::RParen);
                    Expr::StmtExpr(items)
                } else {
                    let e = self.expr();
                    self.expect(&TokenKind::RParen);
                    e
                }
            }
            TokenKind::Builtin(ref name) if name == "_Generic" => {
                panic!("_Generic type dispatch is not implemented");
            }
            TokenKind::Builtin(ref name) if name == "__builtin_offsetof" => {
                self.advance();
                self.expect(&TokenKind::LParen);
                // First arg is a type name (e.g. `struct Foo`), parse it properly
                let tn = self.type_name();
                // Extract the tag/typedef name for codegen
                let type_name = tn.specifiers.iter().find_map(|s| {
                    if let DeclSpecifier::TypeSpec(ts) = s {
                        match ts {
                            TypeSpec::TypedefName(n) => Some(n.clone()),
                            TypeSpec::Struct(st) => st.name.clone(),
                            TypeSpec::Union(st) => st.name.clone(),
                            TypeSpec::Void | TypeSpec::Char | TypeSpec::Short | TypeSpec::Int
                            | TypeSpec::Long | TypeSpec::Float | TypeSpec::Double
                            | TypeSpec::Signed | TypeSpec::Unsigned | TypeSpec::Bool
                            | TypeSpec::Enum(_) | TypeSpec::Typeof(_) | TypeSpec::TypeofType(_)
                            | TypeSpec::Builtin(_) | TypeSpec::Int128 => None,
                        }
                    } else { None }
                }).expect("__builtin_offsetof: cannot determine type name");
                self.expect(&TokenKind::Comma);
                // Second arg is a field name (possibly nested like `a.b`)
                let field = self.assignment_expr();
                self.expect(&TokenKind::RParen);
                Expr::Builtin("__builtin_offsetof".into(), vec![Expr::Ident(type_name), field])
            }
            TokenKind::Builtin(name) => {
                let name = name.clone();
                self.advance();
                if self.peek() == &TokenKind::LParen {
                    self.advance();
                    let mut args = Vec::new();
                    while self.peek() != &TokenKind::RParen {
                        args.push(self.assignment_expr());
                        if !self.eat(&TokenKind::Comma) { break; }
                    }
                    self.expect(&TokenKind::RParen);
                    Expr::Builtin(name, args)
                } else {
                    Expr::Ident(name)
                }
            }
            other => {
                let loc = self.tokens.get(self.pos).map(|t| &t.loc);
                let context: Vec<_> = self.tokens[self.pos.saturating_sub(5)..std::cmp::min(self.pos+5, self.tokens.len())]
                    .iter().map(|t| format!("{:?}@{:?}", t.kind, t.loc)).collect();
                panic!("unexpected token in expression: {:?} at {:?}\ncontext: {}", other, loc, context.join(", "));
            }
        }
    }
}

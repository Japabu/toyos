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

    fn eval_const_expr(expr: &Expr) -> Option<i64> {
        match expr {
            Expr::IntLit(v) => Some(*v as i64),
            Expr::Unary(UnaryOp::Neg, e) => Self::eval_const_expr(e).map(|v| -v),
            Expr::Unary(UnaryOp::BitNot, e) => Self::eval_const_expr(e).map(|v| !v),
            Expr::Binary(op, l, r) => {
                let l = Self::eval_const_expr(l)?;
                let r = Self::eval_const_expr(r)?;
                Some(match op {
                    BinOp::Add => l + r,
                    BinOp::Sub => l - r,
                    BinOp::Mul => l * r,
                    BinOp::Div => if r != 0 { l / r } else { 0 },
                    BinOp::Mod => if r != 0 { l % r } else { 0 },
                    BinOp::Shl => l << r,
                    BinOp::Shr => l >> r,
                    BinOp::BitAnd => l & r,
                    BinOp::BitOr => l | r,
                    BinOp::BitXor => l ^ r,
                    BinOp::Eq => (l == r) as i64,
                    BinOp::Ne => (l != r) as i64,
                    BinOp::Lt => (l < r) as i64,
                    BinOp::Gt => (l > r) as i64,
                    BinOp::Le => (l <= r) as i64,
                    BinOp::Ge => (l >= r) as i64,
                    BinOp::LogAnd => ((l != 0) && (r != 0)) as i64,
                    BinOp::LogOr => ((l != 0) || (r != 0)) as i64,
                })
            }
            Expr::Conditional(cond, then, els) => {
                let c = Self::eval_const_expr(cond)?;
                if c != 0 { Self::eval_const_expr(then) } else { Self::eval_const_expr(els) }
            }
            Expr::Cast(_, e) => Self::eval_const_expr(e),
            Expr::Ident(_) => None, // can't resolve at parse time
            _ => None,
        }
    }

    // Token access
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

    // Entry point
    pub fn parse(mut self) -> (TranslationUnit, TypeEnv) {
        let mut tu = Vec::new();
        while !self.at_eof() {
            // Skip stray semicolons
            if self.eat(&TokenKind::Semi) { continue; }
            // __extension__ prefix
            if matches!(self.peek(), TokenKind::Extension) { self.advance(); }
            tu.push(self.external_decl());
        }
        let env = self.type_env;
        (tu, env)
    }

    fn external_decl(&mut self) -> ExternalDecl {
        let specifiers = self.decl_specifiers();

        // Check for function definition: specifiers declarator { ... }
        if self.peek() == &TokenKind::Semi {
            // Just a declaration with no declarators (e.g., struct definition)
            self.advance();
            return ExternalDecl::Declaration(Declaration { specifiers, declarators: Vec::new() });
        }

        let declarator = self.declarator();

        // Is this a function definition?
        if self.peek() == &TokenKind::LBrace && self.is_function_declarator(&declarator) {
            let is_typedef = specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Typedef)));
            if !is_typedef {
                let body = self.compound_stmt();
                return ExternalDecl::Function(FunctionDef { specifiers, declarator, body });
            }
        }

        // Otherwise it's a declaration
        let mut declarators = vec![InitDeclarator {
            declarator,
            initializer: if self.eat(&TokenKind::Eq) { Some(self.initializer()) } else { None },
        }];

        while self.eat(&TokenKind::Comma) {
            let d = self.declarator();
            let init = if self.eat(&TokenKind::Eq) { Some(self.initializer()) } else { None };
            declarators.push(InitDeclarator { declarator: d, initializer: init });
        }
        self.expect(&TokenKind::Semi);

        // Register typedefs
        let is_typedef = specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Typedef)));
        if is_typedef {
            for id in &declarators {
                if let Some(name) = self.declarator_name(&id.declarator) {
                    self.type_env.typedefs.insert(name, crate::types::CType::Int(true)); // placeholder
                }
            }
        }

        ExternalDecl::Declaration(Declaration { specifiers, declarators })
    }

    fn is_function_declarator(&self, d: &Declarator) -> bool {
        match &d.direct {
            DirectDeclarator::Function(..) => true,
            DirectDeclarator::Paren(inner) => self.is_function_declarator(inner),
            _ => false,
        }
    }

    fn declarator_name(&self, d: &Declarator) -> Option<String> {
        self.direct_declarator_name(&d.direct)
    }

    fn direct_declarator_name(&self, d: &DirectDeclarator) -> Option<String> {
        match d {
            DirectDeclarator::Ident(s) => Some(s.clone()),
            DirectDeclarator::Paren(inner) => self.declarator_name(inner),
            DirectDeclarator::Array(inner, _) | DirectDeclarator::Function(inner, _) => {
                self.direct_declarator_name(inner)
            }
        }
    }

    // Declaration specifiers
    fn decl_specifiers(&mut self) -> Vec<DeclSpecifier> {
        let mut specs = Vec::new();
        loop {
            match self.peek() {
                // Storage class
                TokenKind::Auto => { self.advance(); specs.push(DeclSpecifier::StorageClass(StorageClass::Auto)); }
                TokenKind::Register => { self.advance(); specs.push(DeclSpecifier::StorageClass(StorageClass::Register)); }
                TokenKind::Static => { self.advance(); specs.push(DeclSpecifier::StorageClass(StorageClass::Static)); }
                TokenKind::Extern => { self.advance(); specs.push(DeclSpecifier::StorageClass(StorageClass::Extern)); }
                TokenKind::Typedef => { self.advance(); specs.push(DeclSpecifier::StorageClass(StorageClass::Typedef)); }

                // Type qualifiers
                TokenKind::Const => { self.advance(); specs.push(DeclSpecifier::TypeQual(TypeQual::Const)); }
                TokenKind::Volatile => { self.advance(); specs.push(DeclSpecifier::TypeQual(TypeQual::Volatile)); }
                TokenKind::Restrict => { self.advance(); specs.push(DeclSpecifier::TypeQual(TypeQual::Restrict)); }

                // Function specifiers
                TokenKind::Inline => { self.advance(); specs.push(DeclSpecifier::FuncSpec(FuncSpec::Inline)); }

                // Type specifiers
                TokenKind::Void => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Void)); }
                TokenKind::Char => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Char)); }
                TokenKind::Short => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Short)); }
                TokenKind::Int => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Int)); }
                TokenKind::Long => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Long)); }
                TokenKind::Float => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Float)); }
                TokenKind::Double => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Double)); }
                TokenKind::Signed => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Signed)); }
                TokenKind::Unsigned => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Unsigned)); }
                TokenKind::Bool => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Bool)); }
                TokenKind::Int128 => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Int128)); }

                // Struct/union/enum
                TokenKind::Struct => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Struct(self.struct_or_union_type()))); }
                TokenKind::Union => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Union(self.struct_or_union_type()))); }
                TokenKind::Enum => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Enum(self.enum_type()))); }

                // Typeof
                TokenKind::Typeof => {
                    self.advance();
                    self.expect(&TokenKind::LParen);
                    if self.is_type_start() {
                        let tn = self.type_name();
                        self.expect(&TokenKind::RParen);
                        specs.push(DeclSpecifier::TypeSpec(TypeSpec::TypeofType(Box::new(tn))));
                    } else {
                        let e = self.expr();
                        self.expect(&TokenKind::RParen);
                        specs.push(DeclSpecifier::TypeSpec(TypeSpec::Typeof(Box::new(e))));
                    }
                }

                // __extension__
                TokenKind::Extension => { self.advance(); }

                // __attribute__
                TokenKind::Attribute => {
                    let attrs = self.parse_attributes();
                    specs.push(DeclSpecifier::Attribute(attrs));
                }

                // Typedef name
                TokenKind::Ident(name) if self.type_env.is_typedef(name) => {
                    let name = name.clone();
                    self.advance();
                    specs.push(DeclSpecifier::TypeSpec(TypeSpec::TypedefName(name)));
                }

                // __builtin_ types
                TokenKind::Builtin => {
                    // Consume and treat as identifier
                    let tok = self.tokens[self.pos].clone();
                    self.advance();
                    if let TokenKind::Ident(name) = &tok.kind {
                        specs.push(DeclSpecifier::TypeSpec(TypeSpec::Builtin(name.clone())));
                    }
                }

                _ => break,
            }
        }
        specs
    }

    fn struct_or_union_type(&mut self) -> StructType {
        let mut attrs = Vec::new();

        // Optional __attribute__ before name
        if matches!(self.peek(), TokenKind::Attribute) {
            attrs = self.parse_attributes();
        }

        let name = if let TokenKind::Ident(s) = self.peek() {
            let s = s.clone();
            self.advance();
            Some(s)
        } else {
            None
        };

        // Optional __attribute__ after name
        if matches!(self.peek(), TokenKind::Attribute) {
            attrs.extend(self.parse_attributes());
        }

        let fields = if self.peek() == &TokenKind::LBrace {
            self.advance();
            let mut fields = Vec::new();
            while self.peek() != &TokenKind::RBrace {
                if matches!(self.peek(), TokenKind::Extension) { self.advance(); }
                let spec = self.decl_specifiers();
                let mut declarators = Vec::new();

                if self.peek() == &TokenKind::Semi {
                    // Anonymous field (anonymous struct/union)
                    declarators.push(StructFieldDeclarator { declarator: None, bit_width: None });
                } else {
                    loop {
                        if self.peek() == &TokenKind::Colon {
                            self.advance();
                            let width = self.conditional_expr();
                            declarators.push(StructFieldDeclarator { declarator: None, bit_width: Some(Box::new(width)) });
                        } else {
                            let d = self.declarator();
                            let bw = if self.eat(&TokenKind::Colon) {
                                Some(Box::new(self.conditional_expr()))
                            } else {
                                None
                            };
                            declarators.push(StructFieldDeclarator { declarator: Some(d), bit_width: bw });
                        }
                        // Skip __attribute__
                        if matches!(self.peek(), TokenKind::Attribute) { self.parse_attributes(); }
                        if !self.eat(&TokenKind::Comma) { break; }
                    }
                }
                self.expect(&TokenKind::Semi);
                fields.push(StructField { specifiers: spec, declarators });
            }
            self.expect(&TokenKind::RBrace);
            Some(fields)
        } else {
            None
        };

        // Optional trailing __attribute__
        if matches!(self.peek(), TokenKind::Attribute) {
            attrs.extend(self.parse_attributes());
        }

        StructType { name, fields, attributes: attrs }
    }

    fn enum_type(&mut self) -> EnumType {
        let mut attrs = Vec::new();
        if matches!(self.peek(), TokenKind::Attribute) {
            attrs = self.parse_attributes();
        }

        let name = if let TokenKind::Ident(s) = self.peek() {
            let s = s.clone();
            self.advance();
            Some(s)
        } else {
            None
        };

        let variants = if self.peek() == &TokenKind::LBrace {
            self.advance();
            let mut variants = Vec::new();
            let mut next_val: i64 = 0;
            while self.peek() != &TokenKind::RBrace {
                let vname = self.ident();
                let value = if self.eat(&TokenKind::Eq) {
                    let expr = self.conditional_expr();
                    if let Some(v) = Self::eval_const_expr(&expr) {
                        next_val = v;
                    }
                    Some(expr)
                } else {
                    None
                };
                variants.push(Enumerator { name: vname.clone(), value });
                self.type_env.enum_constants.insert(vname, next_val);
                next_val += 1;
                if !self.eat(&TokenKind::Comma) { break; }
            }
            self.expect(&TokenKind::RBrace);
            Some(variants)
        } else {
            None
        };

        EnumType { name, variants, attributes: attrs }
    }

    fn parse_attributes(&mut self) -> Vec<Attribute> {
        let mut attrs = Vec::new();
        while matches!(self.peek(), TokenKind::Attribute) {
            self.advance();
            self.expect(&TokenKind::LParen);
            self.expect(&TokenKind::LParen);
            while self.peek() != &TokenKind::RParen {
                if let TokenKind::Ident(name) = self.peek().clone() {
                    self.advance();
                    let args = if self.peek() == &TokenKind::LParen {
                        self.advance();
                        let mut a = Vec::new();
                        while self.peek() != &TokenKind::RParen {
                            a.push(self.conditional_expr());
                            if !self.eat(&TokenKind::Comma) { break; }
                        }
                        self.expect(&TokenKind::RParen);
                        a
                    } else {
                        Vec::new()
                    };
                    attrs.push(Attribute { name, args });
                } else {
                    self.advance(); // skip unknown
                }
                let _ = self.eat(&TokenKind::Comma);
            }
            self.expect(&TokenKind::RParen);
            self.expect(&TokenKind::RParen);
        }
        attrs
    }

    // Declarator
    fn declarator(&mut self) -> Declarator {
        let mut pointer = Vec::new();
        while self.peek() == &TokenKind::Star {
            self.advance();
            let mut quals = Vec::new();
            loop {
                match self.peek() {
                    TokenKind::Const => { self.advance(); quals.push(TypeQual::Const); }
                    TokenKind::Volatile => { self.advance(); quals.push(TypeQual::Volatile); }
                    TokenKind::Restrict => { self.advance(); quals.push(TypeQual::Restrict); }
                    _ => break,
                }
            }
            pointer.push(Pointer { qualifiers: quals });
        }

        // Skip __attribute__ on declarators
        if matches!(self.peek(), TokenKind::Attribute) { self.parse_attributes(); }

        let direct = self.direct_declarator();
        Declarator { pointer, direct }
    }

    fn direct_declarator(&mut self) -> DirectDeclarator {
        let mut dd = match self.peek() {
            TokenKind::LParen => {
                // Could be grouped declarator or function
                // Heuristic: if next token after ( is a type specifier, it's a function parameter list on empty declarator
                // Otherwise it's a grouped declarator
                if self.is_declarator_start_after_paren() {
                    self.advance();
                    let inner = self.declarator();
                    self.expect(&TokenKind::RParen);
                    DirectDeclarator::Paren(Box::new(inner))
                } else {
                    // This shouldn't happen at top level - fallback
                    DirectDeclarator::Ident(String::new())
                }
            }
            TokenKind::Ident(s) => {
                let s = s.clone();
                self.advance();
                DirectDeclarator::Ident(s)
            }
            _ => DirectDeclarator::Ident(String::new()), // abstract declarator
        };

        // Postfix parts: array [] and function ()
        loop {
            match self.peek() {
                TokenKind::LBracket => {
                    self.advance();
                    let size = if self.peek() == &TokenKind::RBracket {
                        None
                    } else {
                        Some(Box::new(self.conditional_expr()))
                    };
                    self.expect(&TokenKind::RBracket);
                    dd = DirectDeclarator::Array(Box::new(dd), size);
                }
                TokenKind::LParen => {
                    self.advance();
                    let params = self.param_list();
                    self.expect(&TokenKind::RParen);
                    dd = DirectDeclarator::Function(Box::new(dd), params);
                }
                _ => break,
            }
        }

        // Skip trailing __attribute__
        if matches!(self.peek(), TokenKind::Attribute) { self.parse_attributes(); }

        dd
    }

    fn is_declarator_start_after_paren(&self) -> bool {
        // Look at token after (
        // If it's *, another (, or an identifier that's NOT a type, it's a grouped declarator
        match self.peek2() {
            TokenKind::Star | TokenKind::LParen => true,
            TokenKind::Ident(name) => !self.type_env.is_typedef(name) && !self.is_type_keyword(self.peek2()),
            // If it's a type keyword, it's a parameter list
            _ => false,
        }
    }

    fn is_type_keyword(&self, t: &TokenKind) -> bool {
        matches!(t, TokenKind::Void | TokenKind::Char | TokenKind::Short | TokenKind::Int
            | TokenKind::Long | TokenKind::Float | TokenKind::Double | TokenKind::Signed
            | TokenKind::Unsigned | TokenKind::Struct | TokenKind::Union | TokenKind::Enum
            | TokenKind::Const | TokenKind::Volatile | TokenKind::Restrict
            | TokenKind::Typedef | TokenKind::Static | TokenKind::Extern | TokenKind::Register
            | TokenKind::Auto | TokenKind::Inline | TokenKind::Bool | TokenKind::Typeof
            | TokenKind::Int128)
    }

    fn is_type_start(&self) -> bool {
        match self.peek() {
            t if self.is_type_keyword(t) => true,
            TokenKind::Ident(name) => self.type_env.is_typedef(name),
            TokenKind::Attribute | TokenKind::Extension => true,
            _ => false,
        }
    }

    fn param_list(&mut self) -> ParamList {
        let mut params = Vec::new();
        let mut variadic = false;

        if self.peek() == &TokenKind::RParen {
            return ParamList { params, variadic };
        }

        // Special case: (void)
        if self.peek() == &TokenKind::Void && self.peek2() == &TokenKind::RParen {
            self.advance();
            return ParamList { params, variadic };
        }

        // Old-style identifier list?
        if matches!(self.peek(), TokenKind::Ident(s) if !self.type_env.is_typedef(s)) {
            if matches!(self.peek2(), TokenKind::Comma | TokenKind::RParen) {
                // Could be old-style K&R parameter names
                // Parse as identifiers
                loop {
                    if let TokenKind::Ident(_) = self.peek() {
                        let name = self.ident();
                        params.push(ParamDecl {
                            specifiers: vec![DeclSpecifier::TypeSpec(TypeSpec::Int)],
                            declarator: Some(Declarator {
                                pointer: Vec::new(),
                                direct: DirectDeclarator::Ident(name),
                            }),
                        });
                    }
                    if !self.eat(&TokenKind::Comma) { break; }
                    if self.peek() == &TokenKind::Ellipsis { variadic = true; self.advance(); break; }
                }
                return ParamList { params, variadic };
            }
        }

        loop {
            if self.peek() == &TokenKind::Ellipsis {
                self.advance();
                variadic = true;
                break;
            }

            let specifiers = self.decl_specifiers();
            let declarator = if self.peek() == &TokenKind::RParen || self.peek() == &TokenKind::Comma {
                None
            } else {
                Some(self.declarator_or_abstract())
            };
            params.push(ParamDecl { specifiers, declarator });

            if !self.eat(&TokenKind::Comma) { break; }
        }

        ParamList { params, variadic }
    }

    fn declarator_or_abstract(&mut self) -> Declarator {
        // This is tricky - both declarators and abstract declarators can start with *
        // Try to parse as declarator
        self.declarator()
    }

    // Type name (for casts, sizeof)
    fn type_name(&mut self) -> TypeName {
        let specifiers = self.decl_specifiers();
        let declarator = if self.peek() == &TokenKind::RParen || self.peek() == &TokenKind::Comma {
            None
        } else {
            Some(self.abstract_declarator())
        };
        TypeName { specifiers, declarator }
    }

    fn abstract_declarator(&mut self) -> AbstractDeclarator {
        let mut pointer = Vec::new();
        while self.peek() == &TokenKind::Star {
            self.advance();
            let mut quals = Vec::new();
            loop {
                match self.peek() {
                    TokenKind::Const => { self.advance(); quals.push(TypeQual::Const); }
                    TokenKind::Volatile => { self.advance(); quals.push(TypeQual::Volatile); }
                    TokenKind::Restrict => { self.advance(); quals.push(TypeQual::Restrict); }
                    _ => break,
                }
            }
            pointer.push(Pointer { qualifiers: quals });
        }

        let direct = self.direct_abstract_declarator();
        AbstractDeclarator { pointer, direct }
    }

    fn direct_abstract_declarator(&mut self) -> Option<DirectAbstractDeclarator> {
        let mut dad = match self.peek() {
            TokenKind::LParen if self.is_abstract_paren() => {
                self.advance();
                let inner = self.abstract_declarator();
                self.expect(&TokenKind::RParen);
                Some(DirectAbstractDeclarator::Paren(Box::new(inner)))
            }
            _ => None,
        };

        loop {
            match self.peek() {
                TokenKind::LBracket => {
                    self.advance();
                    let size = if self.peek() == &TokenKind::RBracket {
                        None
                    } else {
                        Some(Box::new(self.conditional_expr()))
                    };
                    self.expect(&TokenKind::RBracket);
                    dad = Some(DirectAbstractDeclarator::Array(dad.map(Box::new), size));
                }
                TokenKind::LParen if dad.is_some() || self.peek2() == &TokenKind::RParen || self.is_param_list_start() => {
                    self.advance();
                    let params = self.param_list();
                    self.expect(&TokenKind::RParen);
                    dad = Some(DirectAbstractDeclarator::Function(dad.map(Box::new), params));
                }
                _ => break,
            }
        }

        dad
    }

    fn is_abstract_paren(&self) -> bool {
        // ( followed by * or ( or [ is abstract declarator
        // ( followed by type keywords is parameter list
        matches!(self.peek2(), TokenKind::Star | TokenKind::LBracket)
    }

    fn is_param_list_start(&self) -> bool {
        self.is_type_keyword(self.peek2()) || matches!(self.peek2(), TokenKind::Ident(name) if self.type_env.is_typedef(name))
    }

    // Initializer
    fn initializer(&mut self) -> Initializer {
        if self.peek() == &TokenKind::LBrace {
            self.advance();
            let mut items = Vec::new();
            while self.peek() != &TokenKind::RBrace {
                let mut designators = Vec::new();
                // Designated initializer
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
                items.push(InitializerItem { designators, initializer: init });
                if !self.eat(&TokenKind::Comma) { break; }
            }
            self.expect(&TokenKind::RBrace);
            Initializer::List(items)
        } else {
            Initializer::Expr(self.assignment_expr())
        }
    }

    // Statements
    fn stmt(&mut self) -> Statement {
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
                self.expect(&TokenKind::Colon);
                let body = self.stmt();
                Statement::Case(Box::new(val), Box::new(body))
            }
            TokenKind::Default => {
                self.advance();
                self.expect(&TokenKind::Colon);
                let body = self.stmt();
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
                let label = self.ident();
                self.expect(&TokenKind::Semi);
                Statement::Goto(label)
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

    fn compound_stmt(&mut self) -> Statement {
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

    fn is_declaration_start(&self) -> bool {
        match self.peek() {
            t if self.is_type_keyword(t) => true,
            TokenKind::Ident(name) => {
                if self.type_env.is_typedef(name) {
                    // But could be label: ident followed by :
                    !matches!(self.peek2(), TokenKind::Colon)
                } else {
                    false
                }
            }
            TokenKind::Attribute | TokenKind::Extension => true,
            _ => false,
        }
    }

    fn local_declaration(&mut self) -> Declaration {
        let specifiers = self.decl_specifiers();
        let mut declarators = Vec::new();

        if self.peek() != &TokenKind::Semi {
            loop {
                let d = self.declarator();
                let init = if self.eat(&TokenKind::Eq) { Some(self.initializer()) } else { None };
                declarators.push(InitDeclarator { declarator: d, initializer: init });
                // Skip __attribute__
                if matches!(self.peek(), TokenKind::Attribute) { self.parse_attributes(); }
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

    // Expressions — operator precedence climbing
    pub fn expr(&mut self) -> Expr {
        let e = self.assignment_expr();
        if self.peek() == &TokenKind::Comma {
            self.advance();
            let right = self.expr();
            Expr::Comma(Box::new(e), Box::new(right))
        } else {
            e
        }
    }

    fn assignment_expr(&mut self) -> Expr {
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

    fn conditional_expr(&mut self) -> Expr {
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
        let saved = self.pos;
        // Look at what follows the (
        let mut depth = 0;
        let mut i = self.pos + 1; // skip the (
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
            TokenKind::FloatLit(v) => { self.advance(); Expr::FloatLit(v) }
            TokenKind::CharLit(v) => { self.advance(); Expr::CharLit(v) }
            TokenKind::StringLit(s) => { self.advance(); Expr::StringLit(s) }
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
            TokenKind::Builtin => {
                // Generic builtin handling - consume name and try to parse as function call
                let name = if let TokenKind::Ident(s) = &self.tokens[self.pos].kind {
                    s.clone()
                } else {
                    "__builtin_unknown".to_string()
                };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::Lexer;
    use crate::preprocess::Preprocessor;

    fn parse_str(src: &str) -> TranslationUnit {
        let mut pp = Preprocessor::new(vec![], vec![]);
        let preprocessed = pp.preprocess(src, "<test>");
        let tokens = Lexer::new(&preprocessed, "<test>").tokenize();
        let parser = Parser::new(tokens);
        let (tu, _) = parser.parse();
        tu
    }

    fn parse_func(src: &str) -> FunctionDef {
        let tu = parse_str(src);
        for decl in tu.iter().rev() {
            if let ExternalDecl::Function(f) = decl {
                return f.clone();
            }
        }
        panic!("expected function");
    }

    fn parse_decl(src: &str) -> Declaration {
        let tu = parse_str(src);
        match &tu[0] {
            ExternalDecl::Declaration(d) => d.clone(),
            _ => panic!("expected declaration"),
        }
    }

    // === Basic function parsing ===

    #[test]
    fn empty_function() {
        let f = parse_func("void f(void) {}");
        assert!(matches!(f.body, Statement::Compound(ref items) if items.is_empty()));
    }

    #[test]
    fn return_constant() {
        let f = parse_func("int main(void) { return 42; }");
        if let Statement::Compound(items) = &f.body {
            assert_eq!(items.len(), 1);
            if let BlockItem::Stmt(Statement::Return(Some(Expr::IntLit(42)))) = &items[0] {
                // OK
            } else {
                panic!("expected return 42, got {:?}", items[0]);
            }
        } else {
            panic!("expected compound statement");
        }
    }

    #[test]
    fn function_with_params() {
        let f = parse_func("int add(int a, int b) { return a + b; }");
        if let DirectDeclarator::Function(_, ref params) = f.declarator.direct {
            assert_eq!(params.params.len(), 2);
        } else {
            panic!("expected function declarator");
        }
    }

    // === Declarations ===

    #[test]
    fn simple_decl() {
        let d = parse_decl("int x;");
        assert_eq!(d.declarators.len(), 1);
    }

    #[test]
    fn decl_with_init() {
        let d = parse_decl("int x = 42;");
        assert!(d.declarators[0].initializer.is_some());
    }

    #[test]
    fn pointer_decl() {
        let d = parse_decl("int *p;");
        assert!(!d.declarators[0].declarator.pointer.is_empty());
    }

    #[test]
    fn array_decl() {
        let d = parse_decl("int arr[10];");
        assert!(matches!(d.declarators[0].declarator.direct, DirectDeclarator::Array(_, Some(_))));
    }

    #[test]
    fn typedef_decl() {
        let tu = parse_str("typedef int myint; myint x;");
        assert_eq!(tu.len(), 2);
    }

    #[test]
    fn struct_decl() {
        let d = parse_decl("struct point { int x; int y; } p;");
        let has_struct = d.specifiers.iter().any(|s| matches!(s, DeclSpecifier::TypeSpec(TypeSpec::Struct(_))));
        assert!(has_struct);
    }

    #[test]
    fn enum_decl() {
        let d = parse_decl("enum color { RED, GREEN, BLUE } c;");
        let has_enum = d.specifiers.iter().any(|s| matches!(s, DeclSpecifier::TypeSpec(TypeSpec::Enum(_))));
        assert!(has_enum);
    }

    #[test]
    fn union_decl() {
        let d = parse_decl("union data { int i; float f; } d;");
        let has_union = d.specifiers.iter().any(|s| matches!(s, DeclSpecifier::TypeSpec(TypeSpec::Union(_))));
        assert!(has_union);
    }

    #[test]
    fn function_pointer_decl() {
        let _d = parse_decl("int (*fp)(int, int);");
    }

    #[test]
    fn multiple_declarators() {
        let d = parse_decl("int a, b, c;");
        assert_eq!(d.declarators.len(), 3);
    }

    // === Statements ===

    #[test]
    fn if_stmt() {
        let f = parse_func("void f(void) { if (1) return; }");
        if let Statement::Compound(items) = &f.body {
            assert!(matches!(&items[0], BlockItem::Stmt(Statement::If(..))));
        }
    }

    #[test]
    fn if_else_stmt() {
        let f = parse_func("void f(void) { if (1) return; else return; }");
        if let Statement::Compound(items) = &f.body {
            if let BlockItem::Stmt(Statement::If(_, _, ref else_)) = items[0] {
                assert!(else_.is_some());
            }
        }
    }

    #[test]
    fn while_stmt() {
        let f = parse_func("void f(void) { while (1) {} }");
        if let Statement::Compound(items) = &f.body {
            assert!(matches!(&items[0], BlockItem::Stmt(Statement::While(..))));
        }
    }

    #[test]
    fn for_stmt() {
        let f = parse_func("void f(void) { for (int i = 0; i < 10; i++) {} }");
        if let Statement::Compound(items) = &f.body {
            assert!(matches!(&items[0], BlockItem::Stmt(Statement::For(..))));
        }
    }

    #[test]
    fn do_while_stmt() {
        let f = parse_func("void f(void) { do {} while (1); }");
        if let Statement::Compound(items) = &f.body {
            assert!(matches!(&items[0], BlockItem::Stmt(Statement::DoWhile(..))));
        }
    }

    #[test]
    fn switch_stmt() {
        let f = parse_func("void f(int x) { switch (x) { case 1: break; default: break; } }");
        if let Statement::Compound(items) = &f.body {
            assert!(matches!(&items[0], BlockItem::Stmt(Statement::Switch(..))));
        }
    }

    #[test]
    fn goto_and_label() {
        let f = parse_func("void f(void) { goto end; end: return; }");
        if let Statement::Compound(items) = &f.body {
            assert!(matches!(&items[0], BlockItem::Stmt(Statement::Goto(_))));
            assert!(matches!(&items[1], BlockItem::Stmt(Statement::Label(..))));
        }
    }

    #[test]
    fn break_continue() {
        let f = parse_func("void f(void) { while(1) { break; continue; } }");
        if let Statement::Compound(items) = &f.body {
            if let BlockItem::Stmt(Statement::While(_, body)) = &items[0] {
                if let Statement::Compound(body_items) = body.as_ref() {
                    assert!(matches!(&body_items[0], BlockItem::Stmt(Statement::Break)));
                    assert!(matches!(&body_items[1], BlockItem::Stmt(Statement::Continue)));
                }
            }
        }
    }

    // === Expressions ===

    #[test]
    fn binary_expr() {
        let f = parse_func("int f(void) { return 1 + 2; }");
        if let Statement::Compound(items) = &f.body {
            if let BlockItem::Stmt(Statement::Return(Some(Expr::Binary(BinOp::Add, _, _)))) = &items[0] {
                // OK
            } else {
                panic!("expected binary add");
            }
        }
    }

    #[test]
    fn operator_precedence() {
        // 1 + 2 * 3 should parse as 1 + (2 * 3)
        let f = parse_func("int f(void) { return 1 + 2 * 3; }");
        if let Statement::Compound(items) = &f.body {
            if let BlockItem::Stmt(Statement::Return(Some(Expr::Binary(BinOp::Add, left, right)))) = &items[0] {
                assert!(matches!(left.as_ref(), Expr::IntLit(1)));
                assert!(matches!(right.as_ref(), Expr::Binary(BinOp::Mul, _, _)));
            } else {
                panic!("expected add with mul on right");
            }
        }
    }

    #[test]
    fn conditional_expr() {
        let f = parse_func("int f(int x) { return x ? 1 : 0; }");
        if let Statement::Compound(items) = &f.body {
            if let BlockItem::Stmt(Statement::Return(Some(Expr::Conditional(..)))) = &items[0] {
                // OK
            } else {
                panic!("expected conditional");
            }
        }
    }

    #[test]
    fn function_call() {
        let f = parse_func("int f(void) { return foo(1, 2); }");
        if let Statement::Compound(items) = &f.body {
            if let BlockItem::Stmt(Statement::Return(Some(Expr::Call(_, args)))) = &items[0] {
                assert_eq!(args.len(), 2);
            } else {
                panic!("expected call");
            }
        }
    }

    #[test]
    fn sizeof_expr() {
        let f = parse_func("int f(void) { return sizeof(int); }");
        if let Statement::Compound(items) = &f.body {
            if let BlockItem::Stmt(Statement::Return(Some(Expr::Sizeof(_)))) = &items[0] {
                // OK
            } else {
                panic!("expected sizeof");
            }
        }
    }

    #[test]
    fn cast_expr() {
        let f = parse_func("int f(void) { return (int)3.14; }");
        if let Statement::Compound(items) = &f.body {
            if let BlockItem::Stmt(Statement::Return(Some(Expr::Cast(..)))) = &items[0] {
                // OK
            } else {
                panic!("expected cast");
            }
        }
    }

    #[test]
    fn assignment() {
        let f = parse_func("void f(void) { int x; x = 42; }");
        if let Statement::Compound(items) = &f.body {
            if let BlockItem::Stmt(Statement::Expr(Some(Expr::Assign(..)))) = &items[1] {
                // OK
            } else {
                panic!("expected assignment, got {:?}", items[1]);
            }
        }
    }

    #[test]
    fn struct_member_access() {
        let f = parse_func("struct S { int x; }; int f(void) { struct S s; return s.x; }");
        if let Statement::Compound(items) = &f.body {
            // Last statement should be return s.x
            let last = items.last().unwrap();
            if let BlockItem::Stmt(Statement::Return(Some(Expr::Member(..)))) = last {
                // OK
            } else {
                panic!("expected member access, got {:?}", last);
            }
        }
    }

    #[test]
    fn arrow_member_access() {
        let f = parse_func("struct S { int x; }; int f(struct S *p) { return p->x; }");
        if let Statement::Compound(items) = &f.body {
            let last = items.last().unwrap();
            if let BlockItem::Stmt(Statement::Return(Some(Expr::Arrow(..)))) = last {
                // OK
            } else {
                panic!("expected arrow access, got {:?}", last);
            }
        }
    }

    #[test]
    fn array_index() {
        let f = parse_func("int f(void) { int arr[10]; return arr[0]; }");
        if let Statement::Compound(items) = &f.body {
            let last = items.last().unwrap();
            if let BlockItem::Stmt(Statement::Return(Some(Expr::Index(..)))) = last {
                // OK
            } else {
                panic!("expected index, got {:?}", last);
            }
        }
    }

    #[test]
    fn unary_ops() {
        let f = parse_func("int f(int x) { return -x; }");
        if let Statement::Compound(items) = &f.body {
            if let BlockItem::Stmt(Statement::Return(Some(Expr::Unary(UnaryOp::Neg, _)))) = &items[0] {
                // OK
            } else {
                panic!("expected unary neg");
            }
        }
    }

    #[test]
    fn address_of_and_deref() {
        let f = parse_func("void f(void) { int x; int *p = &x; *p = 42; }");
        if let Statement::Compound(items) = &f.body {
            assert!(items.len() >= 2);
        }
    }

    #[test]
    fn complex_declaration() {
        // Array of pointers to functions
        parse_str("int (*fps[10])(int);");
    }

    #[test]
    fn static_function() {
        let f = parse_func("static int helper(void) { return 0; }");
        let is_static = f.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Static)));
        assert!(is_static);
    }

    #[test]
    fn extern_decl() {
        let d = parse_decl("extern int printf(const char *fmt, ...);");
        let is_extern = d.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Extern)));
        assert!(is_extern);
    }

    #[test]
    fn variadic_function() {
        let _tu = parse_str("int printf(const char *fmt, ...);");
    }

    #[test]
    fn nested_struct() {
        parse_str("struct outer { struct inner { int x; } i; int y; };");
    }

    #[test]
    fn local_vars_in_for() {
        parse_str("void f(void) { for (int i = 0; i < 10; i++) { } }");
    }

    #[test]
    fn compound_literal() {
        parse_str("void f(void) { int *p = (int[]){1, 2, 3}; }");
    }

    #[test]
    fn multiple_functions() {
        let tu = parse_str("int a(void) { return 1; } int b(void) { return 2; }");
        assert_eq!(tu.len(), 2);
    }

    #[test]
    fn empty_struct() {
        parse_str("struct empty {};");
    }

    #[test]
    fn enum_with_values() {
        parse_str("enum flags { A = 1, B = 2, C = 4 };");
    }

    #[test]
    fn typedef_struct() {
        let tu = parse_str("typedef struct { int x; int y; } Point; Point p;");
        assert_eq!(tu.len(), 2);
    }

    #[test]
    fn string_literal_expr() {
        let f = parse_func("void f(void) { const char *s = \"hello\"; }");
        if let Statement::Compound(items) = &f.body {
            assert!(!items.is_empty());
        }
    }

    #[test]
    fn comma_expr() {
        let f = parse_func("int f(void) { return (1, 2, 3); }");
        if let Statement::Compound(items) = &f.body {
            if let BlockItem::Stmt(Statement::Return(Some(Expr::Comma(..)))) = &items[0] {
                // OK
            } else {
                panic!("expected comma expr");
            }
        }
    }

    #[test]
    fn increment_decrement() {
        let f = parse_func("void f(void) { int x; x++; ++x; x--; --x; }");
        if let Statement::Compound(items) = &f.body {
            assert!(items.len() >= 5);
        }
    }
}

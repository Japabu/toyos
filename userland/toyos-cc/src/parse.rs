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

    pub fn eval_const_expr_static(expr: &Expr) -> Option<i64> {
        Self::eval_const_expr(expr)
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
        let init = if self.eat(&TokenKind::Eq) { Some(self.initializer()) } else { None };
        // Skip trailing __attribute__ and __asm("label") after each declarator
        if matches!(self.peek(), TokenKind::Attribute) { self.parse_attributes(); }
        self.skip_asm_label();
        let mut declarators = vec![InitDeclarator { declarator, initializer: init }];

        while self.eat(&TokenKind::Comma) {
            let d = self.declarator();
            let init = if self.eat(&TokenKind::Eq) { Some(self.initializer()) } else { None };
            if matches!(self.peek(), TokenKind::Attribute) { self.parse_attributes(); }
            self.skip_asm_label();
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

                // __builtin_ types (e.g. __builtin_va_list)
                TokenKind::Builtin(name) => {
                    let name = name.clone();
                    self.advance();
                    specs.push(DeclSpecifier::TypeSpec(TypeSpec::Builtin(name)));
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
                    if self.peek() == &TokenKind::LParen {
                        self.skip_balanced_parens();
                    }
                    attrs.push(Attribute { name, args: Vec::new() });
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

    /// Skip a balanced `(...)` group including nested parens.
    fn skip_balanced_parens(&mut self) {
        assert_eq!(self.peek(), &TokenKind::LParen);
        self.advance();
        let mut depth = 1u32;
        while depth > 0 {
            match self.peek() {
                TokenKind::LParen => { depth += 1; self.advance(); }
                TokenKind::RParen => { depth -= 1; self.advance(); }
                _ => { self.advance(); }
            }
        }
    }

    /// Skip `__asm("symbol")` or `__asm__("symbol")` on declarations (GCC symbol renaming).
    fn skip_asm_label(&mut self) {
        if matches!(self.peek(), TokenKind::Asm) {
            self.advance();
            self.skip_balanced_parens();
        }
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
            TokenKind::Ident(s) | TokenKind::Builtin(s) => {
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

        // Skip trailing __attribute__ and __asm("label")
        if matches!(self.peek(), TokenKind::Attribute) { self.parse_attributes(); }
        self.skip_asm_label();

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
                // Skip __attribute__ and __asm("label")
                if matches!(self.peek(), TokenKind::Attribute) { self.parse_attributes(); }
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

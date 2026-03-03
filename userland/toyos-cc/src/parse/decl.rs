use crate::ast::*;
use crate::lex::TokenKind;
use super::Parser;

impl Parser {
    pub(super) fn external_decl(&mut self) -> ExternalDecl {
        let specifiers = self.decl_specifiers();

        if self.peek() == &TokenKind::Semi {
            self.advance();
            return ExternalDecl::Declaration(Declaration { specifiers, declarators: Vec::new() });
        }

        let mut declarator = self.declarator();

        if self.is_function_declarator(&declarator) {
            let is_typedef = specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Typedef)));
            if !is_typedef {
                let mut kr_types: std::collections::HashMap<String, (Vec<DeclSpecifier>, Declarator)> = std::collections::HashMap::new();
                while self.peek() != &TokenKind::LBrace && self.peek() != &TokenKind::Eof {
                    if self.is_declaration_start() {
                        let kr_decl = self.local_declaration();
                        for id in &kr_decl.declarators {
                            if let Some(name) = self.declarator_name(&id.declarator) {
                                kr_types.insert(name, (kr_decl.specifiers.clone(), id.declarator.clone()));
                            }
                        }
                    } else {
                        break;
                    }
                }
                if !kr_types.is_empty() {
                    Self::apply_kr_types(&mut declarator, &kr_types);
                }
                if self.peek() == &TokenKind::LBrace {
                    let body = self.compound_stmt();
                    return ExternalDecl::Function(FunctionDef { specifiers, declarator, body });
                }
            }
        }

        let init = if self.eat(&TokenKind::Eq) { Some(self.initializer()) } else { None };
        self.skip_asm_label();
        let mut declarators = vec![InitDeclarator { declarator, initializer: init }];

        while self.eat(&TokenKind::Comma) {
            let d = self.declarator();
            let init = if self.eat(&TokenKind::Eq) { Some(self.initializer()) } else { None };
            self.skip_asm_label();
            declarators.push(InitDeclarator { declarator: d, initializer: init });
        }
        self.expect(&TokenKind::Semi);

        let is_typedef = specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Typedef)));
        if is_typedef {
            for id in &declarators {
                if let Some(name) = self.declarator_name(&id.declarator) {
                    self.type_env.typedefs.insert(name, crate::types::CType::Int(crate::types::Signedness::Signed));
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

    fn apply_kr_types(declarator: &mut Declarator, kr_types: &std::collections::HashMap<String, (Vec<DeclSpecifier>, Declarator)>) {
        fn apply_to_direct(dd: &mut DirectDeclarator, kr_types: &std::collections::HashMap<String, (Vec<DeclSpecifier>, Declarator)>) {
            match dd {
                DirectDeclarator::Function(_, params) => {
                    for p in &mut params.params {
                        let name = p.declarator.as_ref().and_then(|d| {
                            fn get_name(dd: &DirectDeclarator) -> Option<String> {
                                match dd {
                                    DirectDeclarator::Ident(s) => Some(s.clone()),
                                    DirectDeclarator::Paren(inner) => get_name(&inner.direct),
                                    DirectDeclarator::Array(inner, _) | DirectDeclarator::Function(inner, _) => get_name(inner),
                                }
                            }
                            get_name(&d.direct)
                        });
                        if let Some(name) = name {
                            if let Some((specs, decl)) = kr_types.get(&name) {
                                p.specifiers = specs.clone();
                                p.declarator = Some(decl.clone());
                            }
                        }
                    }
                }
                DirectDeclarator::Paren(inner) => apply_to_direct(&mut inner.direct, kr_types),
                DirectDeclarator::Array(..) | DirectDeclarator::Ident(_) => {
                    panic!("apply_kr_types: expected function declarator")
                }
            }
        }
        apply_to_direct(&mut declarator.direct, kr_types);
    }

    pub(super) fn declarator_name(&self, d: &Declarator) -> Option<String> {
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

    pub(super) fn is_type_keyword(&self, t: &TokenKind) -> bool {
        matches!(t, TokenKind::Void | TokenKind::Char | TokenKind::Short | TokenKind::Int
            | TokenKind::Long | TokenKind::Float | TokenKind::Double | TokenKind::Signed
            | TokenKind::Unsigned | TokenKind::Struct | TokenKind::Union | TokenKind::Enum
            | TokenKind::Const | TokenKind::Volatile | TokenKind::Restrict
            | TokenKind::Typedef | TokenKind::Static | TokenKind::Extern | TokenKind::Register
            | TokenKind::Auto | TokenKind::Inline | TokenKind::Bool | TokenKind::Typeof
            | TokenKind::Int128)
    }

    pub(super) fn is_type_start(&self) -> bool {
        match self.peek() {
            t if self.is_type_keyword(t) => true,
            TokenKind::Ident(name) => self.type_env.is_typedef(name),
            TokenKind::Extension => true,
            _ => false,
        }
    }

    pub(super) fn is_declaration_start(&self) -> bool {
        match self.peek() {
            t if self.is_type_keyword(t) => true,
            TokenKind::Ident(name) => {
                if self.type_env.is_typedef(name) {
                    !matches!(self.peek2(), TokenKind::Colon)
                } else {
                    false
                }
            }
            TokenKind::Extension => true,
            _ => false,
        }
    }

    // Declaration specifiers
    pub(super) fn decl_specifiers(&mut self) -> Vec<DeclSpecifier> {
        let mut specs = Vec::new();
        // Track whether a base type has been established. Once set, typedef names
        // are no longer consumed as type specifiers — they must be declarator names.
        // This resolves the C typedef-name/identifier ambiguity: in `typedef int foo_t[N]`,
        // if `foo_t` is already a known typedef, it must be parsed as the declarator name,
        // not as an additional type specifier after `int`.
        let mut has_base_type = false;
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
                TokenKind::Void => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Void)); has_base_type = true; }
                TokenKind::Char => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Char)); has_base_type = true; }
                TokenKind::Short => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Short)); }
                TokenKind::Int => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Int)); has_base_type = true; }
                TokenKind::Long => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Long)); }
                TokenKind::Float => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Float)); has_base_type = true; }
                TokenKind::Double => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Double)); has_base_type = true; }
                TokenKind::Signed => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Signed)); }
                TokenKind::Unsigned => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Unsigned)); }
                TokenKind::Bool => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Bool)); has_base_type = true; }
                TokenKind::Int128 => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Int128)); has_base_type = true; }

                // Struct/union/enum
                TokenKind::Struct => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Struct(self.struct_or_union_type()))); has_base_type = true; }
                TokenKind::Union => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Union(self.struct_or_union_type()))); has_base_type = true; }
                TokenKind::Enum => { self.advance(); specs.push(DeclSpecifier::TypeSpec(TypeSpec::Enum(self.enum_type()))); has_base_type = true; }

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
                    has_base_type = true;
                }

                // __extension__
                TokenKind::Extension => { self.advance(); }

                // Typedef name — only if no base type has been established yet.
                // Once a base type is known, this identifier must be a declarator name.
                TokenKind::Ident(name) if self.type_env.is_typedef(name) && !has_base_type => {
                    let name = name.clone();
                    self.advance();
                    specs.push(DeclSpecifier::TypeSpec(TypeSpec::TypedefName(name)));
                    has_base_type = true;
                }

                // __builtin_ types (e.g. __builtin_va_list)
                TokenKind::Builtin(name) => {
                    let name = name.clone();
                    self.advance();
                    specs.push(DeclSpecifier::TypeSpec(TypeSpec::Builtin(name)));
                    has_base_type = true;
                }

                _ => break,
            }
        }
        specs
    }

    fn struct_or_union_type(&mut self) -> StructType {
        let name = if let TokenKind::Ident(s) = self.peek() {
            let s = s.clone();
            self.advance();
            Some(s)
        } else {
            None
        };

        let fields = if self.peek() == &TokenKind::LBrace {
            self.advance();
            let mut fields = Vec::new();
            while self.peek() != &TokenKind::RBrace {
                if matches!(self.peek(), TokenKind::Extension) { self.advance(); }
                let spec = self.decl_specifiers();
                let mut declarators = Vec::new();

                if self.peek() == &TokenKind::Semi {
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

        StructType { name, fields }
    }

    fn enum_type(&mut self) -> EnumType {
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
                    if let Some(v) = crate::ast::eval_const_expr(&expr, None) {
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

        EnumType { name, variants }
    }

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

    pub(super) fn skip_asm_label(&mut self) {
        if matches!(self.peek(), TokenKind::Asm) {
            self.advance();
            self.skip_balanced_parens();
        }
    }

    fn parse_pointer(&mut self) -> Vec<Pointer> {
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
        pointer
    }

    pub(super) fn declarator(&mut self) -> Declarator {
        let pointer = self.parse_pointer();
        let direct = self.direct_declarator();
        Declarator { pointer, direct }
    }

    fn direct_declarator(&mut self) -> DirectDeclarator {
        let mut dd = match self.peek() {
            TokenKind::LParen => {
                if self.is_declarator_start_after_paren() {
                    self.advance();
                    let inner = self.declarator();
                    self.expect(&TokenKind::RParen);
                    DirectDeclarator::Paren(Box::new(inner))
                } else {
                    DirectDeclarator::Ident(String::new())
                }
            }
            TokenKind::Ident(s) | TokenKind::Builtin(s) => {
                let s = s.clone();
                self.advance();
                DirectDeclarator::Ident(s)
            }
            _ => DirectDeclarator::Ident(String::new()),
        };

        loop {
            match self.peek() {
                TokenKind::LBracket => {
                    self.advance();
                    while matches!(self.peek(), TokenKind::Const | TokenKind::Volatile | TokenKind::Restrict | TokenKind::Static) {
                        self.advance();
                    }
                    let size = if self.peek() == &TokenKind::RBracket {
                        None
                    } else if self.peek() == &TokenKind::Star && self.peek2() == &TokenKind::RBracket {
                        self.advance();
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

        self.skip_asm_label();

        dd
    }

    fn is_declarator_start_after_paren(&self) -> bool {
        match self.peek2() {
            TokenKind::Star | TokenKind::LParen => true,
            TokenKind::Ident(name) => !self.type_env.is_typedef(name) && !self.is_type_keyword(self.peek2()),
            _ => false,
        }
    }

    fn param_list(&mut self) -> ParamList {
        let mut params = Vec::new();
        let mut variadic = false;

        if self.peek() == &TokenKind::RParen {
            return ParamList { params, variadic };
        }

        if self.peek() == &TokenKind::Void && self.peek2() == &TokenKind::RParen {
            self.advance();
            return ParamList { params, variadic };
        }

        if matches!(self.peek(), TokenKind::Ident(s) if !self.type_env.is_typedef(s)) {
            if matches!(self.peek2(), TokenKind::Comma | TokenKind::RParen) {
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
                Some(self.declarator())
            };
            params.push(ParamDecl { specifiers, declarator });

            if !self.eat(&TokenKind::Comma) { break; }
        }

        ParamList { params, variadic }
    }

    pub(super) fn type_name(&mut self) -> TypeName {
        let specifiers = self.decl_specifiers();
        let declarator = if self.peek() == &TokenKind::RParen || self.peek() == &TokenKind::Comma {
            None
        } else {
            Some(self.abstract_declarator())
        };
        TypeName { specifiers, declarator }
    }

    fn abstract_declarator(&mut self) -> AbstractDeclarator {
        let pointer = self.parse_pointer();
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
        matches!(self.peek2(), TokenKind::Star | TokenKind::LBracket)
    }

    fn is_param_list_start(&self) -> bool {
        self.is_type_keyword(self.peek2()) || matches!(self.peek2(), TokenKind::Ident(name) if self.type_env.is_typedef(name))
    }

    pub(super) fn initializer(&mut self) -> Initializer {
        if self.peek() == &TokenKind::LBrace {
            self.advance();
            let mut items = Vec::new();
            while self.peek() != &TokenKind::RBrace {
                let mut designators = Vec::new();
                let mut gcc_style = false;
                loop {
                    if self.peek() == &TokenKind::Dot {
                        self.advance();
                        let field = self.ident();
                        designators.push(Designator::Field(field));
                    } else if matches!(self.peek(), TokenKind::Ident(_))
                        && self.peek2() == &TokenKind::Colon
                        && designators.is_empty()
                    {
                        let field = self.ident();
                        self.advance();
                        designators.push(Designator::Field(field));
                        gcc_style = true;
                        break;
                    } else if self.peek() == &TokenKind::LBracket {
                        self.advance();
                        let idx = self.conditional_expr();
                        if self.eat(&TokenKind::Ellipsis) {
                            let hi = self.conditional_expr();
                            self.expect(&TokenKind::RBracket);
                            designators.push(Designator::IndexRange(Box::new(idx), Box::new(hi)));
                        } else {
                            self.expect(&TokenKind::RBracket);
                            designators.push(Designator::Index(Box::new(idx)));
                        }
                    } else {
                        break;
                    }
                }
                if !designators.is_empty() && !gcc_style {
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
}

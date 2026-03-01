use super::*;

impl Codegen {
    pub(crate) fn eval_const(&self, expr: &Expr) -> Option<i64> {
        crate::ast::eval_const_expr(expr, Some(&self.type_env.enum_constants))
    }

    pub(crate) fn clif_type(&self, ty: &CType) -> ir::Type {
        match ty {
            CType::Void => I64, // void expressions produce dummy i64
            CType::Bool | CType::Char(_) => I8,
            CType::Short(_) => I16,
            CType::Int(_) | CType::Enum(_) | CType::Float => I32,
            CType::Long(_) | CType::LongLong(_) | CType::Double | CType::Pointer(_) => I64,
            CType::LongDouble | CType::Int128(_) => I128,
            CType::Array(..) | CType::Function(..) => I64, // pointer-like
            CType::Struct(_) | CType::Union(_) => {
                // Small structs can be passed as integers, larger ones by pointer
                match ty.size() {
                    0 => I8,
                    1 => I8,
                    2 => I16,
                    3..=4 => I32,
                    5..=8 => I64,
                    _ => I64, // passed by pointer
                }
            }
        }
    }

    pub(crate) fn is_float_type(&self, ty: &CType) -> bool {
        matches!(ty, CType::Float | CType::Double | CType::LongDouble)
    }

    pub(crate) fn clif_float_type(&self, ty: &CType) -> ir::Type {
        match ty {
            CType::Float => F32,
            CType::Double | CType::LongDouble => F64,
            _ => panic!("not a float type"),
        }
    }

    pub(crate) fn resolve_type(&mut self, specifiers: &[DeclSpecifier]) -> CType {
        let mut is_signed = None;
        let mut is_unsigned = false;
        let mut base = None;
        let mut long_count = 0u8;
        let mut is_short = false;

        for spec in specifiers {
            if let DeclSpecifier::TypeSpec(ts) = spec {
                match ts {
                    TypeSpec::Void => base = Some(CType::Void),
                    TypeSpec::Char => base = Some(CType::Char(true)),
                    TypeSpec::Short => is_short = true,
                    TypeSpec::Int => { if base.is_none() { base = Some(CType::Int(true)); } }
                    TypeSpec::Long => long_count += 1,
                    TypeSpec::Float => base = Some(CType::Float),
                    TypeSpec::Double => base = Some(CType::Double),
                    TypeSpec::Signed => is_signed = Some(true),
                    TypeSpec::Unsigned => { is_unsigned = true; is_signed = Some(false); }
                    TypeSpec::Bool => base = Some(CType::Bool),
                    TypeSpec::Int128 => base = Some(CType::Int128(true)),
                    TypeSpec::TypedefName(name) => {
                        let ty = self.type_env.typedefs.get(name)
                            .unwrap_or_else(|| panic!("unknown typedef '{name}'"))
                            .clone();
                        // If the typedef resolves to an incomplete (forward-declared) struct/union,
                        // look up the tag for the complete definition.
                        // This handles: typedef struct Foo Foo; ... struct Foo { ... };
                        // Also handles: typedef union Foo *FooPtr; ... union Foo { ... };
                        let ty = self.resolve_incomplete_type(ty);
                        base = Some(ty);
                    }
                    TypeSpec::Struct(st) => {
                        let st = st.clone();
                        base = Some(self.resolve_struct(&st, false));
                    }
                    TypeSpec::Union(st) => {
                        let st = st.clone();
                        base = Some(self.resolve_struct(&st, true));
                    }
                    TypeSpec::Enum(et) => {
                        let et = et.clone();
                        base = Some(self.resolve_enum(&et));
                    }
                    TypeSpec::Typeof(expr) => {
                        // typeof(expr) — deduce the type from the expression
                        if let Some(ty) = self.expr_type_for_typeof(expr) {
                            base = Some(ty);
                        }
                    }
                    TypeSpec::TypeofType(tn) => {
                        base = Some(self.resolve_typename(tn));
                    }
                    _ => {}
                }
            }
        }

        let signed = is_signed.unwrap_or(true);

        if is_short {
            return CType::Short(signed);
        }
        if long_count >= 2 {
            return CType::LongLong(signed);
        }
        if long_count == 1 {
            if matches!(base, Some(CType::Double)) {
                return CType::LongDouble;
            }
            return CType::Long(signed);
        }

        match base {
            Some(CType::Char(_)) => CType::Char(signed),
            Some(CType::Int(_)) => CType::Int(signed),
            Some(CType::Int128(_)) => CType::Int128(signed),
            Some(other) => other,
            None => {
                if is_unsigned { CType::Int(false) }
                else if is_signed.is_some() { CType::Int(true) }
                else { CType::Int(true) } // default
            }
        }
    }

    fn resolve_incomplete_type(&self, ty: CType) -> CType {
        match ty {
            CType::Struct(ref def) if def.fields.is_empty() => {
                def.name.as_ref()
                    .and_then(|n| self.type_env.tags.get(n))
                    .cloned()
                    .unwrap_or(ty)
            }
            CType::Union(ref def) if def.fields.is_empty() => {
                def.name.as_ref()
                    .and_then(|n| self.type_env.tags.get(n))
                    .cloned()
                    .unwrap_or(ty)
            }
            CType::Pointer(inner) => {
                let resolved = self.resolve_incomplete_type(*inner);
                CType::Pointer(Box::new(resolved))
            }
            _ => ty,
        }
    }

    fn resolve_struct(&mut self, st: &StructType, is_union: bool) -> CType {
        verbose!("resolve_struct: {:?} is_union={}", st.name, is_union);
        let packed = st.attributes.iter().any(|a| a.name == "packed");

        if st.fields.is_none() {
            // Forward reference or usage of previously defined struct
            if let Some(tag_name) = &st.name {
                if let Some(existing) = self.type_env.tags.get(tag_name) {
                    return existing.clone();
                }
            }
            // Unknown forward declaration — return empty struct
            let def = StructDef { name: st.name.clone(), fields: Vec::new(), packed };
            return if is_union { CType::Union(def) } else { CType::Struct(def) };
        }

        let mut fields = Vec::new();
        for f in st.fields.as_ref().unwrap() {
            let ty = self.resolve_type(&f.specifiers);
            for fd in &f.declarators {
                let field_ty = if let Some(d) = &fd.declarator {
                    self.apply_declarator(&ty, d)
                } else {
                    ty.clone()
                };
                let name = fd.declarator.as_ref().and_then(|d| self.get_declarator_name(d));
                let bit_width = fd.bit_width.as_ref().map(|bw| {
                    crate::ast::eval_const_expr(bw, Some(&self.type_env.enum_constants)).unwrap_or(0) as u32
                });
                fields.push(FieldDef { name, ty: field_ty, bit_width });
            }
        }

        let def = StructDef { name: st.name.clone(), fields, packed };
        let ty = if is_union { CType::Union(def) } else { CType::Struct(def) };

        // Register named struct/union tag
        if let Some(tag_name) = &st.name {
            self.type_env.tags.insert(tag_name.clone(), ty.clone());
        }

        ty
    }

    fn resolve_enum(&mut self, et: &EnumType) -> CType {
        if et.variants.is_none() {
            // Forward reference or usage of previously defined enum
            if let Some(tag_name) = &et.name {
                if let Some(existing) = self.type_env.tags.get(tag_name) {
                    return existing.clone();
                }
            }
            return CType::Enum(EnumDef { name: et.name.clone(), variants: Vec::new() });
        }

        let mut variants = Vec::new();
        let mut next_val = 0i64;
        for v in et.variants.as_ref().unwrap() {
            if let Some(expr) = &v.value {
                if let Some(val) = crate::ast::eval_const_expr(expr, Some(&self.type_env.enum_constants)) {
                    next_val = val;
                }
            }
            variants.push((v.name.clone(), next_val));
            self.type_env.enum_constants.insert(v.name.clone(), next_val);
            next_val += 1;
        }

        let ty = CType::Enum(EnumDef { name: et.name.clone(), variants });

        // Register named enum tag
        if let Some(tag_name) = &et.name {
            self.type_env.tags.insert(tag_name.clone(), ty.clone());
        }

        ty
    }

    pub(crate) fn apply_declarator(&mut self, base: &CType, d: &Declarator) -> CType {
        // C declarators: postfix (function/array) binds tighter than prefix (pointer).
        // So `char *f()` = function returning char*, `char (*fp)()` = pointer to function returning char.
        // When the direct declarator is a Paren like (*fp), pointers wrap the outer type.
        // Otherwise, pointers modify the base (return) type.
        if matches!(d.direct, DirectDeclarator::Paren(_)) {
            // (*fp)() case: first apply inner, then wrap with pointers
            let mut ty = self.apply_direct_declarator(base, &d.direct);
            for _ in &d.pointer {
                ty = CType::Pointer(Box::new(ty));
            }
            ty
        } else {
            // *f() case: pointers modify the base type (return type)
            let mut ptr_base = base.clone();
            for _ in &d.pointer {
                ptr_base = CType::Pointer(Box::new(ptr_base));
            }
            self.apply_direct_declarator(&ptr_base, &d.direct)
        }
    }

    fn apply_direct_declarator(&mut self, base: &CType, dd: &DirectDeclarator) -> CType {
        match dd {
            DirectDeclarator::Ident(_) => base.clone(),
            DirectDeclarator::Paren(inner) => {
                let inner = inner.clone();
                self.apply_declarator(base, &inner)
            }
            DirectDeclarator::Array(inner, size) => {
                let n = size.as_ref().and_then(|e| crate::ast::eval_const_expr(e, Some(&self.type_env.enum_constants)).map(|v| v as usize));
                // Same inside-out logic as Function: for (*arr)[N], the * wraps
                // the array type (pointer-to-array), not the element type.
                match inner.as_ref() {
                    DirectDeclarator::Paren(inner_decl) => {
                        let arr_type = CType::Array(Box::new(base.clone()), n);
                        let inner_decl = inner_decl.clone();
                        self.apply_declarator(&arr_type, &inner_decl)
                    }
                    _ => {
                        let elem = self.apply_direct_declarator(base, inner);
                        CType::Array(Box::new(elem), n)
                    }
                }
            }
            DirectDeclarator::Function(inner, params) => {
                let params_clone = params.clone();
                let mut param_types = Vec::new();
                for p in &params_clone.params {
                    let ty = self.resolve_type(&p.specifiers);
                    let ty = if let Some(d) = &p.declarator {
                        self.apply_declarator(&ty, d)
                    } else {
                        ty
                    };
                    let name = p.declarator.as_ref().and_then(|d| self.get_declarator_name(d));
                    param_types.push(ParamType { name, ty });
                }
                // C declarators are inside-out: for (*fp)(args), the * inside the
                // parens wraps the function type (pointer-to-function), not the
                // return type. Build the function type first, then let the Paren's
                // declarator apply its pointers around it.
                match inner.as_ref() {
                    DirectDeclarator::Paren(inner_decl) => {
                        let func_type = CType::Function(Box::new(base.clone()), param_types, params_clone.variadic);
                        let inner_decl = inner_decl.clone();
                        self.apply_declarator(&func_type, &inner_decl)
                    }
                    _ => {
                        let ret = self.apply_direct_declarator(base, inner);
                        CType::Function(Box::new(ret), param_types, params_clone.variadic)
                    }
                }
            }
        }
    }

    /// Resolve a full TypeName (specifiers + optional abstract declarator) to CType.
    pub(crate) fn resolve_typename(&mut self, tn: &TypeName) -> CType {
        let base = self.resolve_type(&tn.specifiers);
        if let Some(ad) = &tn.declarator {
            self.apply_abstract_declarator(&base, ad)
        } else {
            base
        }
    }

    fn apply_abstract_declarator(&mut self, base: &CType, ad: &AbstractDeclarator) -> CType {
        if ad.direct.as_ref().is_some_and(|d| matches!(d, DirectAbstractDeclarator::Paren(_))) {
            // (*)(args) case: first apply inner, then wrap with pointers
            let mut ty = self.apply_direct_abstract_declarator(base, ad.direct.as_ref().unwrap());
            for _ in &ad.pointer {
                ty = CType::Pointer(Box::new(ty));
            }
            ty
        } else {
            let mut ptr_base = base.clone();
            for _ in &ad.pointer {
                ptr_base = CType::Pointer(Box::new(ptr_base));
            }
            if let Some(dad) = &ad.direct {
                self.apply_direct_abstract_declarator(&ptr_base, dad)
            } else {
                ptr_base
            }
        }
    }

    fn apply_direct_abstract_declarator(&mut self, base: &CType, dad: &DirectAbstractDeclarator) -> CType {
        match dad {
            DirectAbstractDeclarator::Paren(inner) => {
                self.apply_abstract_declarator(base, inner)
            }
            DirectAbstractDeclarator::Array(inner, size) => {
                let n = size.as_ref().and_then(|e| crate::ast::eval_const_expr(e, Some(&self.type_env.enum_constants)).map(|v| v as usize));
                if let Some(inner) = inner {
                    let elem = self.apply_direct_abstract_declarator(base, inner);
                    CType::Array(Box::new(elem), n)
                } else {
                    CType::Array(Box::new(base.clone()), n)
                }
            }
            DirectAbstractDeclarator::Function(inner, params) => {
                let params_clone = params.clone();
                let mut param_types = Vec::new();
                for p in &params_clone.params {
                    let ty = self.resolve_type(&p.specifiers);
                    let ty = if let Some(d) = &p.declarator {
                        self.apply_declarator(&ty, d)
                    } else {
                        ty
                    };
                    let name = p.declarator.as_ref().and_then(|d| self.get_declarator_name(d));
                    param_types.push(ParamType { name, ty });
                }
                if let Some(inner) = inner {
                    let ret = self.apply_direct_abstract_declarator(base, inner);
                    CType::Function(Box::new(ret), param_types, params_clone.variadic)
                } else {
                    CType::Function(Box::new(base.clone()), param_types, params_clone.variadic)
                }
            }
        }
    }

    pub(crate) fn get_declarator_name(&self, d: &Declarator) -> Option<String> {
        self.get_direct_name(&d.direct)
    }

    fn get_direct_name(&self, dd: &DirectDeclarator) -> Option<String> {
        match dd {
            DirectDeclarator::Ident(s) if !s.is_empty() => Some(s.clone()),
            DirectDeclarator::Ident(_) => None,
            DirectDeclarator::Paren(inner) => self.get_declarator_name(inner),
            DirectDeclarator::Array(inner, _) | DirectDeclarator::Function(inner, _) => self.get_direct_name(inner),
        }
    }

    /// Resolve the type of an expression for typeof() — works without a FuncCtx
    fn expr_type_for_typeof(&mut self, expr: &Expr) -> Option<CType> {
        match expr {
            Expr::Ident(name) => {
                self.global_types.get(name).cloned()
            }
            Expr::IntLit(_) => Some(CType::Int(true)),
            Expr::UIntLit(_) => Some(CType::Long(false)),
            Expr::FloatLit(_, is_f32) => Some(if *is_f32 { CType::Float } else { CType::Double }),
            Expr::StringLit(_) => Some(CType::Pointer(Box::new(CType::Char(true)))),
            Expr::WideStringLit(_) => Some(CType::Pointer(Box::new(CType::Int(true)))),
            Expr::Unary(UnaryOp::Deref, e) => {
                let ty = self.expr_type_for_typeof(e)?;
                match ty {
                    CType::Pointer(inner) => Some(*inner),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

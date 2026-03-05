use super::*;

impl Codegen {
    pub(crate) fn eval_const(&self, expr: &Expr) -> Option<i64> {
        match expr {
            Expr::IntLit(v) => Some(*v as i64),
            Expr::UIntLit(v) => Some(*v as i64),
            Expr::CharLit(c) => Some(*c as i64),
            Expr::Ident(name) => self.type_env.enum_constants.get(name).copied(),
            Expr::Unary(UnaryOp::Neg, e) => self.eval_const(e).map(|v| -v),
            Expr::Unary(UnaryOp::BitNot, e) => self.eval_const(e).map(|v| !v),
            Expr::Unary(UnaryOp::LogNot, e) => self.eval_const(e).map(|v| (v == 0) as i64),
            Expr::Unary(UnaryOp::AddrOf, inner) => self.eval_member_offset(inner),
            Expr::Binary(op, l, r) => {
                let l = self.eval_const(l)?;
                let r = self.eval_const(r)?;
                Some(match op {
                    BinOp::Add => l + r,
                    BinOp::Sub => l - r,
                    BinOp::Mul => l * r,
                    BinOp::Div => { assert!(r != 0, "division by zero in constant expression"); l / r },
                    BinOp::Mod => { assert!(r != 0, "modulo by zero in constant expression"); l % r },
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
            Expr::Conditional(cond, then_e, else_e) => {
                let c = self.eval_const(cond)?;
                if c != 0 { self.eval_const(then_e) } else { self.eval_const(else_e) }
            }
            Expr::Cast(_, e) => self.eval_const(e),
            Expr::Sizeof(arg) => {
                let size = match arg.as_ref() {
                    SizeofArg::Type(tn) => self.resolve_typename_const(tn)?.size(),
                    SizeofArg::Expr(e) => self.const_expr_type(e)?.size(),
                };
                Some(size as i64)
            }
            Expr::FloatLit(..) | Expr::StringLit(_) | Expr::WideStringLit(_)
            | Expr::Unary(UnaryOp::Deref | UnaryOp::PreInc | UnaryOp::PreDec, _)
            | Expr::PostUnary(..) | Expr::Alignof(_)
            | Expr::Call(..) | Expr::Member(..) | Expr::Arrow(..) | Expr::Index(..)
            | Expr::Assign(..) | Expr::Comma(..) | Expr::CompoundLiteral(..)
            | Expr::StmtExpr(_) | Expr::VaArg(..) | Expr::Builtin(..) => None,
        }
    }

    /// Get the type of a constant expression (for sizeof(expr)).
    fn const_expr_type(&self, expr: &Expr) -> Option<CType> {
        match expr {
            Expr::StringLit(data) => Some(CType::Array(
                Box::new(CType::Char(Signedness::Signed)),
                Some(data.len() + 1),
            )),
            Expr::IntLit(_) | Expr::CharLit(_) => Some(CType::Int(Signedness::Signed)),
            Expr::UIntLit(_) => Some(CType::Long(Signedness::Unsigned)),
            Expr::FloatLit(_, is_f32) => {
                if *is_f32 { Some(CType::Float) } else { Some(CType::Double) }
            }
            Expr::Ident(name) => self.local_types.get(name).or_else(|| self.global_types.get(name)).cloned(),
            Expr::Unary(UnaryOp::Deref, e) => {
                let ty = self.const_expr_type(e)?;
                match ty {
                    CType::Pointer(inner) | CType::Array(inner, _) => Some(*inner),
                    CType::Void | CType::Bool | CType::Char(_) | CType::Short(_) | CType::Int(_)
                    | CType::Long(_) | CType::LongLong(_) | CType::Int128(_) | CType::Float
                    | CType::Double | CType::LongDouble | CType::Enum(_)
                    | CType::Function(..) | CType::Struct(_) | CType::Union(_) => None,
                }
            }
            Expr::Index(base, _) => {
                let ty = self.const_expr_type(base)?;
                match ty {
                    CType::Array(inner, _) | CType::Pointer(inner) => Some(*inner),
                    CType::Void | CType::Bool | CType::Char(_) | CType::Short(_) | CType::Int(_)
                    | CType::Long(_) | CType::LongLong(_) | CType::Int128(_) | CType::Float
                    | CType::Double | CType::LongDouble | CType::Enum(_)
                    | CType::Function(..) | CType::Struct(_) | CType::Union(_) => None,
                }
            }
            Expr::Member(base, field) | Expr::Arrow(base, field) => {
                let ty = if matches!(expr, Expr::Arrow(..)) {
                    match self.const_expr_type(base)? {
                        CType::Pointer(inner) => *inner,
                        t => t,
                    }
                } else {
                    self.const_expr_type(base)?
                };
                ty.field_offset(field).map(|fi| fi.ty)
            }
            Expr::WideStringLit(_) | Expr::Binary(..) | Expr::Unary(..) | Expr::PostUnary(..)
            | Expr::Cast(..) | Expr::Sizeof(_) | Expr::Alignof(_) | Expr::Conditional(..)
            | Expr::Call(..) | Expr::Assign(..) | Expr::Comma(..)
            | Expr::CompoundLiteral(..) | Expr::StmtExpr(_) | Expr::VaArg(..)
            | Expr::Builtin(..) => None,
        }
    }

    /// Compute the byte offset of a member access chain rooted at a null-pointer cast,
    /// e.g. `((StructType*)0)->field` or `((StructType*)0)->field.sub`.
    fn eval_member_offset(&self, expr: &Expr) -> Option<i64> {
        match expr {
            Expr::Arrow(base, field) => {
                // base should be (StructType*)0
                let struct_ty = self.null_pointer_cast_type(base)?;
                let fi = struct_ty.field_offset(field)?;
                Some(fi.byte_offset as i64)
            }
            Expr::Member(base, field) => {
                // Nested member: base.field — accumulate offset
                let (base_offset, base_ty) = self.eval_member_with_type(base)?;
                let fi = base_ty.field_offset(field)?;
                Some((base_offset + fi.byte_offset) as i64)
            }
            Expr::IntLit(_) | Expr::UIntLit(_) | Expr::FloatLit(..) | Expr::CharLit(_)
            | Expr::StringLit(_) | Expr::WideStringLit(_) | Expr::Ident(_)
            | Expr::Binary(..) | Expr::Unary(..) | Expr::PostUnary(..) | Expr::Cast(..)
            | Expr::Sizeof(_) | Expr::Alignof(_) | Expr::Conditional(..)
            | Expr::Call(..) | Expr::Index(..) | Expr::Assign(..) | Expr::Comma(..)
            | Expr::CompoundLiteral(..) | Expr::StmtExpr(_) | Expr::VaArg(..)
            | Expr::Builtin(..) => None,
        }
    }

    /// Return (accumulated_offset, resulting_type) for a member access chain.
    fn eval_member_with_type(&self, expr: &Expr) -> Option<(usize, CType)> {
        match expr {
            Expr::Arrow(base, field) => {
                let struct_ty = self.null_pointer_cast_type(base)?;
                let fi = struct_ty.field_offset(field)?;
                Some((fi.byte_offset, fi.ty))
            }
            Expr::Member(base, field) => {
                let (base_offset, base_ty) = self.eval_member_with_type(base)?;
                let fi = base_ty.field_offset(field)?;
                Some((base_offset + fi.byte_offset, fi.ty))
            }
            Expr::IntLit(_) | Expr::UIntLit(_) | Expr::FloatLit(..) | Expr::CharLit(_)
            | Expr::StringLit(_) | Expr::WideStringLit(_) | Expr::Ident(_)
            | Expr::Binary(..) | Expr::Unary(..) | Expr::PostUnary(..) | Expr::Cast(..)
            | Expr::Sizeof(_) | Expr::Alignof(_) | Expr::Conditional(..)
            | Expr::Call(..) | Expr::Index(..) | Expr::Assign(..) | Expr::Comma(..)
            | Expr::CompoundLiteral(..) | Expr::StmtExpr(_) | Expr::VaArg(..)
            | Expr::Builtin(..) => None,
        }
    }

    /// If expr is `(SomeStruct*)0`, resolve and return `SomeStruct`.
    fn null_pointer_cast_type(&self, expr: &Expr) -> Option<CType> {
        if let Expr::Cast(tn, inner) = expr {
            if matches!(inner.as_ref(), Expr::IntLit(0)) {
                let ty = self.resolve_typename_const(tn)?;
                // Unwrap the pointer to get the struct type
                if let CType::Pointer(inner) = ty {
                    // Resolve incomplete (forward-declared) structs/unions
                    let resolved = self.resolve_incomplete_type(*inner);
                    return Some(resolved);
                }
            }
        }
        None
    }

    /// Resolve a TypeName without mutating self (for const eval contexts).
    fn resolve_typename_const(&self, tn: &TypeName) -> Option<CType> {
        let base = self.resolve_type_const(&tn.specifiers)?;
        if let Some(ad) = &tn.declarator {
            Some(self.apply_abstract_declarator_const(&base, ad))
        } else {
            Some(base)
        }
    }

    fn resolve_type_const(&self, specifiers: &[DeclSpecifier]) -> Option<CType> {
        use Signedness::*;
        let mut explicit_sign: Option<Signedness> = None;
        let mut base = None;
        let mut long_count = 0u8;
        let mut is_short = false;
        for spec in specifiers {
            if let DeclSpecifier::TypeSpec(ts) = spec {
                match ts {
                    TypeSpec::Void => base = Some(CType::Void),
                    TypeSpec::Char => base = Some(CType::Char(Signed)),
                    TypeSpec::Short => is_short = true,
                    TypeSpec::Int => { if base.is_none() { base = Some(CType::Int(Signed)); } }
                    TypeSpec::Long => long_count += 1,
                    TypeSpec::Float => base = Some(CType::Float),
                    TypeSpec::Double => base = Some(CType::Double),
                    TypeSpec::Signed => explicit_sign = Some(Signed),
                    TypeSpec::Unsigned => explicit_sign = Some(Unsigned),
                    TypeSpec::Bool => base = Some(CType::Bool),
                    TypeSpec::Int128 => base = Some(CType::Int128(Signed)),
                    TypeSpec::TypedefName(name) => {
                        base = Some(self.type_env.typedefs.get(name)?.clone());
                    }
                    TypeSpec::Struct(st) => {
                        if st.fields.is_none() {
                            base = st.name.as_ref().and_then(|n| self.type_env.tags.get(n)).cloned();
                        } else {
                            return None; // not handling inline struct defs in const eval
                        }
                    }
                    TypeSpec::Union(st) => {
                        if st.fields.is_none() {
                            base = st.name.as_ref().and_then(|n| self.type_env.tags.get(n)).cloned();
                        } else {
                            return None;
                        }
                    }
                    TypeSpec::Enum(et) => {
                        if let Some(tag_name) = &et.name {
                            base = self.type_env.tags.get(tag_name).cloned();
                        }
                    }
                    TypeSpec::Typeof(_) | TypeSpec::TypeofType(_) => return None,
                    TypeSpec::Builtin(name) => panic!("unsupported builtin type specifier: {name}"),
                }
            }
        }
        let sign = |default| explicit_sign.unwrap_or(default);
        if is_short { return Some(CType::Short(sign(Signed))); }
        if long_count >= 2 { return Some(CType::LongLong(sign(Signed))); }
        if long_count == 1 {
            if matches!(base, Some(CType::Double)) { return Some(CType::LongDouble); }
            return Some(CType::Long(sign(Signed)));
        }
        match base {
            Some(CType::Char(s)) => Some(CType::Char(sign(s))),
            Some(CType::Int(s)) => Some(CType::Int(sign(s))),
            Some(CType::Int128(s)) => Some(CType::Int128(sign(s))),
            Some(other) => Some(other),
            None => Some(CType::Int(sign(Signed))),
        }
    }

    fn apply_abstract_declarator_const(&self, base: &CType, ad: &AbstractDeclarator) -> CType {
        let mut ty = base.clone();
        for _ in &ad.pointer {
            ty = CType::Pointer(Box::new(ty));
        }
        ty
    }

    pub(crate) fn clif_type(&self, ty: &CType) -> ir::Type {
        match ty {
            CType::Void => I64, // void expressions produce dummy i64
            CType::Bool | CType::Char(_) => I8,
            CType::Short(_) => I16,
            CType::Int(_) | CType::Enum(_) => I32,
            CType::Float => F32,
            CType::Long(_) | CType::LongLong(_) | CType::Pointer(_) => I64,
            CType::Double | CType::LongDouble => F64,
            CType::Int128(_) => I128,
            CType::Array(..) | CType::Function(..) => I64, // pointer-like
            CType::Struct(_) | CType::Union(_) => {
                match ty.size() {
                    0..=1 => I8,
                    2 => I16,
                    3..=4 => I32,
                    _ => I64,
                }
            }
        }
    }

    pub(crate) fn resolve_type(&mut self, specifiers: &[DeclSpecifier]) -> CType {
        use Signedness::*;
        let mut explicit_sign: Option<Signedness> = None;
        let mut base = None;
        let mut long_count = 0u8;
        let mut is_short = false;

        for spec in specifiers {
            if let DeclSpecifier::TypeSpec(ts) = spec {
                match ts {
                    TypeSpec::Void => base = Some(CType::Void),
                    TypeSpec::Char => base = Some(CType::Char(Signed)),
                    TypeSpec::Short => is_short = true,
                    TypeSpec::Int => { if base.is_none() { base = Some(CType::Int(Signed)); } }
                    TypeSpec::Long => long_count += 1,
                    TypeSpec::Float => base = Some(CType::Float),
                    TypeSpec::Double => base = Some(CType::Double),
                    TypeSpec::Signed => explicit_sign = Some(Signed),
                    TypeSpec::Unsigned => explicit_sign = Some(Unsigned),
                    TypeSpec::Bool => base = Some(CType::Bool),
                    TypeSpec::Int128 => base = Some(CType::Int128(Signed)),
                    TypeSpec::TypedefName(name) => {
                        let ty = self.type_env.typedefs.get(name)
                            .unwrap_or_else(|| panic!("unknown typedef '{name}'"))
                            .clone();
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
                        base = Some(self.expr_type_for_typeof(expr));
                    }
                    TypeSpec::TypeofType(tn) => {
                        base = Some(self.resolve_typename(tn));
                    }
                    TypeSpec::Builtin(name) => panic!("unsupported builtin type specifier: {name}"),
                }
            }
        }

        let sign = |default| explicit_sign.unwrap_or(default);
        if is_short { return CType::Short(sign(Signed)); }
        if long_count >= 2 { return CType::LongLong(sign(Signed)); }
        if long_count == 1 {
            if matches!(base, Some(CType::Double)) { return CType::LongDouble; }
            return CType::Long(sign(Signed));
        }

        match base {
            Some(CType::Char(s)) => CType::Char(sign(s)),
            Some(CType::Int(s)) => CType::Int(sign(s)),
            Some(CType::Int128(s)) => CType::Int128(sign(s)),
            Some(other) => other,
            None => CType::Int(sign(Signed)),
        }
    }

    pub(crate) fn resolve_incomplete_type(&self, ty: CType) -> CType {
        match ty {
            // Forward-declared struct/union: try to resolve, keep incomplete if not yet defined (opaque types)
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
            CType::Void | CType::Bool | CType::Char(_) | CType::Short(_) | CType::Int(_)
            | CType::Long(_) | CType::LongLong(_) | CType::Int128(_) | CType::Float
            | CType::Double | CType::LongDouble | CType::Array(..) | CType::Function(..)
            | CType::Enum(_) | CType::Struct(_) | CType::Union(_) => ty,
        }
    }

    fn resolve_struct(&mut self, st: &StructType, is_union: bool) -> CType {
        verbose!("resolve_struct: {:?} is_union={}", st.name, is_union);

        if st.fields.is_none() {
            // Forward reference or usage of previously defined struct
            if let Some(tag_name) = &st.name {
                if let Some(existing) = self.type_env.tags.get(tag_name) {
                    return existing.clone();
                }
            }
            // Unknown forward declaration — return empty struct
            let def = StructDef { name: st.name.clone(), fields: Vec::new() };
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
                let name = fd.declarator.as_ref().map(|d| self.get_declarator_name(d)).filter(|n| !n.is_empty());
                let bit_width = fd.bit_width.as_ref().map(|bw| {
                    self.eval_const(bw)
                        .expect("bitfield width must be a constant expression") as u32
                });
                fields.push(FieldDef { name, ty: field_ty, bit_width });
            }
        }

        let def = StructDef { name: st.name.clone(), fields };
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
                if let Some(val) = self.eval_const(expr) {
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
                let n = size.as_ref().and_then(|e| self.eval_const(e).map(|v| v as usize));
                // Same inside-out logic as Function: for (*arr)[N], the * wraps
                // the array type (pointer-to-array), not the element type.
                match inner.as_ref() {
                    DirectDeclarator::Paren(inner_decl) => {
                        let arr_type = CType::Array(Box::new(base.clone()), n);
                        let inner_decl = inner_decl.clone();
                        self.apply_declarator(&arr_type, &inner_decl)
                    }
                    DirectDeclarator::Ident(_) | DirectDeclarator::Array(..)
                    | DirectDeclarator::Function(..) => {
                        // C declarator inside-out: for `int a[6][2]`, the parser
                        // builds Array(Array(Ident, 6), 2). The rightmost [2] is
                        // closest to the element type, so we wrap base with our
                        // dimension first, then let the inner declarator wrap that.
                        // Result: Array(Array(Int, 2), 6) — array of 6 arrays of 2 ints.
                        let arr_type = CType::Array(Box::new(base.clone()), n);
                        self.apply_direct_declarator(&arr_type, inner)
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
                    let name = p.declarator.as_ref().map(|d| self.get_declarator_name(d)).filter(|n| !n.is_empty());
                    param_types.push(ParamType { name, ty });
                }
                // C declarators are inside-out: for (*fp)(args), the * inside the
                // parens wraps the function type (pointer-to-function), not the
                // return type. Build the function type first, then let the Paren's
                // declarator apply its pointers around it.
                let unspecified = params_clone.unspecified_params;
                match inner.as_ref() {
                    DirectDeclarator::Paren(inner_decl) => {
                        let func_type = CType::Function(Box::new(base.clone()), param_types, params_clone.variadic, unspecified);
                        let inner_decl = inner_decl.clone();
                        self.apply_declarator(&func_type, &inner_decl)
                    }
                    DirectDeclarator::Ident(_) | DirectDeclarator::Array(..)
                    | DirectDeclarator::Function(..) => {
                        let ret = self.apply_direct_declarator(base, inner);
                        CType::Function(Box::new(ret), param_types, params_clone.variadic, unspecified)
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
                let n = size.as_ref().and_then(|e| self.eval_const(e).map(|v| v as usize));
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
                    let name = p.declarator.as_ref().map(|d| self.get_declarator_name(d)).filter(|n| !n.is_empty());
                    param_types.push(ParamType { name, ty });
                }
                let unspecified = params_clone.unspecified_params;
                if let Some(inner) = inner {
                    let ret = self.apply_direct_abstract_declarator(base, inner);
                    CType::Function(Box::new(ret), param_types, params_clone.variadic, unspecified)
                } else {
                    CType::Function(Box::new(base.clone()), param_types, params_clone.variadic, unspecified)
                }
            }
        }
    }

    pub(crate) fn get_declarator_name(&self, d: &Declarator) -> String {
        self.get_direct_name(&d.direct)
    }

    fn get_direct_name(&self, dd: &DirectDeclarator) -> String {
        match dd {
            DirectDeclarator::Ident(s) => s.clone(),
            DirectDeclarator::Paren(inner) => self.get_declarator_name(inner),
            DirectDeclarator::Array(inner, _) | DirectDeclarator::Function(inner, _) => self.get_direct_name(inner),
        }
    }

    /// Resolve the type of an expression for typeof() — works without a FuncCtx
    fn expr_type_for_typeof(&mut self, expr: &Expr) -> CType {
        match expr {
            Expr::Ident(name) => {
                self.global_types.get(name)
                    .or_else(|| self.func_ctypes.get(name))
                    .cloned()
                    .unwrap_or_else(|| panic!("typeof: unknown identifier '{name}'"))
            }
            Expr::IntLit(_) => CType::Int(Signedness::Signed),
            Expr::UIntLit(_) => CType::Long(Signedness::Unsigned),
            Expr::FloatLit(_, is_f32) => if *is_f32 { CType::Float } else { CType::Double },
            Expr::StringLit(_) => CType::Pointer(Box::new(CType::Char(Signedness::Signed))),
            Expr::WideStringLit(_) => CType::Pointer(Box::new(CType::Int(Signedness::Signed))),
            Expr::Unary(UnaryOp::Deref, e) => {
                let ty = self.expr_type_for_typeof(e);
                match ty {
                    CType::Pointer(inner) => *inner,
                    CType::Void | CType::Bool | CType::Char(_) | CType::Short(_) | CType::Int(_)
                    | CType::Long(_) | CType::LongLong(_) | CType::Int128(_) | CType::Float
                    | CType::Double | CType::LongDouble | CType::Array(..) | CType::Enum(_)
                    | CType::Function(..) | CType::Struct(_) | CType::Union(_)
                    => panic!("typeof: deref of non-pointer type {ty:?}"),
                }
            }
            Expr::Binary(..) | Expr::PostUnary(..) | Expr::Cast(..) | Expr::Sizeof(_)
            | Expr::Alignof(_) | Expr::Conditional(..) | Expr::Call(..) | Expr::Member(..)
            | Expr::Arrow(..) | Expr::Index(..) | Expr::Assign(..) | Expr::Comma(..)
            | Expr::CompoundLiteral(..) | Expr::StmtExpr(_) | Expr::VaArg(..)
            | Expr::Builtin(..) | Expr::CharLit(_)
            | Expr::Unary(UnaryOp::Neg | UnaryOp::BitNot | UnaryOp::LogNot | UnaryOp::AddrOf | UnaryOp::PreInc | UnaryOp::PreDec, _)
            => panic!("typeof: unhandled expression {expr:?}"),
        }
    }
}

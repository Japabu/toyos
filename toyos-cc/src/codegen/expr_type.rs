use super::*;

impl Codegen {
    pub(crate) fn expr_type(&mut self, ctx: &FuncCtx, expr: &Expr) -> CType {
        verbose_enter!("expr_type", "{:?}", std::mem::discriminant(expr));
        let result = self.expr_type_inner(ctx, expr);
        verbose!("expr_type => {:?}", result);
        verbose_leave!();
        result
    }

    fn expr_type_inner(&mut self, ctx: &FuncCtx, expr: &Expr) -> CType {
        match expr {
            Expr::Ident(name) => {
                if let Some((_, ty)) = ctx.locals.get(name) {
                    return ty.clone();
                }
                if let Some(ty) = self.global_types.get(name) {
                    return ty.clone();
                }
                if let Some(fty) = self.func_ctypes.get(name) {
                    return fty.clone();
                }
                if self.type_env.enum_constants.contains_key(name) {
                    return CType::Int(Signedness::Signed);
                }
                panic!("expr_type: unknown identifier '{name}'")
            }
            Expr::Arrow(e, field) => {
                let base_ty = self.expr_type(ctx, e);
                let pointee = match base_ty {
                    CType::Pointer(inner) | CType::Array(inner, _) => *inner,
                    other => panic!("expr_type: arrow on non-pointer type {other:?}"),
                };
                let pointee = self.resolve_incomplete_type(pointee);
                let fi = pointee.field_offset(field)
                    .unwrap_or_else(|| panic!("expr_type: no field '{field}' in {pointee:?}"));
                CType::promote_integer(fi.ty, fi.bit_width)
            }
            Expr::Member(e, field) => {
                let base_ty = self.expr_type(ctx, e);
                let base_ty = self.resolve_incomplete_type(base_ty);
                let fi = base_ty.field_offset(field)
                    .unwrap_or_else(|| panic!("expr_type: no field '{field}' in {base_ty:?}"));
                CType::promote_integer(fi.ty, fi.bit_width)
            }
            Expr::Unary(UnaryOp::Deref, e) => {
                let ty = self.expr_type(ctx, e);
                match ty {
                    CType::Pointer(inner) | CType::Array(inner, _) => *inner,
                    other => panic!("expr_type: deref of non-pointer type {other:?}"),
                }
            }
            Expr::Unary(UnaryOp::AddrOf, e) => {
                let ty = self.expr_type(ctx, e);
                CType::Pointer(Box::new(ty))
            }
            Expr::Index(arr, _) => {
                let ty = self.expr_type(ctx, arr);
                match ty {
                    CType::Pointer(inner) | CType::Array(inner, _) => *inner,
                    other => panic!("expr_type: index on non-pointer/array type {other:?}"),
                }
            }
            Expr::Call(func, _) => {
                if let Expr::Ident(name) = func.as_ref() {
                    if let Some(ret_ty) = self.func_ret_types.get(name) {
                        return ret_ty.clone();
                    }
                    // C89 implicit declaration: assume returns int
                    return CType::Int(Signedness::Signed);
                }
                // Indirect call: derive return type from callee's function pointer type
                let callee_ty = self.expr_type(ctx, func);
                match &callee_ty {
                    CType::Function(ret, _, _, _) => ret.as_ref().clone(),
                    CType::Pointer(inner) => match inner.as_ref() {
                        CType::Function(ret, _, _, _) => ret.as_ref().clone(),
                        _ => panic!("call through non-function pointer: {callee_ty:?}"),
                    },
                    _ => panic!("call on non-function type: {callee_ty:?}"),
                }
            }
            Expr::Cast(type_name, _) => self.resolve_typename(type_name),
            Expr::PostUnary(_, e) => self.expr_type(ctx, e),
            Expr::Unary(UnaryOp::LogNot, _) => CType::Int(Signedness::Signed),
            Expr::Unary(UnaryOp::PreInc | UnaryOp::PreDec | UnaryOp::Neg | UnaryOp::BitNot, e) => {
                self.expr_type(ctx, e)
            }
            Expr::Sizeof(_) | Expr::Alignof(_) => CType::Long(Signedness::Unsigned),
            Expr::StringLit(_) => CType::Pointer(Box::new(CType::Char(Signedness::Signed))),
            Expr::WideStringLit(_) => CType::Pointer(Box::new(CType::Int(Signedness::Signed))),
            Expr::IntLit(_) | Expr::CharLit(_) => CType::Int(Signedness::Signed),
            Expr::UIntLit(_) => CType::Long(Signedness::Unsigned),
            Expr::FloatLit(_, is_f32) => if *is_f32 { CType::Float } else { CType::Double },
            Expr::Binary(op, l, r) => {
                match op {
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge
                    | BinOp::LogAnd | BinOp::LogOr => CType::Int(Signedness::Signed),
                    BinOp::Shl | BinOp::Shr => self.expr_type(ctx, l).promote(),
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
                    | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                        let lt = self.expr_type(ctx, l);
                        let rt = self.expr_type(ctx, r);
                        CType::common(&lt, &rt)
                    }
                }
            }
            Expr::Conditional(_, t, f) => {
                let tt = self.expr_type(ctx, t);
                let ft = self.expr_type(ctx, f);
                // Function types decay to function pointers in ternary context
                let l = if tt.is_function() { CType::Pointer(Box::new(tt)) } else { tt };
                let r = if ft.is_function() { CType::Pointer(Box::new(ft)) } else { ft };
                if let (CType::Pointer(ref li), CType::Pointer(_)) = (&l, &r) {
                    return if matches!(**li, CType::Void) { r } else { l };
                }
                if matches!(l, CType::Pointer(_)) { l }
                else if matches!(r, CType::Pointer(_)) { r }
                else { CType::common(&l, &r) }
            }
            Expr::Assign(_, lhs, _) => self.expr_type(ctx, lhs),
            Expr::Comma(_, b) => self.expr_type(ctx, b),
            Expr::CompoundLiteral(tn, items) => {
                let ty = self.resolve_typename(tn);
                if let CType::Array(elem, None) = ty {
                    CType::Array(elem, Some(items.len()))
                } else {
                    ty
                }
            }
            Expr::VaArg(_, type_name) => self.resolve_typename(type_name),
            Expr::StmtExpr(items) => {
                for item in items.iter().rev() {
                    if let BlockItem::Stmt(Statement::Expr(Some(e))) = item {
                        return self.expr_type(ctx, e);
                    }
                }
                CType::Void
            }
            Expr::Builtin(name, args) => match name.as_str() {
                "__builtin_offsetof" => CType::Long(Signedness::Unsigned),
                "__builtin_expect" => self.expr_type(ctx, &args[0]),
                "__builtin_constant_p" => CType::Int(Signedness::Signed),
                "__builtin_choose_expr" => {
                    let val = self.eval_const(&args[0]);
                    match val {
                        Some(v) if v != 0 => self.expr_type(ctx, &args[1]),
                        Some(0) | None => self.expr_type(ctx, &args[2]),
                        Some(_) => unreachable!("v != 0 handled above"),
                    }
                }
                "__builtin_unreachable" => CType::Void,
                "__builtin_va_start" | "__builtin_va_end" | "__builtin_va_copy" => CType::Void,
                "__builtin_va_arg" => CType::Long(Signedness::Signed),
                _ => panic!("expr_type: unknown builtin '{name}'"),
            },
        }
    }
}

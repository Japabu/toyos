use super::*;

impl Codegen {
    /// Returns the element stride for a pointer or array type, or None.
    fn elem_stride(ty: &CType) -> Option<i64> {
        match ty {
            CType::Pointer(inner) | CType::Array(inner, _) => Some(inner.size().max(1) as i64),
            _ => None,
        }
    }

    /// Returns the stride for pointer increment/decrement (sizeof pointee).
    /// Returns 1 for non-pointer types (ordinary integer inc/dec).
    fn pointer_stride(&mut self, ctx: &FuncCtx, expr: &Expr) -> i64 {
        let ty = self.expr_type(ctx, expr);
        Self::elem_stride(&ty).unwrap_or(1)
    }

    /// Extract function name from a call expression, seeing through
    /// comma expressions like `(side_effect(), func_name)(args...)`.
    fn extract_func_name(expr: &Expr) -> Option<String> {
        match expr {
            Expr::Ident(name) => Some(name.clone()),
            Expr::Comma(_, rhs) => Self::extract_func_name(rhs),
            _ => None,
        }
    }

    pub(crate) fn compile_expr(&mut self, ctx: &mut FuncCtx, expr: &Expr) -> TypedValue {
        let expr_name = match expr {
            Expr::IntLit(v) => format!("IntLit({v})"),
            Expr::UIntLit(v) => format!("UIntLit({v})"),
            Expr::FloatLit(v, _) => format!("FloatLit({v})"),
            Expr::CharLit(v) => format!("CharLit({v})"),
            Expr::StringLit(_) | Expr::WideStringLit(_) => "StringLit".into(),
            Expr::Ident(n) => format!("Ident({n})"),
            Expr::Binary(op, ..) => format!("Binary({op:?})"),
            Expr::Unary(op, ..) => format!("Unary({op:?})"),
            Expr::PostUnary(op, ..) => format!("PostUnary({op:?})"),
            Expr::Cast(..) => "Cast".into(),
            Expr::Sizeof(..) => "Sizeof".into(),
            Expr::Alignof(..) => "Alignof".into(),
            Expr::Conditional(..) => "Conditional".into(),
            Expr::Call(f, _) => {
                if let Expr::Ident(n) = f.as_ref() { format!("Call({n})") }
                else { "Call(indirect)".into() }
            }
            Expr::Member(_, f) => format!("Member(.{f})"),
            Expr::Arrow(_, f) => format!("Arrow(->{f})"),
            Expr::Index(..) => "Index".into(),
            Expr::Assign(op, ..) => format!("Assign({op:?})"),
            Expr::Comma(..) => "Comma".into(),
            Expr::CompoundLiteral(..) => "CompoundLiteral".into(),
            Expr::StmtExpr(..) => "StmtExpr".into(),
            Expr::VaArg(..) => "VaArg".into(),
            Expr::Builtin(n, _) => format!("Builtin({n})"),
        };
        verbose_enter!("compile_expr", "{}", expr_name);
        let result = stacker::maybe_grow(128 * 1024, 2 * 1024 * 1024, || {
            self.compile_expr_inner(ctx, expr)
        });
        verbose_leave!();
        result
    }

    fn compile_expr_inner(&mut self, ctx: &mut FuncCtx, expr: &Expr) -> TypedValue {
        match expr {
            Expr::IntLit(v) => TypedValue::signed(ctx.builder.ins().iconst(I64, *v as i64)),
            Expr::UIntLit(v) => TypedValue::unsigned(ctx.builder.ins().iconst(I64, *v as i64)),
            Expr::FloatLit(v, is_f32) => {
                TypedValue::signed(if *is_f32 {
                    ctx.builder.ins().f32const(*v as f32)
                } else {
                    ctx.builder.ins().f64const(*v)
                })
            }
            Expr::CharLit(v) => TypedValue::signed(ctx.builder.ins().iconst(I8, *v as i64)),
            e @ (Expr::StringLit(_) | Expr::WideStringLit(_)) => self.compile_string_lit(ctx, e),

            Expr::Ident(name) => self.compile_ident(ctx, name),

            Expr::Binary(op, lhs, rhs) => self.compile_binary(ctx, *op, lhs, rhs),

            Expr::Unary(op, e) => self.compile_unary(ctx, op, e),

            Expr::PostUnary(op, e) => self.compile_post_unary(ctx, op, e),

            Expr::Assign(op, lhs, rhs) => self.compile_assign(ctx, *op, lhs, rhs),

            Expr::Call(func, args) => self.compile_call(ctx, func, args),

            Expr::Cast(tn, e) => self.compile_cast(ctx, tn, e),

            Expr::Sizeof(arg) => {
                let size = match arg.as_ref() {
                    SizeofArg::Type(tn) => {
                        let ty = self.resolve_typename(tn);
                        ty.size()
                    }
                    SizeofArg::Expr(e) => {
                        self.expr_type(ctx, e).size()
                    }
                };
                TypedValue::unsigned(ctx.builder.ins().iconst(I64, size as i64))
            }

            Expr::Alignof(tn) => {
                let ty = self.resolve_typename(tn);
                TypedValue::unsigned(ctx.builder.ins().iconst(I64, ty.align() as i64))
            }

            Expr::Conditional(cond, then, else_) => self.compile_conditional(ctx, cond, then, else_),

            Expr::Comma(a, b) => {
                let _ = self.compile_expr(ctx, a);
                self.compile_expr(ctx, b)
            }

            Expr::Index(arr, idx) => self.compile_index(ctx, arr, idx),

            Expr::Member(e, field) => self.compile_member(ctx, e, field),
            Expr::Arrow(e, field) => self.compile_arrow(ctx, e, field),

            Expr::StmtExpr(items) => self.compile_stmt_expr(ctx, items),
            Expr::CompoundLiteral(tn, items) => self.compile_compound_literal(ctx, tn, items),
            Expr::VaArg(ap_expr, type_name) => self.compile_va_arg(ctx, ap_expr, type_name),

            Expr::Builtin(name, args) => self.compile_builtin(ctx, name, args),
        }
    }

    fn compile_string_lit(&mut self, ctx: &mut FuncCtx, e: &Expr) -> TypedValue {
        let sym = format!(".str.{}", self.string_counter);
        self.string_counter += 1;
        let data = super::init::string_lit_bytes(e);
        self.strings.push((sym.clone(), data));
        let data_id = self.module.declare_data(&sym, Linkage::Local, false, false).unwrap();
        let gv = self.module.declare_data_in_func(data_id, ctx.builder.func);
        TypedValue::unsigned(ctx.builder.ins().global_value(I64, gv))
    }

    fn compile_ident(&mut self, ctx: &mut FuncCtx, name: &str) -> TypedValue {
        if name == "__func__" || name == "__FUNCTION__" {
            let func_name = ctx.name.clone();
            let sym = format!(".str.{}", self.string_counter);
            self.string_counter += 1;
            let mut data: Vec<u8> = func_name.into_bytes();
            data.push(0);
            self.strings.push((sym.clone(), data));
            let data_id = self.module.declare_data(&sym, Linkage::Local, false, false).unwrap();
            let gv = self.module.declare_data_in_func(data_id, ctx.builder.func);
            return TypedValue::unsigned(ctx.builder.ins().global_value(I64, gv));
        }
        if let Some((var, ty)) = ctx.locals.get(name) {
            let (var, sign) = (*var, ty.signedness());
            return TypedValue::new(ctx.builder.use_var(var), sign);
        }
        if let Some((slot, ty)) = ctx.spilled_locals.get(name) {
            let (slot, sign) = (*slot, ty.signedness());
            let ptr = ctx.builder.ins().stack_addr(I64, slot, 0);
            let load_ty = self.clif_type(ty);
            return TypedValue::new(ctx.builder.ins().load(load_ty, MemFlags::new(), ptr, 0), sign);
        }
        if let Some((ptr, ty)) = ctx.local_ptrs.get(name) {
            let (ptr, sign) = (*ptr, ty.signedness());
            return match ty {
                CType::Struct(_) | CType::Union(_) | CType::Array(_, _) => TypedValue::unsigned(ptr),
                _ => {
                    let load_ty = self.clif_type(ty);
                    TypedValue::new(ctx.builder.ins().load(load_ty, MemFlags::new(), ptr, 0), sign)
                }
            };
        }
        if let Some(&val) = self.type_env.enum_constants.get(name) {
            return TypedValue::signed(ctx.builder.ins().iconst(I32, val));
        }
        if let Some(func_id) = self.func_ids.get(name) {
            let func_ref = self.module.declare_func_in_func(*func_id, ctx.builder.func);
            return TypedValue::unsigned(ctx.builder.ins().func_addr(I64, func_ref));
        }
        if let Some(data_id) = self.data_ids.get(name) {
            let gv = self.module.declare_data_in_func(*data_id, ctx.builder.func);
            let addr = ctx.builder.ins().global_value(I64, gv);
            if let Some(ty) = self.global_types.get(name) {
                if !matches!(ty, CType::Array(..) | CType::Struct(_) | CType::Union(_)) {
                    let (load_ty, sign) = (self.clif_type(ty), ty.signedness());
                    return TypedValue::new(ctx.builder.ins().load(load_ty, MemFlags::new(), addr, 0), sign);
                }
            }
            return TypedValue::unsigned(addr);
        }
        if let Ok(data_id) = self.module.declare_data(name, Linkage::Import, true, false) {
            self.data_ids.insert(name.to_string(), data_id);
            let gv = self.module.declare_data_in_func(data_id, ctx.builder.func);
            let addr = ctx.builder.ins().global_value(I64, gv);
            return TypedValue::signed(ctx.builder.ins().load(I64, MemFlags::new(), addr, 0));
        }
        panic!("unknown identifier '{name}'")
    }

    fn compile_unary(&mut self, ctx: &mut FuncCtx, op: &UnaryOp, e: &Expr) -> TypedValue {
        match op {
            UnaryOp::Neg => {
                let tv = self.compile_expr(ctx, e);
                let mut v = tv.raw();
                let vt = ctx.builder.func.dfg.value_type(v);
                if vt.is_float() {
                    TypedValue::signed(ctx.builder.ins().fneg(v))
                } else {
                    if vt.is_int() && vt.bits() < 32 {
                        v = self.coerce_typed(ctx, tv, I32);
                    }
                    TypedValue::signed(ctx.builder.ins().ineg(v))
                }
            }
            UnaryOp::BitNot => {
                let tv = self.compile_expr(ctx, e);
                let mut v = tv.raw();
                let vt = ctx.builder.func.dfg.value_type(v);
                if vt.is_int() && vt.bits() < 32 {
                    v = self.coerce_typed(ctx, tv, I32);
                }
                TypedValue::new(ctx.builder.ins().bnot(v), tv.signedness())
            }
            UnaryOp::LogNot => {
                let v = self.compile_expr(ctx, e).raw();
                let vt = ctx.builder.func.dfg.value_type(v);
                let zero = ctx.builder.ins().iconst(vt, 0);
                let is_zero = ctx.builder.ins().icmp(IntCC::Equal, v, zero);
                TypedValue::signed(Self::safe_uextend(ctx, vt, is_zero))
            }
            UnaryOp::Deref => {
                let ptr = self.compile_expr(ctx, e).raw();
                let deref_ty = match self.expr_type(ctx, e) {
                    CType::Pointer(inner) => *inner,
                    other => panic!("deref: non-pointer type {other:?}"),
                };
                if matches!(deref_ty, CType::Struct(_) | CType::Union(_) | CType::Array(..) | CType::Function(..)) {
                    return TypedValue::unsigned(ptr);
                }
                let load_ty = self.clif_type(&deref_ty);
                TypedValue::new(
                    ctx.builder.ins().load(load_ty, MemFlags::new(), ptr, 0),
                    deref_ty.signedness(),
                )
            }
            UnaryOp::AddrOf => {
                TypedValue::unsigned(self.compile_addr(ctx, e))
            }
            UnaryOp::PreInc | UnaryOp::PreDec => {
                let is_inc = matches!(op, UnaryOp::PreInc);
                let stride = self.pointer_stride(ctx, e);
                let ety = self.expr_type(ctx, e);
                let sign = ety.signedness();
                if let Expr::Ident(name) = e {
                    if let Some((var, _)) = ctx.locals.get(name.as_str()) {
                        let var = *var;
                        let val = ctx.builder.use_var(var);
                        let vt = ctx.builder.func.dfg.value_type(val);
                        let step = ctx.builder.ins().iconst(vt, stride);
                        let new_val = if is_inc { ctx.builder.ins().iadd(val, step) } else { ctx.builder.ins().isub(val, step) };
                        ctx.builder.def_var(var, new_val);
                        return TypedValue::new(new_val, sign);
                    }
                }
                let mem_ty = self.clif_type(&ety);
                let addr = self.compile_addr(ctx, e);
                let val = ctx.builder.ins().load(mem_ty, MemFlags::new(), addr, 0);
                let step = ctx.builder.ins().iconst(mem_ty, stride);
                let new_val = if is_inc { ctx.builder.ins().iadd(val, step) } else { ctx.builder.ins().isub(val, step) };
                ctx.builder.ins().store(MemFlags::new(), new_val, addr, 0);
                TypedValue::new(new_val, sign)
            }
        }
    }

    fn compile_post_unary(&mut self, ctx: &mut FuncCtx, op: &PostOp, e: &Expr) -> TypedValue {
        let stride = self.pointer_stride(ctx, e);
        let sign = self.expr_type(ctx, e).signedness();
        if let Expr::Ident(name) = e {
            if let Some((var, _)) = ctx.locals.get(name.as_str()) {
                let var = *var;
                let val = ctx.builder.use_var(var);
                let vt = ctx.builder.func.dfg.value_type(val);
                let step = ctx.builder.ins().iconst(vt, stride);
                let new_val = match op {
                    PostOp::PostInc => ctx.builder.ins().iadd(val, step),
                    PostOp::PostDec => ctx.builder.ins().isub(val, step),
                };
                ctx.builder.def_var(var, new_val);
                return TypedValue::new(val, sign);
            }
        }
        let ety = self.expr_type(ctx, e);
        let mem_ty = self.clif_type(&ety);
        let addr = self.compile_addr(ctx, e);
        let val = ctx.builder.ins().load(mem_ty, MemFlags::new(), addr, 0);
        let step = ctx.builder.ins().iconst(mem_ty, stride);
        let new_val = match op {
            PostOp::PostInc => ctx.builder.ins().iadd(val, step),
            PostOp::PostDec => ctx.builder.ins().isub(val, step),
        };
        ctx.builder.ins().store(MemFlags::new(), new_val, addr, 0);
        TypedValue::new(val, sign)
    }

    fn compile_cast(&mut self, ctx: &mut FuncCtx, tn: &TypeName, e: &Expr) -> TypedValue {
        let tv = self.compile_expr(ctx, e);
        let target_ty = self.resolve_typename(tn);
        let target_sign = target_ty.signedness();
        if matches!(target_ty, CType::Void) { return tv.with_sign(target_sign); }
        if matches!(target_ty, CType::Struct(_) | CType::Union(_)) { return tv.with_sign(target_sign); }
        let target_clif = self.clif_type(&target_ty);
        let val = tv.raw();
        let val_type = ctx.builder.func.dfg.value_type(val);
        // For float→unsigned int, use fcvt_to_uint to avoid trap on large values
        if val_type.is_float() && target_clif.is_int() && !target_ty.is_signed() {
            if target_clif.bits() < 32 {
                let wide = ctx.builder.ins().fcvt_to_uint(I32, val);
                return TypedValue::new(ctx.builder.ins().ireduce(target_clif, wide), target_sign);
            }
            return TypedValue::new(ctx.builder.ins().fcvt_to_uint(target_clif, val), target_sign);
        }
        TypedValue::new(self.coerce_typed(ctx, tv, target_clif), target_sign)
    }

    fn compile_conditional(&mut self, ctx: &mut FuncCtx, cond: &Expr, then: &Expr, else_: &Expr) -> TypedValue {
        let common_cty = {
            let tt = self.expr_type(ctx, then);
            let ft = self.expr_type(ctx, else_);
            CType::common(&tt, &ft)
        };
        let common_sign = common_cty.signedness();
        let merge_ty = Some(self.clif_type(&common_cty));

        let cond_val = self.compile_expr(ctx, cond).raw();
        let cond_bool = self.to_bool(ctx, cond_val);

        let then_block = ctx.builder.create_block();
        let else_block = ctx.builder.create_block();
        let merge = ctx.builder.create_block();

        ctx.builder.ins().brif(cond_bool, then_block, &[], else_block, &[]);

        ctx.builder.switch_to_block(then_block);
        ctx.builder.seal_block(then_block);
        let then_tv = self.compile_expr(ctx, then);
        let val_ty = merge_ty.unwrap_or_else(|| ctx.builder.func.dfg.value_type(then_tv.raw()));
        let then_val = self.coerce_typed(ctx, then_tv, val_ty);
        ctx.builder.ins().jump(merge, &[BlockArg::Value(then_val)]);

        ctx.builder.switch_to_block(else_block);
        ctx.builder.seal_block(else_block);
        let else_tv = self.compile_expr(ctx, else_);
        let else_val = self.coerce_typed(ctx, else_tv, val_ty);
        ctx.builder.ins().jump(merge, &[BlockArg::Value(else_val)]);

        ctx.builder.append_block_param(merge, val_ty);
        ctx.builder.switch_to_block(merge);
        ctx.builder.seal_block(merge);
        TypedValue::new(ctx.builder.block_params(merge)[0], common_sign)
    }

    fn compile_index(&mut self, ctx: &mut FuncCtx, arr: &Expr, idx: &Expr) -> TypedValue {
        let arr_ty = self.expr_type(ctx, arr);
        let elem_ty = match &arr_ty {
            CType::Pointer(inner) | CType::Array(inner, _) => inner.as_ref().clone(),
            other => panic!("index on non-pointer/array type {other:?}"),
        };
        let elem_size = elem_ty.size();
        let arr_val = self.compile_expr(ctx, arr).raw();
        let idx_raw = self.compile_expr(ctx, idx).raw();
        let idx_val = self.coerce(ctx, idx_raw, I64);
        let offset = ctx.builder.ins().imul_imm(idx_val, elem_size as i64);
        let addr = ctx.builder.ins().iadd(arr_val, offset);
        if matches!(&elem_ty, CType::Array(..) | CType::Struct(_) | CType::Union(_)) {
            return TypedValue::unsigned(addr);
        }
        let load_ty = self.clif_type(&elem_ty);
        TypedValue::new(
            ctx.builder.ins().load(load_ty, MemFlags::new(), addr, 0),
            elem_ty.signedness(),
        )
    }

    /// Shared field access: load a field value from a base address given the struct type.
    pub(super) fn compile_field_access(&mut self, ctx: &mut FuncCtx, base: Value, struct_ty: &CType, field: &str) -> TypedValue {
        let fi = struct_ty.field_offset(field)
            .unwrap_or_else(|| panic!("no field '{field}' in {struct_ty:?}"));
        if matches!(fi.ty, CType::Struct(_) | CType::Union(_) | CType::Array(..)) {
            if fi.byte_offset != 0 {
                return TypedValue::unsigned(ctx.builder.ins().iadd_imm(base, fi.byte_offset as i64));
            }
            return TypedValue::unsigned(base);
        }
        let sign = fi.ty.signedness();
        let load_ty = self.clif_type(&fi.ty);
        let val = ctx.builder.ins().load(load_ty, MemFlags::new(), base, fi.byte_offset as i32);
        TypedValue::new(self.extract_bitfield(ctx, val, fi.bit_offset, fi.bit_width, &fi.ty), sign)
    }

    /// Shared field address: compute the address of a field within a struct at base.
    pub(super) fn compile_field_addr(&mut self, ctx: &mut FuncCtx, base: Value, struct_ty: &CType, field: &str) -> Value {
        let fi = struct_ty.field_offset(field)
            .unwrap_or_else(|| panic!("compile_addr: no field '{field}' in {struct_ty:?}"));
        if fi.byte_offset != 0 {
            ctx.builder.ins().iadd_imm(base, fi.byte_offset as i64)
        } else {
            base
        }
    }

    fn compile_member(&mut self, ctx: &mut FuncCtx, e: &Expr, field: &str) -> TypedValue {
        let base = self.compile_addr(ctx, e);
        let base_ty = self.expr_type(ctx, e);
        let base_ty = self.resolve_incomplete_type(base_ty);
        self.compile_field_access(ctx, base, &base_ty, field)
    }

    fn compile_arrow(&mut self, ctx: &mut FuncCtx, e: &Expr, field: &str) -> TypedValue {
        let ptr = self.compile_expr(ctx, e).raw();
        let ptr_ty = self.expr_type(ctx, e);
        let pointee_ty = match ptr_ty {
            CType::Pointer(inner) => *inner,
            other => panic!("arrow '->{field}' on non-pointer type {other:?}"),
        };
        let pointee_ty = self.resolve_incomplete_type(pointee_ty);
        self.compile_field_access(ctx, ptr, &pointee_ty, field)
    }

    fn compile_stmt_expr(&mut self, ctx: &mut FuncCtx, items: &[BlockItem]) -> TypedValue {
        let mut last = TypedValue::signed(ctx.builder.ins().iconst(I64, 0));
        for item in items {
            if ctx.filled { self.ensure_unfilled(ctx); }
            match item {
                BlockItem::Decl(d) => self.compile_local_decl(ctx, d),
                BlockItem::Stmt(Statement::Expr(Some(e))) => { last = self.compile_expr(ctx, e); }
                BlockItem::Stmt(s) => self.compile_stmt(ctx, s),
            }
        }
        if ctx.filled { self.ensure_unfilled(ctx); }
        last
    }

    fn compile_compound_literal(&mut self, ctx: &mut FuncCtx, tn: &TypeName, items: &[InitializerItem]) -> TypedValue {
        let ty = self.resolve_typename(tn);
        let size = ty.size().max(1);
        let ss = ctx.builder.create_sized_stack_slot(ir::StackSlotData::new(
            ir::StackSlotKind::ExplicitSlot, size as u32, 0));
        let ptr = ctx.builder.ins().stack_addr(I64, ss, 0);
        let init = Initializer::List(items.to_vec());
        self.compile_aggregate_init(ctx, ptr, &ty, &init);
        if matches!(ty, CType::Struct(_) | CType::Union(_) | CType::Array(..)) {
            TypedValue::unsigned(ptr)
        } else {
            let load_ty = self.clif_type(&ty);
            TypedValue::new(
                ctx.builder.ins().load(load_ty, MemFlags::new(), ptr, 0),
                ty.signedness(),
            )
        }
    }

    fn compile_va_arg(&mut self, ctx: &mut FuncCtx, ap_expr: &Expr, type_name: &TypeName) -> TypedValue {
        let ap_val = self.compile_expr(ctx, ap_expr).raw();
        let ty = self.resolve_typename(type_name);
        let load_ty = self.clif_type(&ty);
        let result = ctx.builder.ins().load(load_ty, MemFlags::new(), ap_val, 0);
        let new_ap = ctx.builder.ins().iadd_imm(ap_val, 8);
        if let Expr::Ident(name) = ap_expr {
            if let Some((var, _)) = ctx.locals.get(name.as_str()) {
                let var = *var;
                ctx.builder.def_var(var, new_ap);
            } else if let Some((slot, _)) = ctx.spilled_locals.get(name.as_str()) {
                let ptr = ctx.builder.ins().stack_addr(I64, *slot, 0);
                ctx.builder.ins().store(MemFlags::new(), new_ap, ptr, 0);
            } else if let Some((ptr, _)) = ctx.local_ptrs.get(name.as_str()) {
                let ptr = *ptr;
                ctx.builder.ins().store(MemFlags::new(), new_ap, ptr, 0);
            }
        }
        TypedValue::new(result, ty.signedness())
    }

    fn compile_builtin(&mut self, ctx: &mut FuncCtx, name: &str, args: &[Expr]) -> TypedValue {
        match name {
            "__builtin_offsetof" => {
                let type_name = match &args[0] {
                    Expr::Ident(n) => n.clone(),
                    _ => panic!("__builtin_offsetof: expected type name"),
                };
                let field_name = match &args[1] {
                    Expr::Ident(n) => n.clone(),
                    _ => panic!("__builtin_offsetof: expected field name"),
                };
                let ty = self.type_env.typedefs.get(&type_name)
                    .or_else(|| self.type_env.tags.get(&type_name))
                    .unwrap_or_else(|| panic!("__builtin_offsetof: unknown type '{type_name}'"))
                    .clone();
                let ty = match &ty {
                    CType::Struct(def) if def.fields.is_empty() => {
                        def.name.as_ref()
                            .and_then(|n| self.type_env.tags.get(n))
                            .cloned()
                            .unwrap_or(ty)
                    }
                    CType::Union(def) if def.fields.is_empty() => {
                        def.name.as_ref()
                            .and_then(|n| self.type_env.tags.get(n))
                            .cloned()
                            .unwrap_or(ty)
                    }
                    _ => ty,
                };
                let fi = ty.field_offset(&field_name)
                    .unwrap_or_else(|| {
                        let field_names: Vec<_> = match &ty {
                            CType::Struct(def) => def.fields.iter().map(|f| f.name.clone().unwrap_or("<anon>".into())).collect(),
                            _ => vec!["<not a struct>".into()],
                        };
                        panic!("__builtin_offsetof: no field '{field_name}' in '{type_name}' (type has {} fields: {:?})", field_names.len(), &field_names[..field_names.len().min(10)])
                    });
                TypedValue::unsigned(ctx.builder.ins().iconst(I64, fi.byte_offset as i64))
            }
            "__builtin_expect" => self.compile_expr(ctx, &args[0]),
            "__builtin_constant_p" => {
                TypedValue::signed(ctx.builder.ins().iconst(I32, 0))
            }
            "__builtin_choose_expr" => {
                let val = crate::ast::eval_const_expr(&args[0], Some(&self.type_env.enum_constants))
                    .expect("__builtin_choose_expr: first argument must be a constant expression");
                if val != 0 { self.compile_expr(ctx, &args[1]) } else { self.compile_expr(ctx, &args[2]) }
            }
            "__builtin_types_compatible_p" | "__builtin_frame_address" | "__builtin_return_address" => {
                panic!("unsupported builtin: {name}");
            }
            "__builtin_unreachable" => {
                TypedValue::signed(self.emit_trap_with_value(ctx, I64))
            }
            "__builtin_va_end" => {
                TypedValue::signed(ctx.builder.ins().iconst(I64, 0))
            }
            "__builtin_va_start" => {
                let va_slot = ctx.va_area.unwrap_or_else(|| {
                    panic!("__builtin_va_start used in non-variadic function '{}'", ctx.name)
                });
                let va_addr = ctx.builder.ins().stack_addr(I64, va_slot, 0);
                if let Expr::Ident(ap_name) = &args[0] {
                    if let Some((var, _)) = ctx.locals.get(ap_name) {
                        let var = *var;
                        ctx.builder.def_var(var, va_addr);
                    } else if let Some((slot, _)) = ctx.spilled_locals.get(ap_name) {
                        let ptr = ctx.builder.ins().stack_addr(I64, *slot, 0);
                        ctx.builder.ins().store(MemFlags::new(), va_addr, ptr, 0);
                    } else if let Some((ptr, _)) = ctx.local_ptrs.get(ap_name) {
                        let ptr = *ptr;
                        ctx.builder.ins().store(MemFlags::new(), va_addr, ptr, 0);
                    }
                }
                TypedValue::unsigned(va_addr)
            }
            "__builtin_va_copy" => {
                let src_val = self.compile_expr(ctx, &args[1]).raw();
                if let Expr::Ident(dest_name) = &args[0] {
                    if let Some((var, _)) = ctx.locals.get(dest_name) {
                        let var = *var;
                        ctx.builder.def_var(var, src_val);
                    } else if let Some((slot, _)) = ctx.spilled_locals.get(dest_name) {
                        let ptr = ctx.builder.ins().stack_addr(I64, *slot, 0);
                        ctx.builder.ins().store(MemFlags::new(), src_val, ptr, 0);
                    } else if let Some((ptr, _)) = ctx.local_ptrs.get(dest_name) {
                        let ptr = *ptr;
                        ctx.builder.ins().store(MemFlags::new(), src_val, ptr, 0);
                    }
                }
                TypedValue::unsigned(src_val)
            }
            "__builtin_va_arg" => {
                let ap_val = self.compile_expr(ctx, &args[0]).raw();
                let result = ctx.builder.ins().load(I64, MemFlags::new(), ap_val, 0);
                let new_ap = ctx.builder.ins().iadd_imm(ap_val, 8);
                if let Expr::Ident(ap_name) = &args[0] {
                    if let Some((var, _)) = ctx.locals.get(ap_name) {
                        let var = *var;
                        ctx.builder.def_var(var, new_ap);
                    }
                }
                TypedValue::signed(result)
            }
            _ => panic!("builtin '{name}' not yet implemented"),
        }
    }

    fn compile_binary(&mut self, ctx: &mut FuncCtx, op: BinOp, lhs: &Expr, rhs: &Expr) -> TypedValue {
        // Pointer arithmetic: ptr + n => ptr + n*sizeof(*ptr)
        if matches!(op, BinOp::Add | BinOp::Sub) {
            let lty = self.expr_type(ctx, lhs);
            let rty = self.expr_type(ctx, rhs);
            let l_stride = Self::elem_stride(&lty);
            let r_stride = Self::elem_stride(&rty);

            if l_stride.is_some() && r_stride.is_none() {
                let stride = l_stride.unwrap();
                let l = self.compile_expr(ctx, lhs).raw();
                let r = self.compile_expr(ctx, rhs).raw();
                let r = self.coerce(ctx, r, I64);
                let r = if stride != 1 {
                    let s = ctx.builder.ins().iconst(I64, stride);
                    ctx.builder.ins().imul(r, s)
                } else { r };
                return self.compile_binop(ctx, op, TypedValue::signed(l), TypedValue::signed(r))
                    .with_sign(Signedness::Unsigned);
            }
            if r_stride.is_some() && l_stride.is_none() && matches!(op, BinOp::Add) {
                let stride = r_stride.unwrap();
                let l = self.compile_expr(ctx, lhs).raw();
                let r = self.compile_expr(ctx, rhs).raw();
                let l = self.coerce(ctx, l, I64);
                let l = if stride != 1 {
                    let s = ctx.builder.ins().iconst(I64, stride);
                    ctx.builder.ins().imul(l, s)
                } else { l };
                return self.compile_binop(ctx, op, TypedValue::signed(l), TypedValue::signed(r))
                    .with_sign(Signedness::Unsigned);
            }
            // ptr - ptr => (ptr - ptr) / sizeof(*ptr)
            if l_stride.is_some() && r_stride.is_some() && matches!(op, BinOp::Sub) {
                let stride = l_stride.unwrap();
                let l = self.compile_expr(ctx, lhs).raw();
                let r = self.compile_expr(ctx, rhs).raw();
                let diff = self.compile_binop(ctx, op, TypedValue::signed(l), TypedValue::signed(r)).raw();
                if stride != 1 {
                    let s = ctx.builder.ins().iconst(I64, stride);
                    return TypedValue::signed(ctx.builder.ins().sdiv(diff, s));
                }
                return TypedValue::signed(diff);
            }
        }
        // Short-circuit evaluation for && and ||
        if matches!(op, BinOp::LogAnd | BinOp::LogOr) {
            let l = self.compile_expr(ctx, lhs).raw();
            let l_bool = self.to_bool(ctx, l);

            let rhs_block = ctx.builder.create_block();
            let merge = ctx.builder.create_block();

            if op == BinOp::LogAnd {
                let false_val = ctx.builder.ins().iconst(I64, 0);
                ctx.builder.ins().brif(l_bool, rhs_block, &[], merge, &[BlockArg::Value(false_val)]);
            } else {
                let true_val = ctx.builder.ins().iconst(I64, 1);
                ctx.builder.ins().brif(l_bool, merge, &[BlockArg::Value(true_val)], rhs_block, &[]);
            }

            ctx.builder.switch_to_block(rhs_block);
            ctx.builder.seal_block(rhs_block);
            let r = self.compile_expr(ctx, rhs).raw();
            let r_bool = self.to_bool(ctx, r);
            let r_i64 = Self::safe_uextend(ctx, I64, r_bool);
            ctx.builder.ins().jump(merge, &[BlockArg::Value(r_i64)]);

            ctx.builder.append_block_param(merge, I64);
            ctx.builder.switch_to_block(merge);
            ctx.builder.seal_block(merge);
            return TypedValue::signed(ctx.builder.block_params(merge)[0]);
        }
        // C usual arithmetic conversions: determine operation signedness
        // Only unsigned when the common type is at least int-sized
        // (smaller unsigned types get promoted to signed int per C integer promotion)
        let lty = self.expr_type(ctx, lhs);
        let rty = self.expr_type(ctx, rhs);
        let is_unsigned =
            (lty.is_unsigned() && lty.size() >= 4) || (rty.is_unsigned() && rty.size() >= 4);
        let op_sign = if is_unsigned { Signedness::Unsigned } else { Signedness::Signed };
        let l_tv = self.compile_expr(ctx, lhs);
        let r_tv = self.compile_expr(ctx, rhs);
        // C integer promotion: promote narrow types to at least int (I32)
        // using TypedValue's signedness for correct extension
        let l = if ctx.builder.func.dfg.value_type(l_tv.raw()).is_int()
            && ctx.builder.func.dfg.value_type(l_tv.raw()).bits() < 32 {
            TypedValue::new(self.coerce_typed(ctx, l_tv, I32), op_sign)
        } else { l_tv.with_sign(op_sign) };
        let r = if ctx.builder.func.dfg.value_type(r_tv.raw()).is_int()
            && ctx.builder.func.dfg.value_type(r_tv.raw()).bits() < 32 {
            TypedValue::new(self.coerce_typed(ctx, r_tv, I32), op_sign)
        } else { r_tv.with_sign(op_sign) };
        // For unsigned ops, narrow operands to the correct C type width
        // so that e.g. (unsigned)-1 / -2 operates at 32-bit, not 64-bit
        let (l, r) = if is_unsigned {
            let common = CType::common(&lty, &rty);
            let w = self.clif_type(&common);
            (TypedValue::unsigned(self.coerce_unsigned(ctx, l.raw(), w)),
             TypedValue::unsigned(self.coerce_unsigned(ctx, r.raw(), w)))
        } else { (l, r) };
        self.compile_binop(ctx, op, l, r)
    }

    fn compile_assign(&mut self, ctx: &mut FuncCtx, op: AssignOp, lhs: &Expr, rhs: &Expr) -> TypedValue {
        let rhs_tv = self.compile_expr(ctx, rhs);
        let mut rhs_val = rhs_tv.raw();

        // Scale RHS for pointer += / -= by sizeof(pointee)
        if matches!(op, AssignOp::AddAssign | AssignOp::SubAssign) {
            let lty = self.expr_type(ctx, lhs);
            if let Some(stride) = Self::elem_stride(&lty) {
                if stride != 1 {
                    rhs_val = self.coerce(ctx, rhs_val, I64);
                    let s = ctx.builder.ins().iconst(I64, stride);
                    rhs_val = ctx.builder.ins().imul(rhs_val, s);
                }
            }
        }

        let lhs_sign = self.expr_type(ctx, lhs).signedness();

        // Direct variable assignment
        if let Expr::Ident(name) = lhs {
            if let Some((var, ty)) = ctx.locals.get(name) {
                let var = *var;
                let var_clif = self.clif_type(&ty);
                let val = if op == AssignOp::Assign {
                    rhs_val
                } else {
                    let lhs_val = ctx.builder.use_var(var);
                    self.compile_compound_assign(ctx, op,
                        TypedValue::new(lhs_val, lhs_sign),
                        TypedValue::new(rhs_val, rhs_tv.signedness()))
                };
                let val = self.coerce_typed(ctx, TypedValue::new(val, rhs_tv.signedness()), var_clif);
                ctx.builder.def_var(var, val);
                return TypedValue::new(val, lhs_sign);
            }
            // Spilled locals: store through stack slot
            if let Some((slot, ty)) = ctx.spilled_locals.get(name) {
                let slot = *slot;
                let var_clif = self.clif_type(&ty);
                let ptr = ctx.builder.ins().stack_addr(I64, slot, 0);
                let val = if op == AssignOp::Assign {
                    rhs_val
                } else {
                    let lhs_val = ctx.builder.ins().load(var_clif, MemFlags::new(), ptr, 0);
                    self.compile_compound_assign(ctx, op,
                        TypedValue::new(lhs_val, lhs_sign),
                        TypedValue::new(rhs_val, rhs_tv.signedness()))
                };
                let val = self.coerce_typed(ctx, TypedValue::new(val, rhs_tv.signedness()), var_clif);
                ctx.builder.ins().store(MemFlags::new(), val, ptr, 0);
                return TypedValue::new(val, lhs_sign);
            }
        }

        // Memory assignment — determine LHS type for correct store size
        let lhs_ty = self.expr_type(ctx, lhs);

        // Array/struct assignment: emit memcpy
        if op == AssignOp::Assign {
            if matches!(&lhs_ty, CType::Array(..) | CType::Struct(_) | CType::Union(_)) {
                let size = lhs_ty.size();
                let dst = self.compile_addr(ctx, lhs);
                let src = rhs_val; // for aggregates, compile_expr returns address
                let size_val = ctx.builder.ins().iconst(I64, size as i64);
                self.emit_memcpy(ctx, dst, src, size_val);
                return TypedValue::unsigned(dst);
            }
        }

        // Bitfield assignment: read-modify-write
        if let Some((bit_offset, bw, storage_ty)) = self.bitfield_info(ctx, lhs) {
            let addr = self.compile_addr(ctx, lhs);
            let val = if op == AssignOp::Assign {
                rhs_val
            } else {
                let store_clif = self.clif_type(&storage_ty);
                let old = ctx.builder.ins().load(store_clif, MemFlags::new(), addr, 0);
                let lhs_val = self.extract_bitfield(ctx, old, bit_offset, Some(bw), &storage_ty);
                self.compile_compound_assign(ctx, op,
                    TypedValue::new(lhs_val, lhs_sign),
                    TypedValue::new(rhs_val, rhs_tv.signedness()))
            };
            return TypedValue::new(
                self.store_bitfield(ctx, addr, val, bit_offset, bw, &storage_ty),
                lhs_sign,
            );
        }

        let addr = self.compile_addr(ctx, lhs);
        // Use actual field type for stores (not promoted type) to avoid
        // clobbering adjacent fields
        let store_ty = self.field_storage_type(ctx, lhs).unwrap_or(lhs_ty);
        let store_clif = self.clif_type(&store_ty);
        let val = if op == AssignOp::Assign {
            rhs_val
        } else {
            let lhs_val = ctx.builder.ins().load(store_clif, MemFlags::new(), addr, 0);
            self.compile_compound_assign(ctx, op,
                TypedValue::new(lhs_val, lhs_sign),
                TypedValue::new(rhs_val, rhs_tv.signedness()))
        };
        let val = self.coerce_typed(ctx, TypedValue::new(val, rhs_tv.signedness()), store_clif);
        ctx.builder.ins().store(MemFlags::new(), val, addr, 0);
        TypedValue::new(val, lhs_sign)
    }

    fn compile_call(&mut self, ctx: &mut FuncCtx, func: &Expr, args: &[Expr]) -> TypedValue {
        let arg_vals: Vec<Value> = args.iter().map(|a| self.compile_expr(ctx, a).raw()).collect();

        let func_name = Self::extract_func_name(func);

        // If the function expression has side effects (e.g. comma expr), compile them now.
        if func_name.is_some() && !matches!(func, Expr::Ident(_)) {
            let _ = self.compile_expr(ctx, func);
        }

        if let Some(ref name) = func_name {
            // Check if this is actually a variable (function pointer), not a function
            let is_var = ctx.locals.contains_key(name)
                || ctx.spilled_locals.contains_key(name)
                || ctx.local_ptrs.contains_key(name)
                || self.data_ids.contains_key(name);
            if is_var {
                return self.compile_indirect_call_var(ctx, func, &arg_vals);
            }

            // Detect struct return (uses sret convention)
            let is_struct_ret = self.func_ret_types.get(name)
                .map(|t| Self::needs_sret(t))
                .unwrap_or(false);

            let ret_cty = self.func_ret_types.get(name).cloned();

            // Use previously declared signature, or create an I64-based fallback.
            // Strip sret param from declared sig for user-arg matching.
            let declared_sig = self.func_sigs.get(name).cloned();
            let user_sig = if is_struct_ret {
                declared_sig.as_ref().map(|ds| {
                    let mut s = ds.clone();
                    if !s.params.is_empty() { s.params.remove(0); }
                    s
                })
            } else {
                declared_sig.clone()
            };

            // Build call signature matching actual arguments
            let mut call_sig = self.module.make_signature();
            if is_struct_ret {
                call_sig.params.push(AbiParam::new(I64));
            }
            for (i, &val) in arg_vals.iter().enumerate() {
                let val_ty = ctx.builder.func.dfg.value_type(val);
                if let Some(ref us) = user_sig {
                    if i < us.params.len() {
                        call_sig.params.push(AbiParam::new(us.params[i].value_type));
                    } else {
                        call_sig.params.push(AbiParam::new(val_ty));
                    }
                } else {
                    call_sig.params.push(AbiParam::new(I64));
                }
            }
            if !is_struct_ret {
                if let Some(ref ds) = declared_sig {
                    call_sig.returns = ds.returns.clone();
                } else {
                    call_sig.returns.push(AbiParam::new(I64));
                }
            }

            // Allocate sret temp if needed
            let sret_addr = if is_struct_ret {
                let ret_ty = self.func_ret_types.get(name).unwrap();
                let size = ret_ty.size().max(1);
                let ss = ctx.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot, size as u32, 0));
                Some(ctx.builder.ins().stack_addr(I64, ss, 0))
            } else { None };

            // Coerce arguments to match call signature
            let sret_offset = if is_struct_ret { 1 } else { 0 };
            let fixed_param_count = self.variadic_funcs.get(name).copied();
            let mut coerced_args = Vec::new();
            if let Some(addr) = sret_addr {
                coerced_args.push(addr);
            }
            for (i, &val) in arg_vals.iter().enumerate() {
                let val_ty = ctx.builder.func.dfg.value_type(val);
                let is_variadic_arg = fixed_param_count.is_some_and(|n| i >= n);
                if is_variadic_arg && val_ty.is_float() {
                    let f64_val = if val_ty == F32 {
                        ctx.builder.ins().fpromote(F64, val)
                    } else { val };
                    coerced_args.push(ctx.builder.ins().bitcast(I64, MemFlags::new(), f64_val));
                } else {
                    coerced_args.push(self.coerce(ctx, val, call_sig.params[i + sret_offset].value_type));
                }
            }

            // On aarch64, variadic args must go on the stack.
            if let Some(&fixed_count) = self.variadic_funcs.get(name) {
                let padding = self.variadic_padding(fixed_count);
                if padding > 0 {
                    let zero = ctx.builder.ins().iconst(I64, 0);
                    let insert_at = fixed_count + sret_offset;
                    for j in 0..padding {
                        coerced_args.insert(insert_at + j, zero);
                        call_sig.params.insert(insert_at + j, AbiParam::new(I64));
                    }
                }
            }

            // Look up existing func_id, or declare new
            let func_id = if let Some(&id) = self.func_ids.get(name) {
                id
            } else if let Some(FuncOrDataId::Func(id)) = self.module.get_name(name) {
                self.func_ids.insert(name.clone(), id);
                id
            } else {
                let id = self.module.declare_function(name, Linkage::Import, &call_sig).unwrap();
                self.func_ids.insert(name.clone(), id);
                self.func_sigs.insert(name.clone(), call_sig.clone());
                id
            };
            let func_ref = self.module.declare_func_in_func(func_id, ctx.builder.func);

            // If declared sig doesn't match call args, use indirect call
            let decl_sig = &self.module.declarations().get_function_decl(func_id).signature;
            let declared_param_count = decl_sig.params.len();
            let call = if declared_param_count != coerced_args.len() {
                let sig_ref = ctx.builder.import_signature(call_sig);
                let func_addr = ctx.builder.ins().func_addr(I64, func_ref);
                ctx.builder.ins().call_indirect(sig_ref, func_addr, &coerced_args)
            } else {
                let types_match = decl_sig.params.iter().zip(coerced_args.iter())
                    .all(|(p, &a)| p.value_type == ctx.builder.func.dfg.value_type(a));
                if !types_match {
                    for (i, param) in decl_sig.params.iter().enumerate() {
                        coerced_args[i] = self.coerce(ctx, coerced_args[i], param.value_type);
                    }
                }
                ctx.builder.ins().call(func_ref, &coerced_args)
            };
            if let Some(addr) = sret_addr {
                return TypedValue::unsigned(addr);
            }
            let results = ctx.builder.inst_results(call);
            if results.is_empty() {
                TypedValue::signed(ctx.builder.ins().iconst(I64, 0))
            } else {
                let sign = ret_cty.map(|t| t.signedness()).unwrap_or(Signedness::Signed);
                TypedValue::new(results[0], sign)
            }
        } else {
            // Indirect call (function pointer expression, not a named variable)
            self.compile_indirect_call_expr(ctx, func, &arg_vals)
        }
    }

    /// Indirect call through a named variable that holds a function pointer.
    fn compile_indirect_call_var(&mut self, ctx: &mut FuncCtx, func: &Expr, arg_vals: &[Value]) -> TypedValue {
        let func_ptr = if let Expr::Ident(name) = func {
            if ctx.local_ptrs.contains_key(name) {
                let addr = self.compile_expr(ctx, func).raw();
                ctx.builder.ins().load(I64, MemFlags::new(), addr, 0)
            } else {
                self.compile_expr(ctx, func).raw()
            }
        } else {
            self.compile_expr(ctx, func).raw()
        };
        self.compile_indirect_call_common(ctx, func, func_ptr, arg_vals)
    }

    /// Indirect call through a computed function pointer expression.
    fn compile_indirect_call_expr(&mut self, ctx: &mut FuncCtx, func: &Expr, arg_vals: &[Value]) -> TypedValue {
        let func_ptr = self.compile_expr(ctx, func).raw();
        self.compile_indirect_call_common(ctx, func, func_ptr, arg_vals)
    }

    /// Shared logic for indirect calls (both named-variable and expression-based).
    fn compile_indirect_call_common(
        &mut self, ctx: &mut FuncCtx, func: &Expr, func_ptr: Value, arg_vals: &[Value],
    ) -> TypedValue {
        let fptr_ty = self.expr_type(ctx, func);
        let (ret_cty, param_ctypes, is_variadic) = match &fptr_ty {
            CType::Pointer(inner) => match inner.as_ref() {
                CType::Function(ret, params, v) => (ret.as_ref().clone(), params.clone(), *v),
                other => panic!("indirect call: pointer to non-function type {other:?}"),
            },
            CType::Function(ret, params, v) => (ret.as_ref().clone(), params.clone(), *v),
            other => panic!("indirect call: expected function pointer, got {other:?}"),
        };
        let is_sret = Self::needs_sret(&ret_cty);
        let mut sig = self.module.make_signature();
        if is_sret {
            sig.params.push(AbiParam::new(I64));
        }
        for p in &param_ctypes {
            let clif_ty = if matches!(&p.ty, CType::Struct(_) | CType::Union(_)) {
                I64
            } else {
                self.clif_type(&p.ty)
            };
            sig.params.push(AbiParam::new(clif_ty));
        }
        // Add params for extra args: variadic or C's unspecified-param `()` syntax
        for _ in arg_vals.iter().skip(param_ctypes.len()) {
            sig.params.push(AbiParam::new(I64));
        }
        if !is_sret {
            if !matches!(&ret_cty, CType::Void) {
                let ret_clif = self.clif_type(&ret_cty);
                sig.returns.push(AbiParam::new(ret_clif));
            }
        }
        let sret_addr = if is_sret {
            let size = ret_cty.size().max(1);
            let ss = ctx.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot, size as u32, 0));
            Some(ctx.builder.ins().stack_addr(I64, ss, 0))
        } else { None };
        let sret_off = if is_sret { 1 } else { 0 };
        let fixed_count = param_ctypes.len();
        let mut coerced: Vec<Value> = Vec::new();
        if let Some(addr) = sret_addr {
            coerced.push(addr);
        }
        for (i, &val) in arg_vals.iter().enumerate() {
            if i < param_ctypes.len() {
                let target = if matches!(&param_ctypes[i].ty, CType::Struct(_) | CType::Union(_)) {
                    I64
                } else {
                    self.clif_type(&param_ctypes[i].ty)
                };
                coerced.push(self.coerce(ctx, val, target));
            } else {
                let val_ty = ctx.builder.func.dfg.value_type(val);
                if val_ty.is_float() {
                    let f64_val = if val_ty == F32 {
                        ctx.builder.ins().fpromote(F64, val)
                    } else { val };
                    coerced.push(ctx.builder.ins().bitcast(I64, MemFlags::new(), f64_val));
                } else {
                    coerced.push(self.coerce(ctx, val, I64));
                }
            }
        }
        if is_variadic {
            let padding = self.variadic_padding(fixed_count);
            if padding > 0 {
                let zero = ctx.builder.ins().iconst(I64, 0);
                let insert_at = fixed_count + sret_off;
                for j in 0..padding {
                    coerced.insert(insert_at + j, zero);
                    sig.params.insert(insert_at + j, AbiParam::new(I64));
                }
            }
        }
        let sig_ref = ctx.builder.import_signature(sig);
        let call = ctx.builder.ins().call_indirect(sig_ref, func_ptr, &coerced);
        if let Some(addr) = sret_addr {
            return TypedValue::unsigned(addr);
        }
        let results = ctx.builder.inst_results(call);
        let has_return = !matches!(&ret_cty, CType::Void);
        if results.is_empty() || !has_return {
            TypedValue::signed(ctx.builder.ins().iconst(I64, 0))
        } else {
            TypedValue::new(results[0], ret_cty.signedness())
        }
    }

    /// Emit a trap instruction followed by a dummy value in an unreachable block.
    /// Used for unimplemented builtins that need to return a value to satisfy the type system.
    fn emit_trap_with_value(&mut self, ctx: &mut FuncCtx, ty: ir::Type) -> Value {
        ctx.builder.ins().trap(ir::TrapCode::user(1).unwrap());
        // trap terminates the block — create a new unreachable block for the dummy value
        let dead_block = ctx.builder.create_block();
        ctx.builder.switch_to_block(dead_block);
        ctx.builder.seal_block(dead_block);
        ctx.builder.ins().iconst(ty, 0)
    }
}

use super::*;

impl Codegen {
    /// Returns the element stride for a pointer or array type, or None.
    fn elem_stride(ty: &Option<CType>) -> Option<i64> {
        match ty.as_ref()? {
            CType::Pointer(inner) | CType::Array(inner, _) => Some(inner.size().max(1) as i64),
            _ => None,
        }
    }

    /// Returns the stride for pointer increment/decrement (sizeof pointee).
    /// Returns 1 for non-pointer types (ordinary integer inc/dec).
    fn pointer_stride(&mut self, ctx: &FuncCtx, expr: &Expr) -> i64 {
        let ty = self.expr_type(ctx, expr);
        // Non-pointer types return None from elem_stride; stride 1 is correct for integer inc/dec
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

    /// Extract a bitfield value: shift right by bit_offset, mask to bit_width.
    /// For non-bitfield fields (bw == None), returns val unchanged.
    fn extract_bitfield(&self, ctx: &mut FuncCtx, val: Value, bit_offset: u32, bw: Option<u32>, field_ty: &CType) -> Value {
        let bw = match bw {
            Some(w) if w > 0 => w,
            _ => return val,
        };
        // Zero-extend storage unit to I64 first
        let val64 = self.coerce(ctx, val, I64);
        let shifted = if bit_offset > 0 {
            ctx.builder.ins().ushr_imm(val64, bit_offset as i64)
        } else {
            val64
        };
        let mask = if bw >= 64 { u64::MAX } else { (1u64 << bw) - 1 };
        let masked = ctx.builder.ins().band_imm(shifted, mask as i64);
        // Sign-extend if the underlying type is signed
        if field_ty.is_signed() && bw < 64 {
            let shift = 64 - bw;
            let shl = ctx.builder.ins().ishl_imm(masked, shift as i64);
            ctx.builder.ins().sshr_imm(shl, shift as i64)
        } else {
            masked
        }
    }

    /// Returns the actual (unpromoted) storage type for Member/Arrow field access.
    /// Unlike expr_type which applies integer promotion (correct for arithmetic),
    /// this returns the real field type so stores use the right width.
    fn field_storage_type(&mut self, ctx: &FuncCtx, expr: &Expr) -> Option<CType> {
        match expr {
            Expr::Member(e, field) => {
                let base_ty = self.expr_type(ctx, e)?;
                let base_ty = self.resolve_incomplete_type(base_ty);
                base_ty.field_offset(field).map(|(_, _, ty)| ty)
            }
            Expr::Arrow(e, field) => {
                let ptr_ty = self.expr_type(ctx, e)?;
                let pointee = match ptr_ty {
                    CType::Pointer(inner) => *inner,
                    _ => return None,
                };
                let pointee = self.resolve_incomplete_type(pointee);
                pointee.field_offset(field).map(|(_, _, ty)| ty)
            }
            _ => None,
        }
    }

    /// Check if an expression is a bitfield member access.
    /// Returns (bit_offset, bit_width, storage_type) if it is.
    fn bitfield_info(&mut self, ctx: &FuncCtx, expr: &Expr) -> Option<(u32, u32, CType)> {
        match expr {
            Expr::Member(e, field) => {
                let base_ty = self.expr_type(ctx, e)?;
                let base_ty = self.resolve_incomplete_type(base_ty);
                let (_, bit_offset, field_ty) = base_ty.field_offset(field)?;
                let bw = base_ty.field_bit_width(field)?;
                if bw > 0 { Some((bit_offset, bw, field_ty)) } else { None }
            }
            Expr::Arrow(e, field) => {
                let ptr_ty = self.expr_type(ctx, e)?;
                let pointee_ty = match ptr_ty {
                    CType::Pointer(inner) => *inner,
                    _ => return None,
                };
                let pointee_ty = self.resolve_incomplete_type(pointee_ty);
                let (_, bit_offset, field_ty) = pointee_ty.field_offset(field)?;
                let bw = pointee_ty.field_bit_width(field)?;
                if bw > 0 { Some((bit_offset, bw, field_ty)) } else { None }
            }
            _ => None,
        }
    }

    /// Store a value into a bitfield using read-modify-write.
    fn store_bitfield(&self, ctx: &mut FuncCtx, addr: Value, new_val: Value, bit_offset: u32, bw: u32, storage_ty: &CType) -> Value {
        let store_clif = self.clif_type(storage_ty);
        // Load the current storage unit
        let old = ctx.builder.ins().load(store_clif, MemFlags::new(), addr, 0);
        let old64 = self.coerce(ctx, old, I64);
        let new64 = self.coerce(ctx, new_val, I64);
        // Mask the new value to the bit width
        let field_mask = (1u64 << bw) - 1;
        let masked_new = ctx.builder.ins().band_imm(new64, field_mask as i64);
        // Shift into position
        let shifted_new = if bit_offset > 0 {
            ctx.builder.ins().ishl_imm(masked_new, bit_offset as i64)
        } else {
            masked_new
        };
        // Clear the field bits in the old value
        let clear_mask = !(field_mask << bit_offset) as i64;
        let cleared = ctx.builder.ins().band_imm(old64, clear_mask);
        // Merge
        let merged = ctx.builder.ins().bor(cleared, shifted_new);
        let merged = self.coerce(ctx, merged, store_clif);
        ctx.builder.ins().store(MemFlags::new(), merged, addr, 0);
        new_val
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
            e @ (Expr::StringLit(_) | Expr::WideStringLit(_)) => {
                let sym = format!(".str.{}", self.string_counter);
                self.string_counter += 1;
                let data = super::init::string_lit_bytes(e);
                self.strings.push((sym.clone(), data));

                let data_id = self.module.declare_data(&sym, Linkage::Local, false, false).unwrap();
                let gv = self.module.declare_data_in_func(data_id, ctx.builder.func);
                TypedValue::unsigned(ctx.builder.ins().global_value(I64, gv))
            }

            Expr::Ident(name) if name == "__func__" || name == "__FUNCTION__" => {
                let func_name = ctx.name.clone();
                let sym = format!(".str.{}", self.string_counter);
                self.string_counter += 1;
                let mut data: Vec<u8> = func_name.into_bytes();
                data.push(0);
                self.strings.push((sym.clone(), data));
                let data_id = self.module.declare_data(&sym, Linkage::Local, false, false).unwrap();
                let gv = self.module.declare_data_in_func(data_id, ctx.builder.func);
                TypedValue::unsigned(ctx.builder.ins().global_value(I64, gv))
            }
            Expr::Ident(name) => {
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
                    self.data_ids.insert(name.clone(), data_id);
                    let gv = self.module.declare_data_in_func(data_id, ctx.builder.func);
                    let addr = ctx.builder.ins().global_value(I64, gv);
                    return TypedValue::signed(ctx.builder.ins().load(I64, MemFlags::new(), addr, 0));
                }
                panic!("unknown identifier '{name}'")
            }

            Expr::Binary(op, lhs, rhs) => self.compile_binary(ctx, *op, lhs, rhs),

            Expr::Unary(op, e) => {
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
                        let deref_ty = self.expr_type(ctx, e).and_then(|ty| match ty {
                            CType::Pointer(inner) => Some(*inner),
                            _ => None,
                        });
                        if let Some(ref ty) = deref_ty {
                            if matches!(ty, CType::Struct(_) | CType::Union(_) | CType::Array(..) | CType::Function(..)) {
                                return TypedValue::unsigned(ptr);
                            }
                        }
                        let deref_ty = deref_ty.expect("deref: cannot resolve pointee type");
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
                        let sign = self.expr_type(ctx, e).map(|t| t.signedness()).unwrap_or(Signedness::Signed);
                        if let Expr::Ident(name) = e.as_ref() {
                            if let Some((var, _)) = ctx.locals.get(name) {
                                let var = *var;
                                let val = ctx.builder.use_var(var);
                                let vt = ctx.builder.func.dfg.value_type(val);
                                let step = ctx.builder.ins().iconst(vt, stride);
                                let new_val = if is_inc { ctx.builder.ins().iadd(val, step) } else { ctx.builder.ins().isub(val, step) };
                                ctx.builder.def_var(var, new_val);
                                return TypedValue::new(new_val, sign);
                            }
                        }
                        let mem_ty = self.expr_type(ctx, e).map(|ty| self.clif_type(&ty))
                            .expect("pre-inc/dec: cannot resolve type");
                        let addr = self.compile_addr(ctx, e);
                        let val = ctx.builder.ins().load(mem_ty, MemFlags::new(), addr, 0);
                        let step = ctx.builder.ins().iconst(mem_ty, stride);
                        let new_val = if is_inc { ctx.builder.ins().iadd(val, step) } else { ctx.builder.ins().isub(val, step) };
                        ctx.builder.ins().store(MemFlags::new(), new_val, addr, 0);
                        TypedValue::new(new_val, sign)
                    }
                }
            }

            Expr::PostUnary(op, e) => {
                let stride = self.pointer_stride(ctx, e);
                let sign = self.expr_type(ctx, e).map(|t| t.signedness()).unwrap_or(Signedness::Signed);
                if let Expr::Ident(name) = e.as_ref() {
                    if let Some((var, _)) = ctx.locals.get(name) {
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
                let mem_ty = self.expr_type(ctx, e).map(|ty| self.clif_type(&ty))
                    .expect("post-inc/dec: cannot resolve type");
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

            Expr::Assign(op, lhs, rhs) => self.compile_assign(ctx, *op, lhs, rhs),

            Expr::Call(func, args) => self.compile_call(ctx, func, args),

            Expr::Cast(tn, e) => {
                let tv = self.compile_expr(ctx, e);
                let target_ty = self.resolve_typename(tn);
                let target_sign = target_ty.signedness();
                if matches!(target_ty, CType::Void) { return tv.with_sign(target_sign); }
                // Struct/union cast: val is already an address, return as-is
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
                // Use source signedness for extension (sign-extend signed, zero-extend unsigned)
                TypedValue::new(self.coerce_typed(ctx, tv, target_clif), target_sign)
            }

            Expr::Sizeof(arg) => {
                let size = match arg.as_ref() {
                    SizeofArg::Type(tn) => {
                        let ty = self.resolve_typename(tn);
                        ty.size()
                    }
                    SizeofArg::Expr(e) => {
                        let ty = self.expr_type(ctx, e)
                            .unwrap_or_else(|| panic!("sizeof: cannot resolve type of expression"));
                        ty.size()
                    }
                };
                TypedValue::unsigned(ctx.builder.ins().iconst(I64, size as i64))
            }

            Expr::Alignof(tn) => {
                let ty = self.resolve_typename(tn);
                TypedValue::unsigned(ctx.builder.ins().iconst(I64, ty.align() as i64))
            }

            Expr::Conditional(cond, then, else_) => {
                // Determine common type for both branches (usual arithmetic conversions)
                let common_cty = {
                    let tt = self.expr_type(ctx, then);
                    let ft = self.expr_type(ctx, else_);
                    match (&tt, &ft) {
                        (Some(l), Some(r)) => Some(CType::common(l, r)),
                        (Some(t), None) | (None, Some(t)) => Some(t.clone()),
                        _ => None,
                    }
                };
                let common_sign = common_cty.as_ref().map(|t| t.signedness()).unwrap_or(Signedness::Signed);
                let merge_ty = common_cty.as_ref().map(|ty| self.clif_type(ty));

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

            Expr::Comma(a, b) => {
                let _ = self.compile_expr(ctx, a);
                self.compile_expr(ctx, b)
            }

            Expr::Index(arr, idx) => {
                let arr_ty = self.expr_type(ctx, arr)
                    .unwrap_or_else(|| panic!("index: cannot resolve array/pointer type"));
                let elem_size = match &arr_ty {
                    CType::Pointer(inner) | CType::Array(inner, _) => inner.size(),
                    other => panic!("index on non-pointer/array type {other:?}"),
                };
                let arr_val = self.compile_expr(ctx, arr).raw();
                let idx_raw = self.compile_expr(ctx, idx).raw();
                let idx_val = self.coerce(ctx, idx_raw, I64);
                let offset = ctx.builder.ins().imul_imm(idx_val, elem_size as i64);
                let addr = ctx.builder.ins().iadd(arr_val, offset);
                // If result type is array or struct/union, return address (decay to pointer)
                let result_ty = self.expr_type(ctx, expr)
                    .expect("index: cannot resolve result type");
                if matches!(&result_ty, CType::Array(..) | CType::Struct(_) | CType::Union(_)) {
                    return TypedValue::unsigned(addr);
                }
                let load_ty = self.clif_type(&result_ty);
                TypedValue::new(
                    ctx.builder.ins().load(load_ty, MemFlags::new(), addr, 0),
                    result_ty.signedness(),
                )
            }

            Expr::Member(e, field) => {
                let base = self.compile_addr(ctx, e);
                let base_ty = self.expr_type(ctx, e)
                    .unwrap_or_else(|| panic!("cannot resolve type of member base '.{field}'"));
                let base_ty = self.resolve_incomplete_type(base_ty);
                let (byte_offset, bit_offset, field_ty) = base_ty.field_offset(field)
                    .unwrap_or_else(|| panic!("no field '{field}' in {base_ty:?}"));
                let bw = base_ty.field_bit_width(field);
                // For aggregate fields, return the address
                if matches!(field_ty, CType::Struct(_) | CType::Union(_) | CType::Array(..)) {
                    if byte_offset != 0 {
                        return TypedValue::unsigned(ctx.builder.ins().iadd_imm(base, byte_offset as i64));
                    }
                    return TypedValue::unsigned(base);
                }
                let sign = field_ty.signedness();
                let load_ty = self.clif_type(&field_ty);
                let val = ctx.builder.ins().load(load_ty, MemFlags::new(), base, byte_offset as i32);
                TypedValue::new(self.extract_bitfield(ctx, val, bit_offset, bw, &field_ty), sign)
            }

            Expr::Arrow(e, field) => {
                let ptr = self.compile_expr(ctx, e).raw();
                let ptr_ty = self.expr_type(ctx, e)
                    .unwrap_or_else(|| panic!("cannot resolve type of arrow base '->{field}'"));
                let pointee_ty = match ptr_ty {
                    CType::Pointer(inner) => *inner,
                    other => panic!("arrow '->{field}' on non-pointer type {other:?}"),
                };
                let pointee_ty = self.resolve_incomplete_type(pointee_ty);
                let (byte_offset, bit_offset, field_ty) = pointee_ty.field_offset(field)
                    .unwrap_or_else(|| panic!("no field '{field}' in {pointee_ty:?}"));
                let bw = pointee_ty.field_bit_width(field);
                // For aggregate fields, return the address
                if matches!(field_ty, CType::Struct(_) | CType::Union(_) | CType::Array(..)) {
                    if byte_offset != 0 {
                        return TypedValue::unsigned(ctx.builder.ins().iadd_imm(ptr, byte_offset as i64));
                    }
                    return TypedValue::unsigned(ptr);
                }
                let sign = field_ty.signedness();
                let load_ty = self.clif_type(&field_ty);
                let val = ctx.builder.ins().load(load_ty, MemFlags::new(), ptr, byte_offset as i32);
                TypedValue::new(self.extract_bitfield(ctx, val, bit_offset, bw, &field_ty), sign)
            }

            Expr::StmtExpr(items) => {
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

            Expr::CompoundLiteral(tn, items) => {
                let ty = self.resolve_typename(tn);
                let size = ty.size().max(1);
                let ss = ctx.builder.create_sized_stack_slot(ir::StackSlotData::new(
                    ir::StackSlotKind::ExplicitSlot, size as u32, 0));
                let ptr = ctx.builder.ins().stack_addr(I64, ss, 0);
                let init = Initializer::List(items.clone());
                self.compile_aggregate_init(ctx, ptr, &ty, &init);
                // For scalars, load the value; for aggregates, return the pointer
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

            Expr::VaArg(ap_expr, type_name) => {
                // va_arg(ap, type): load value at *ap, advance ap by 8
                let ap_val = self.compile_expr(ctx, ap_expr).raw();
                let ty = self.resolve_typename(type_name);
                let load_ty = self.clif_type(&ty);
                let result = ctx.builder.ins().load(load_ty, MemFlags::new(), ap_val, 0);
                // Advance ap by 8 (each vararg slot is 8 bytes)
                let new_ap = ctx.builder.ins().iadd_imm(ap_val, 8);
                // Store new ap back — ap_expr must be an lvalue (usually an ident)
                if let Expr::Ident(name) = ap_expr.as_ref() {
                    if let Some((var, _)) = ctx.locals.get(name) {
                        let var = *var;
                        ctx.builder.def_var(var, new_ap);
                    } else if let Some((slot, _)) = ctx.spilled_locals.get(name) {
                        let ptr = ctx.builder.ins().stack_addr(I64, *slot, 0);
                        ctx.builder.ins().store(MemFlags::new(), new_ap, ptr, 0);
                    } else if let Some((ptr, _)) = ctx.local_ptrs.get(name) {
                        let ptr = *ptr;
                        ctx.builder.ins().store(MemFlags::new(), new_ap, ptr, 0);
                    }
                }
                TypedValue::new(result, ty.signedness())
            }

            Expr::Builtin(name, args) => {
                match name.as_str() {
                    "__builtin_offsetof" => {
                        // args[0] is the type (parsed as ident), args[1] is the field
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
                        // Resolve incomplete forward declarations via tag lookup
                        // Falls back to incomplete type if not yet defined (opaque types)
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
                        let (offset, _, _) = ty.field_offset(&field_name)
                            .unwrap_or_else(|| {
                                let field_names: Vec<_> = match &ty {
                                    CType::Struct(def) => def.fields.iter().map(|f| f.name.clone().unwrap_or("<anon>".into())).collect(),
                                    _ => vec!["<not a struct>".into()],
                                };
                                panic!("__builtin_offsetof: no field '{field_name}' in '{type_name}' (type has {} fields: {:?})", field_names.len(), &field_names[..field_names.len().min(10)])
                            });
                        TypedValue::unsigned(ctx.builder.ins().iconst(I64, offset as i64))
                    }
                    "__builtin_expect" => {
                        // __builtin_expect(expr, expected) — just return expr
                        self.compile_expr(ctx, &args[0])
                    }
                    "__builtin_constant_p" => {
                        // Conservative: always return 0 (not a compile-time constant).
                        // This is correct per GCC semantics — 0 means "don't optimize as constant".
                        TypedValue::signed(ctx.builder.ins().iconst(I32, 0))
                    }
                    "__builtin_choose_expr" => {
                        // __builtin_choose_expr(const_expr, expr1, expr2)
                        let val = crate::ast::eval_const_expr(&args[0], Some(&self.type_env.enum_constants))
                            .expect("__builtin_choose_expr: first argument must be a constant expression");
                        if val != 0 {
                            self.compile_expr(ctx, &args[1])
                        } else {
                            self.compile_expr(ctx, &args[2])
                        }
                    }
                    "__builtin_types_compatible_p" | "__builtin_frame_address" | "__builtin_return_address" => {
                        panic!("unsupported builtin: {name}");
                    }
                    "__builtin_unreachable" => {
                        TypedValue::signed(self.emit_trap_with_value(ctx, I64))
                    }
                    "__builtin_va_end" => {
                        // va_end is a no-op
                        TypedValue::signed(ctx.builder.ins().iconst(I64, 0))
                    }
                    "__builtin_va_start" => {
                        // va_start(ap, last_named): set ap to point to the va_area
                        let va_slot = ctx.va_area.unwrap_or_else(|| {
                            panic!("__builtin_va_start used in non-variadic function '{}'", ctx.name)
                        });
                        let va_addr = ctx.builder.ins().stack_addr(I64, va_slot, 0);
                        // Store va_area address into ap (first argument)
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
                        // va_copy(dest, src): copy the va_list pointer
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
                        // Same as Expr::VaArg but called as a builtin
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
                return TypedValue::unsigned(self.compile_binop(ctx, op, l, r, false));
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
                return TypedValue::unsigned(self.compile_binop(ctx, op, l, r, false));
            }
            // ptr - ptr => (ptr - ptr) / sizeof(*ptr)
            if l_stride.is_some() && r_stride.is_some() && matches!(op, BinOp::Sub) {
                let stride = l_stride.unwrap();
                let l = self.compile_expr(ctx, lhs).raw();
                let r = self.compile_expr(ctx, rhs).raw();
                let diff = self.compile_binop(ctx, op, l, r, false);
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
        // Determine if this is an unsigned operation (C usual arithmetic conversions)
        // Only treat as unsigned when the common type is at least int-sized
        // (smaller unsigned types get promoted to signed int per C integer promotion)
        let lty = self.expr_type(ctx, lhs);
        let rty = self.expr_type(ctx, rhs);
        let is_unsigned = match (&lty, &rty) {
            (Some(l), Some(r)) => {
                (l.is_unsigned() && l.size() >= 4) || (r.is_unsigned() && r.size() >= 4)
            }
            (Some(t), None) | (None, Some(t)) => t.is_unsigned() && t.size() >= 4,
            _ => false,
        };
        let l_tv = self.compile_expr(ctx, lhs);
        let r_tv = self.compile_expr(ctx, rhs);
        // C integer promotion: promote narrow types to at least int (I32)
        // using TypedValue's signedness for correct extension
        let mut l = l_tv.raw();
        let mut r = r_tv.raw();
        let lv_ty = ctx.builder.func.dfg.value_type(l);
        if lv_ty.is_int() && lv_ty.bits() < 32 {
            l = self.coerce_typed(ctx, l_tv, I32);
        }
        let rv_ty = ctx.builder.func.dfg.value_type(r);
        if rv_ty.is_int() && rv_ty.bits() < 32 {
            r = self.coerce_typed(ctx, r_tv, I32);
        }
        // For unsigned ops, narrow operands to the correct C type width
        // so that e.g. (unsigned)-1 / -2 operates at 32-bit, not 64-bit
        let (l, r) = if is_unsigned {
            if let (Some(lt), Some(rt)) = (&lty, &rty) {
                let common = CType::common(lt, rt);
                let w = self.clif_type(&common);
                (self.coerce_unsigned(ctx, l, w), self.coerce_unsigned(ctx, r, w))
            } else { (l, r) }
        } else { (l, r) };
        // Comparison operators always produce signed int
        let result_sign = match op {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => Signedness::Signed,
            _ => if is_unsigned { Signedness::Unsigned } else { Signedness::Signed },
        };
        TypedValue::new(self.compile_binop(ctx, op, l, r, is_unsigned), result_sign)
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

        let lhs_sign = self.expr_type(ctx, lhs).map(|t| t.signedness()).unwrap_or(Signedness::Signed);

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
            if let Some(ref ty) = lhs_ty {
                if matches!(ty, CType::Array(..) | CType::Struct(_) | CType::Union(_)) {
                    let size = ty.size();
                    let dst = self.compile_addr(ctx, lhs);
                    let src = rhs_val; // for aggregates, compile_expr returns address
                    let size_val = ctx.builder.ins().iconst(I64, size as i64);
                    self.emit_memcpy(ctx, dst, src, size_val);
                    return TypedValue::unsigned(dst);
                }
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
        let store_ty = self.field_storage_type(ctx, lhs).or(lhs_ty);
        let store_clif = store_ty.as_ref().map(|ty| self.clif_type(ty))
            .expect("assignment: cannot resolve lhs type");
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
        let fptr_ty = self.expr_type(ctx, func)
            .unwrap_or_else(|| panic!("indirect call: cannot resolve function pointer type"));
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
        if is_variadic {
            for _ in arg_vals.iter().skip(param_ctypes.len()) {
                sig.params.push(AbiParam::new(I64));
            }
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

    pub(crate) fn expr_type(&mut self, ctx: &FuncCtx, expr: &Expr) -> Option<CType> {
        verbose_enter!("expr_type", "{:?}", std::mem::discriminant(expr));
        let result = stacker::maybe_grow(128 * 1024, 2 * 1024 * 1024, || {
            self.expr_type_inner(ctx, expr)
        });
        verbose!("expr_type => {:?}", result);
        verbose_leave!();
        result
    }

    fn expr_type_inner(&mut self, ctx: &FuncCtx, expr: &Expr) -> Option<CType> {
        match expr {
            Expr::Ident(name) => {
                if let Some((_, ty)) = ctx.locals.get(name) {
                    return Some(ty.clone());
                }
                if let Some((_, ty)) = ctx.spilled_locals.get(name) {
                    return Some(ty.clone());
                }
                if let Some((_, ty)) = ctx.local_ptrs.get(name) {
                    return Some(ty.clone());
                }
                if let Some(ty) = self.global_types.get(name) {
                    return Some(ty.clone());
                }
                // Function names: return the Function type (decays to pointer in most contexts)
                if let Some(fty) = self.func_ctypes.get(name) {
                    return Some(fty.clone());
                }
                None
            }
            Expr::Arrow(e, field) => {
                let base_ty = self.expr_type(ctx, e)?;
                let pointee = match base_ty {
                    CType::Pointer(inner) => *inner,
                    CType::Array(inner, _) => *inner, // array decays to pointer
                    _ => return None,
                };
                let pointee = self.resolve_incomplete_type(pointee);
                let bw = pointee.field_bit_width(field);
                pointee.field_offset(field).map(|(_, _, ty)| CType::promote_integer(ty, bw))
            }
            Expr::Member(e, field) => {
                let base_ty = self.expr_type(ctx, e)?;
                let base_ty = self.resolve_incomplete_type(base_ty);
                let bw = base_ty.field_bit_width(field);
                base_ty.field_offset(field).map(|(_, _, ty)| CType::promote_integer(ty, bw))
            }
            Expr::Unary(UnaryOp::Deref, e) => {
                let ty = self.expr_type(ctx, e)?;
                match ty {
                    CType::Pointer(inner) | CType::Array(inner, _) => Some(*inner),
                    _ => None,
                }
            }
            Expr::Unary(UnaryOp::AddrOf, e) => {
                let ty = self.expr_type(ctx, e)?;
                Some(CType::Pointer(Box::new(ty)))
            }
            Expr::Index(arr, _) => {
                let ty = self.expr_type(ctx, arr)?;
                match ty {
                    CType::Pointer(inner) | CType::Array(inner, _) => Some(*inner),
                    _ => None,
                }
            }
            Expr::Call(func, _) => {
                if let Expr::Ident(name) = func.as_ref() {
                    return self.func_ret_types.get(name).cloned();
                }
                None
            }
            Expr::Cast(type_name, _) => {
                Some(self.resolve_typename(type_name))
            }
            Expr::PostUnary(_, e) => self.expr_type(ctx, e),
            Expr::Unary(UnaryOp::LogNot, _) => Some(CType::Int(Signedness::Signed)),
            Expr::Unary(UnaryOp::PreInc | UnaryOp::PreDec | UnaryOp::Neg | UnaryOp::BitNot, e) => {
                self.expr_type(ctx, e)
            }
            Expr::Sizeof(_) | Expr::Alignof(_) => Some(CType::Long(Signedness::Unsigned)),
            Expr::StringLit(_) => Some(CType::Pointer(Box::new(CType::Char(Signedness::Signed)))),
            Expr::WideStringLit(_) => Some(CType::Pointer(Box::new(CType::Int(Signedness::Signed)))),
            Expr::IntLit(_) | Expr::CharLit(_) => Some(CType::Int(Signedness::Signed)),
            Expr::UIntLit(_) => Some(CType::Long(Signedness::Unsigned)),
            Expr::FloatLit(_, is_f32) => Some(if *is_f32 { CType::Float } else { CType::Double }),
            Expr::Binary(op, l, r) => {
                match op {
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge
                    | BinOp::LogAnd | BinOp::LogOr => Some(CType::Int(Signedness::Signed)),
                    // Shifts: result type is promoted left operand (C99 6.5.7)
                    BinOp::Shl | BinOp::Shr => {
                        self.expr_type(ctx, l).map(|t| t.promote())
                    }
                    // Arithmetic: usual arithmetic conversions (C99 6.3.1.8)
                    _ => {
                        let lt = self.expr_type(ctx, l);
                        let rt = self.expr_type(ctx, r);
                        match (&lt, &rt) {
                            (Some(l), Some(r)) => Some(CType::common(l, r)),
                            (Some(t), None) | (None, Some(t)) => Some(t.clone()),
                            _ => None,
                        }
                    }
                }
            }
            Expr::Conditional(_, t, f) => {
                let tt = self.expr_type(ctx, t);
                let ft = self.expr_type(ctx, f);
                match (&tt, &ft) {
                    (Some(l), Some(r)) => {
                        // Function types decay to function pointers in ternary context
                        let l = match l {
                            CType::Function(..) => CType::Pointer(Box::new(l.clone())),
                            other => other.clone(),
                        };
                        let r = match r {
                            CType::Function(..) => CType::Pointer(Box::new(r.clone())),
                            other => other.clone(),
                        };
                        // Both pointers: prefer non-void (null pointer constant yields target type per C99 6.5.15)
                        if let (CType::Pointer(ref li), CType::Pointer(_)) = (&l, &r) {
                            return if matches!(**li, CType::Void) { Some(r) } else { Some(l) };
                        }
                        if matches!(l, CType::Pointer(_)) { Some(l) }
                        else if matches!(r, CType::Pointer(_)) { Some(r) }
                        else { Some(CType::common(&l, &r)) }
                    }
                    (Some(t), None) | (None, Some(t)) => Some(t.clone()),
                    _ => None,
                }
            }
            Expr::Assign(_, lhs, _) => self.expr_type(ctx, lhs),
            Expr::CompoundLiteral(tn, items) => {
                let ty = self.resolve_typename(tn);
                // For incomplete array types, determine size from initializer count
                if let CType::Array(elem, None) = ty {
                    Some(CType::Array(elem, Some(items.len())))
                } else {
                    Some(ty)
                }
            }
            _ => None,
        }
    }

    fn compile_addr(&mut self, ctx: &mut FuncCtx, expr: &Expr) -> Value {
        verbose_enter!("compile_addr", "{:?}", std::mem::discriminant(expr));
        let result = stacker::maybe_grow(128 * 1024, 2 * 1024 * 1024, || {
            self.compile_addr_inner(ctx, expr)
        });
        verbose_leave!();
        result
    }

    fn compile_addr_inner(&mut self, ctx: &mut FuncCtx, expr: &Expr) -> Value {
        match expr {
            Expr::Ident(name) => {
                // For stack-allocated locals, return their pointer
                if let Some((ptr, _)) = ctx.local_ptrs.get(name) {
                    return *ptr;
                }
                // For already-spilled locals, return a fresh stack_addr
                if let Some((slot, _)) = ctx.spilled_locals.get(name) {
                    return ctx.builder.ins().stack_addr(I64, *slot, 0);
                }
                // Spill SSA variable to stack permanently so aliases
                // through pointers (e.g. strstart(&r1)) see updates.
                if let Some((var, ty)) = ctx.locals.get(name).cloned() {
                    let size = ty.size().max(1);
                    let ss = ctx.builder.create_sized_stack_slot(ir::StackSlotData::new(
                        ir::StackSlotKind::ExplicitSlot, size as u32, 0,
                    ));
                    let ptr = ctx.builder.ins().stack_addr(I64, ss, 0);
                    let val = ctx.builder.use_var(var);
                    ctx.builder.ins().store(MemFlags::new(), val, ptr, 0);
                    ctx.locals.remove(name);
                    ctx.spilled_locals.insert(name.clone(), (ss, ty));
                    ptr
                } else if let Some(func_id) = self.func_ids.get(name) {
                    let func_ref = self.module.declare_func_in_func(*func_id, ctx.builder.func);
                    ctx.builder.ins().func_addr(I64, func_ref)
                } else if let Some(data_id) = self.data_ids.get(name) {
                    let gv = self.module.declare_data_in_func(*data_id, ctx.builder.func);
                    ctx.builder.ins().global_value(I64, gv)
                } else {
                    // Undeclared global — import it
                    let data_id = self.module.declare_data(name, Linkage::Import, true, false)
                        .unwrap_or_else(|e| panic!("unknown identifier '{name}': {e}"));
                    self.data_ids.insert(name.clone(), data_id);
                    let gv = self.module.declare_data_in_func(data_id, ctx.builder.func);
                    ctx.builder.ins().global_value(I64, gv)
                }
            }
            Expr::Unary(UnaryOp::Deref, e) => {
                self.compile_expr(ctx, e).raw() // *p address is just p
            }
            Expr::Index(arr, idx) => {
                let arr_ty = self.expr_type(ctx, arr)
                    .unwrap_or_else(|| panic!("compile_addr: cannot resolve type of indexed expression"));
                let elem_size = match &arr_ty {
                    CType::Pointer(inner) | CType::Array(inner, _) => inner.size(),
                    other => panic!("compile_addr: index on non-pointer/array type {other:?}"),
                };
                let arr_val = self.compile_expr(ctx, arr).raw();
                let idx_raw = self.compile_expr(ctx, idx).raw();
                let idx_val = self.coerce(ctx, idx_raw, I64);
                let offset = ctx.builder.ins().imul_imm(idx_val, elem_size as i64);
                ctx.builder.ins().iadd(arr_val, offset)
            }
            Expr::Member(e, field) => {
                let base = self.compile_addr(ctx, e);
                let base_ty = self.expr_type(ctx, e)
                    .unwrap_or_else(|| panic!("compile_addr: cannot resolve type of member base '.{field}'"));
                let base_ty = self.resolve_incomplete_type(base_ty);
                let (byte_offset, _, _) = base_ty.field_offset(field)
                    .unwrap_or_else(|| panic!("compile_addr: no field '{field}' in {base_ty:?}"));
                if byte_offset != 0 {
                    ctx.builder.ins().iadd_imm(base, byte_offset as i64)
                } else {
                    base
                }
            }
            Expr::Arrow(e, field) => {
                let ptr = self.compile_expr(ctx, e).raw();
                let ptr_ty = self.expr_type(ctx, e)
                    .unwrap_or_else(|| panic!("compile_addr: cannot resolve type of arrow base '->{field}'"));
                let pointee_ty = match ptr_ty {
                    CType::Pointer(inner) => *inner,
                    other => panic!("compile_addr: arrow '->{field}' on non-pointer type {other:?}"),
                };
                let pointee_ty = self.resolve_incomplete_type(pointee_ty);
                let (byte_offset, _, _) = pointee_ty.field_offset(field)
                    .unwrap_or_else(|| panic!("compile_addr: no field '{field}' in {pointee_ty:?}"));
                if byte_offset != 0 {
                    ctx.builder.ins().iadd_imm(ptr, byte_offset as i64)
                } else {
                    ptr
                }
            }
            Expr::Conditional(cond, then, else_) => {
                // For ternary with struct result, we need to produce an address.
                // Evaluate the condition, compile_addr each branch, select the address.
                let cond_val = self.compile_expr(ctx, cond).raw();
                let cond_bool = self.to_bool(ctx, cond_val);
                let then_block = ctx.builder.create_block();
                let else_block = ctx.builder.create_block();
                let merge = ctx.builder.create_block();
                ctx.builder.ins().brif(cond_bool, then_block, &[], else_block, &[]);
                ctx.builder.switch_to_block(then_block);
                ctx.builder.seal_block(then_block);
                let then_addr = self.compile_addr(ctx, then);
                ctx.builder.ins().jump(merge, &[BlockArg::Value(then_addr)]);
                ctx.builder.switch_to_block(else_block);
                ctx.builder.seal_block(else_block);
                let else_addr = self.compile_addr(ctx, else_);
                ctx.builder.ins().jump(merge, &[BlockArg::Value(else_addr)]);
                ctx.builder.append_block_param(merge, I64);
                ctx.builder.switch_to_block(merge);
                ctx.builder.seal_block(merge);
                return ctx.builder.block_params(merge)[0];
            }
            _ => {
                // For struct/union-typed expressions, compile_expr already returns an address
                let ty = self.expr_type(ctx, expr);
                if matches!(ty.as_ref(), Some(CType::Struct(_) | CType::Union(_))) {
                    return self.compile_expr(ctx, expr).raw();
                }
                // Expression doesn't have an address - create a temporary
                let val = self.compile_expr(ctx, expr).raw();
                let ss = ctx.builder.create_sized_stack_slot(ir::StackSlotData::new(
                    ir::StackSlotKind::ExplicitSlot, 8, 0,
                ));
                let ptr = ctx.builder.ins().stack_addr(I64, ss, 0);
                ctx.builder.ins().store(MemFlags::new(), val, ptr, 0);
                ptr
            }
        }
    }

    fn compile_binop(&mut self, ctx: &mut FuncCtx, op: BinOp, l: Value, r: Value, is_unsigned: bool) -> Value {
        // Coerce both operands to the wider type (C integer promotion)
        let lt = ctx.builder.func.dfg.value_type(l);
        let rt = ctx.builder.func.dfg.value_type(r);
        let is_float = lt.is_float() || rt.is_float();
        let common = if is_float {
            // Float promotion: if either is float, promote both
            if lt == F64 || rt == F64 { F64 } else { F32 }
        } else {
            // C integer promotion: result is at least int (I32)
            let wider = if lt.bits() >= rt.bits() { lt } else { rt };
            if wider.bits() < 32 { I32 } else { wider }
        };
        let l = if is_unsigned && !is_float { self.coerce_unsigned(ctx, l, common) } else { self.coerce(ctx, l, common) };
        let r = if is_unsigned && !is_float { self.coerce_unsigned(ctx, r, common) } else { self.coerce(ctx, r, common) };

        if is_float {
            // Comparisons return i32 (C int), not float
            let int_type = I32;
            match op {
                BinOp::Add => ctx.builder.ins().fadd(l, r),
                BinOp::Sub => ctx.builder.ins().fsub(l, r),
                BinOp::Mul => ctx.builder.ins().fmul(l, r),
                BinOp::Div => ctx.builder.ins().fdiv(l, r),
                BinOp::Eq => {
                    let c = ctx.builder.ins().fcmp(FloatCC::Equal, l, r);
                    Self::safe_uextend(ctx,int_type, c)
                }
                BinOp::Ne => {
                    let c = ctx.builder.ins().fcmp(FloatCC::NotEqual, l, r);
                    Self::safe_uextend(ctx,int_type, c)
                }
                BinOp::Lt => {
                    let c = ctx.builder.ins().fcmp(FloatCC::LessThan, l, r);
                    Self::safe_uextend(ctx,int_type, c)
                }
                BinOp::Gt => {
                    let c = ctx.builder.ins().fcmp(FloatCC::GreaterThan, l, r);
                    Self::safe_uextend(ctx,int_type, c)
                }
                BinOp::Le => {
                    let c = ctx.builder.ins().fcmp(FloatCC::LessThanOrEqual, l, r);
                    Self::safe_uextend(ctx,int_type, c)
                }
                BinOp::Ge => {
                    let c = ctx.builder.ins().fcmp(FloatCC::GreaterThanOrEqual, l, r);
                    Self::safe_uextend(ctx,int_type, c)
                }
                BinOp::LogAnd => {
                    let l_bool = self.to_bool(ctx, l);
                    let r_bool = self.to_bool(ctx, r);
                    let result = ctx.builder.ins().band(l_bool, r_bool);
                    Self::safe_uextend(ctx,int_type, result)
                }
                BinOp::LogOr => {
                    let l_bool = self.to_bool(ctx, l);
                    let r_bool = self.to_bool(ctx, r);
                    let result = ctx.builder.ins().bor(l_bool, r_bool);
                    Self::safe_uextend(ctx,int_type, result)
                }
                // Bitwise/shift ops don't apply to floats — treat as integer
                BinOp::Mod | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
                | BinOp::Shl | BinOp::Shr => {
                    let l = self.coerce(ctx, l, I64);
                    let r = self.coerce(ctx, r, I64);
                    match op {
                        BinOp::Mod => ctx.builder.ins().srem(l, r),
                        BinOp::BitAnd => ctx.builder.ins().band(l, r),
                        BinOp::BitOr => ctx.builder.ins().bor(l, r),
                        BinOp::BitXor => ctx.builder.ins().bxor(l, r),
                        BinOp::Shl => ctx.builder.ins().ishl(l, r),
                        BinOp::Shr => ctx.builder.ins().sshr(l, r),
                        _ => unreachable!(),
                    }
                }
            }
        } else {
            // Comparison results are at least I32 (C 'int')
            let cmp_result = if common.bits() >= 32 { common } else { I32 };
            match op {
                BinOp::Add => ctx.builder.ins().iadd(l, r),
                BinOp::Sub => ctx.builder.ins().isub(l, r),
                BinOp::Mul => ctx.builder.ins().imul(l, r),
                BinOp::Div => if is_unsigned { ctx.builder.ins().udiv(l, r) } else { ctx.builder.ins().sdiv(l, r) },
                BinOp::Mod => if is_unsigned { ctx.builder.ins().urem(l, r) } else { ctx.builder.ins().srem(l, r) },
                BinOp::BitAnd => ctx.builder.ins().band(l, r),
                BinOp::BitOr => ctx.builder.ins().bor(l, r),
                BinOp::BitXor => ctx.builder.ins().bxor(l, r),
                BinOp::Shl => ctx.builder.ins().ishl(l, r),
                BinOp::Shr => if is_unsigned { ctx.builder.ins().ushr(l, r) } else { ctx.builder.ins().sshr(l, r) },
                BinOp::Eq => {
                    let c = ctx.builder.ins().icmp(IntCC::Equal, l, r);
                    Self::safe_uextend(ctx,cmp_result, c)
                }
                BinOp::Ne => {
                    let c = ctx.builder.ins().icmp(IntCC::NotEqual, l, r);
                    Self::safe_uextend(ctx,cmp_result, c)
                }
                BinOp::Lt => {
                    let cc = if is_unsigned { IntCC::UnsignedLessThan } else { IntCC::SignedLessThan };
                    let c = ctx.builder.ins().icmp(cc, l, r);
                    Self::safe_uextend(ctx,cmp_result, c)
                }
                BinOp::Gt => {
                    let cc = if is_unsigned { IntCC::UnsignedGreaterThan } else { IntCC::SignedGreaterThan };
                    let c = ctx.builder.ins().icmp(cc, l, r);
                    Self::safe_uextend(ctx,cmp_result, c)
                }
                BinOp::Le => {
                    let cc = if is_unsigned { IntCC::UnsignedLessThanOrEqual } else { IntCC::SignedLessThanOrEqual };
                    let c = ctx.builder.ins().icmp(cc, l, r);
                    Self::safe_uextend(ctx,cmp_result, c)
                }
                BinOp::Ge => {
                    let cc = if is_unsigned { IntCC::UnsignedGreaterThanOrEqual } else { IntCC::SignedGreaterThanOrEqual };
                    let c = ctx.builder.ins().icmp(cc, l, r);
                    Self::safe_uextend(ctx,cmp_result, c)
                }
                BinOp::LogAnd => {
                    let l_bool = self.to_bool(ctx, l);
                    let r_bool = self.to_bool(ctx, r);
                    let result = ctx.builder.ins().band(l_bool, r_bool);
                    Self::safe_uextend(ctx,cmp_result, result)
                }
                BinOp::LogOr => {
                    let l_bool = self.to_bool(ctx, l);
                    let r_bool = self.to_bool(ctx, r);
                    let result = ctx.builder.ins().bor(l_bool, r_bool);
                    Self::safe_uextend(ctx,cmp_result, result)
                }
            }
        }
    }

    fn compile_compound_assign(&mut self, ctx: &mut FuncCtx, op: AssignOp, lhs: TypedValue, rhs: TypedValue) -> Value {
        let lt = ctx.builder.func.dfg.value_type(lhs.raw());
        let rt = ctx.builder.func.dfg.value_type(rhs.raw());
        let is_float = lt.is_float() || rt.is_float();
        let common = if is_float {
            if lt == F64 || rt == F64 { F64 } else { F32 }
        } else {
            if lt.bits() >= rt.bits() { lt } else { rt }
        };
        let l = self.coerce_typed(ctx, lhs, common);
        let r = self.coerce_typed(ctx, rhs, common);
        let is_unsigned = lhs.is_unsigned();

        if is_float {
            match op {
                AssignOp::AddAssign => ctx.builder.ins().fadd(l, r),
                AssignOp::SubAssign => ctx.builder.ins().fsub(l, r),
                AssignOp::MulAssign => ctx.builder.ins().fmul(l, r),
                AssignOp::DivAssign => ctx.builder.ins().fdiv(l, r),
                _ => {
                    let l = self.coerce(ctx, l, I64);
                    let r = self.coerce(ctx, r, I64);
                    match op {
                        AssignOp::ModAssign => ctx.builder.ins().srem(l, r),
                        AssignOp::ShlAssign => ctx.builder.ins().ishl(l, r),
                        AssignOp::ShrAssign => ctx.builder.ins().sshr(l, r),
                        AssignOp::AndAssign => ctx.builder.ins().band(l, r),
                        AssignOp::XorAssign => ctx.builder.ins().bxor(l, r),
                        AssignOp::OrAssign => ctx.builder.ins().bor(l, r),
                        _ => unreachable!(),
                    }
                }
            }
        } else {
            match op {
                AssignOp::AddAssign => ctx.builder.ins().iadd(l, r),
                AssignOp::SubAssign => ctx.builder.ins().isub(l, r),
                AssignOp::MulAssign => ctx.builder.ins().imul(l, r),
                AssignOp::DivAssign => if is_unsigned { ctx.builder.ins().udiv(l, r) } else { ctx.builder.ins().sdiv(l, r) },
                AssignOp::ModAssign => if is_unsigned { ctx.builder.ins().urem(l, r) } else { ctx.builder.ins().srem(l, r) },
                AssignOp::ShlAssign => ctx.builder.ins().ishl(l, r),
                AssignOp::ShrAssign => if is_unsigned { ctx.builder.ins().ushr(l, r) } else { ctx.builder.ins().sshr(l, r) },
                AssignOp::AndAssign => ctx.builder.ins().band(l, r),
                AssignOp::XorAssign => ctx.builder.ins().bxor(l, r),
                AssignOp::OrAssign => ctx.builder.ins().bor(l, r),
                AssignOp::Assign => unreachable!(),
            }
        }
    }

    pub(crate) fn to_bool(&self, ctx: &mut FuncCtx, val: Value) -> Value {
        let val_type = ctx.builder.func.dfg.value_type(val);
        if val_type.is_float() {
            let zero = if val_type == F32 {
                ctx.builder.ins().f32const(0.0)
            } else {
                ctx.builder.ins().f64const(0.0)
            };
            return ctx.builder.ins().fcmp(ir::condcodes::FloatCC::NotEqual, val, zero);
        }
        let zero = ctx.builder.ins().iconst(val_type, 0);
        ctx.builder.ins().icmp(IntCC::NotEqual, val, zero)
    }

    /// uextend that handles the no-op case where source and target types match
    fn safe_uextend(ctx: &mut FuncCtx, target: ir::Type, val: Value) -> Value {
        let val_type = ctx.builder.func.dfg.value_type(val);
        if val_type == target || val_type.bits() >= target.bits() { val }
        else { ctx.builder.ins().uextend(target, val) }
    }

    pub(crate) fn coerce(&self, ctx: &mut FuncCtx, val: Value, target: ir::Type) -> Value {
        let val_type = ctx.builder.func.dfg.value_type(val);
        if val_type == target { return val; }

        if val_type.is_int() && target.is_int() {
            if val_type.bits() < target.bits() {
                return ctx.builder.ins().sextend(target, val);
            } else if val_type.bits() > target.bits() {
                return ctx.builder.ins().ireduce(target, val);
            }
        }

        // Float conversions
        if val_type.is_float() && target.is_float() {
            if val_type.bits() < target.bits() {
                return ctx.builder.ins().fpromote(target, val);
            } else {
                return ctx.builder.ins().fdemote(target, val);
            }
        }
        if val_type.is_float() && target.is_int() {
            // fcvt_to_sint produces at least I32; reduce if needed
            if target.bits() < 32 {
                let wide = ctx.builder.ins().fcvt_to_sint(I32, val);
                return ctx.builder.ins().ireduce(target, wide);
            }
            return ctx.builder.ins().fcvt_to_sint(target, val);
        }
        if val_type.is_int() && target.is_float() {
            // fcvt_from_sint requires at least I32 input
            let widened = if val_type.bits() < 32 {
                ctx.builder.ins().sextend(I32, val)
            } else { val };
            return ctx.builder.ins().fcvt_from_sint(target, widened);
        }

        val
    }

    /// Like coerce, but uses zero-extension for integer widening (for unsigned types)
    pub(crate) fn coerce_unsigned(&self, ctx: &mut FuncCtx, val: Value, target: ir::Type) -> Value {
        let val_type = ctx.builder.func.dfg.value_type(val);
        if val_type == target { return val; }
        if val_type.is_int() && target.is_int() {
            if val_type.bits() < target.bits() {
                return ctx.builder.ins().uextend(target, val);
            } else if val_type.bits() > target.bits() {
                return ctx.builder.ins().ireduce(target, val);
            }
        }
        // Fall back to regular coerce for float conversions
        self.coerce(ctx, val, target)
    }

    /// Coerce a TypedValue to a target IR type, using its stored signedness
    /// to choose sign-extension vs zero-extension.
    pub(crate) fn coerce_typed(&self, ctx: &mut FuncCtx, tv: TypedValue, target: ir::Type) -> Value {
        match tv.signedness() {
            Signedness::Signed => self.coerce(ctx, tv.raw(), target),
            Signedness::Unsigned => self.coerce_unsigned(ctx, tv.raw(), target),
        }
    }
}

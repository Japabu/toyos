use super::*;

impl Codegen {
    pub(super) fn compile_binop(&mut self, ctx: &mut FuncCtx, op: BinOp, l: TypedValue, r: TypedValue) -> TypedValue {
        let is_unsigned = l.is_unsigned() || r.is_unsigned();
        // Coerce both operands to the wider type (C integer promotion)
        let lt = ctx.builder.func.dfg.value_type(l.raw());
        let rt = ctx.builder.func.dfg.value_type(r.raw());
        let is_float = lt.is_float() || rt.is_float();
        let common = if is_float {
            // Float promotion: if either is float, promote both
            if lt == F64 || rt == F64 { F64 } else { F32 }
        } else {
            // C integer promotion: result is at least int (I32)
            let wider = if lt.bits() >= rt.bits() { lt } else { rt };
            if wider.bits() < 32 { I32 } else { wider }
        };
        let l = if is_unsigned && !is_float { self.coerce_unsigned(ctx, l.raw(), common) } else { self.coerce(ctx, l.raw(), common) };
        let r = if is_unsigned && !is_float { self.coerce_unsigned(ctx, r.raw(), common) } else { self.coerce(ctx, r.raw(), common) };

        // Comparisons and logical ops always produce signed int
        let result_sign = match op {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge
            | BinOp::LogAnd | BinOp::LogOr => Signedness::Signed,
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
            | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr
                => if is_unsigned { Signedness::Unsigned } else { Signedness::Signed },
        };

        let val = if is_float {
            // Comparisons return i32 (C int), not float
            let int_type = I32;
            match op {
                BinOp::Add => ctx.builder.ins().fadd(l, r),
                BinOp::Sub => ctx.builder.ins().fsub(l, r),
                BinOp::Mul => ctx.builder.ins().fmul(l, r),
                BinOp::Div => ctx.builder.ins().fdiv(l, r),
                BinOp::Eq => {
                    let c = ctx.builder.ins().fcmp(FloatCC::Equal, l, r);
                    Self::safe_uextend(ctx, int_type, c)
                }
                BinOp::Ne => {
                    let c = ctx.builder.ins().fcmp(FloatCC::NotEqual, l, r);
                    Self::safe_uextend(ctx, int_type, c)
                }
                BinOp::Lt => {
                    let c = ctx.builder.ins().fcmp(FloatCC::LessThan, l, r);
                    Self::safe_uextend(ctx, int_type, c)
                }
                BinOp::Gt => {
                    let c = ctx.builder.ins().fcmp(FloatCC::GreaterThan, l, r);
                    Self::safe_uextend(ctx, int_type, c)
                }
                BinOp::Le => {
                    let c = ctx.builder.ins().fcmp(FloatCC::LessThanOrEqual, l, r);
                    Self::safe_uextend(ctx, int_type, c)
                }
                BinOp::Ge => {
                    let c = ctx.builder.ins().fcmp(FloatCC::GreaterThanOrEqual, l, r);
                    Self::safe_uextend(ctx, int_type, c)
                }
                BinOp::LogAnd => {
                    let l_bool = self.to_bool(ctx, l);
                    let r_bool = self.to_bool(ctx, r);
                    let result = ctx.builder.ins().band(l_bool, r_bool);
                    Self::safe_uextend(ctx, int_type, result)
                }
                BinOp::LogOr => {
                    let l_bool = self.to_bool(ctx, l);
                    let r_bool = self.to_bool(ctx, r);
                    let result = ctx.builder.ins().bor(l_bool, r_bool);
                    Self::safe_uextend(ctx, int_type, result)
                }
                // Bitwise/shift ops don't apply to floats — treat as integer
                BinOp::Mod | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
                | BinOp::Shl | BinOp::Shr => {
                    let l = self.coerce(ctx, l, I64);
                    let r = self.coerce(ctx, r, I64);
                    // outer match constrains op to exactly these 6 variants
                    match op {
                        BinOp::Mod => ctx.builder.ins().srem(l, r),
                        BinOp::BitAnd => ctx.builder.ins().band(l, r),
                        BinOp::BitOr => ctx.builder.ins().bor(l, r),
                        BinOp::BitXor => ctx.builder.ins().bxor(l, r),
                        BinOp::Shl => ctx.builder.ins().ishl(l, r),
                        BinOp::Shr => ctx.builder.ins().sshr(l, r),
                        _ => unreachable!("outer match constrains to Mod|BitAnd|BitOr|BitXor|Shl|Shr"),
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
                    Self::safe_uextend(ctx, cmp_result, c)
                }
                BinOp::Ne => {
                    let c = ctx.builder.ins().icmp(IntCC::NotEqual, l, r);
                    Self::safe_uextend(ctx, cmp_result, c)
                }
                BinOp::Lt => {
                    let cc = if is_unsigned { IntCC::UnsignedLessThan } else { IntCC::SignedLessThan };
                    let c = ctx.builder.ins().icmp(cc, l, r);
                    Self::safe_uextend(ctx, cmp_result, c)
                }
                BinOp::Gt => {
                    let cc = if is_unsigned { IntCC::UnsignedGreaterThan } else { IntCC::SignedGreaterThan };
                    let c = ctx.builder.ins().icmp(cc, l, r);
                    Self::safe_uextend(ctx, cmp_result, c)
                }
                BinOp::Le => {
                    let cc = if is_unsigned { IntCC::UnsignedLessThanOrEqual } else { IntCC::SignedLessThanOrEqual };
                    let c = ctx.builder.ins().icmp(cc, l, r);
                    Self::safe_uextend(ctx, cmp_result, c)
                }
                BinOp::Ge => {
                    let cc = if is_unsigned { IntCC::UnsignedGreaterThanOrEqual } else { IntCC::SignedGreaterThanOrEqual };
                    let c = ctx.builder.ins().icmp(cc, l, r);
                    Self::safe_uextend(ctx, cmp_result, c)
                }
                BinOp::LogAnd => {
                    let l_bool = self.to_bool(ctx, l);
                    let r_bool = self.to_bool(ctx, r);
                    let result = ctx.builder.ins().band(l_bool, r_bool);
                    Self::safe_uextend(ctx, cmp_result, result)
                }
                BinOp::LogOr => {
                    let l_bool = self.to_bool(ctx, l);
                    let r_bool = self.to_bool(ctx, r);
                    let result = ctx.builder.ins().bor(l_bool, r_bool);
                    Self::safe_uextend(ctx, cmp_result, result)
                }
            }
        };
        TypedValue::new(val, result_sign)
    }

    pub(super) fn compile_compound_assign(&mut self, ctx: &mut FuncCtx, op: AssignOp, lhs: TypedValue, rhs: TypedValue) -> Value {
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
                // Bitwise/shift ops don't apply to floats — coerce to int first
                AssignOp::ModAssign | AssignOp::ShlAssign | AssignOp::ShrAssign
                | AssignOp::AndAssign | AssignOp::XorAssign | AssignOp::OrAssign => {
                    let l = self.coerce(ctx, l, I64);
                    let r = self.coerce(ctx, r, I64);
                    // outer match constrains op to exactly these 6 variants
                    match op {
                        AssignOp::ModAssign => ctx.builder.ins().srem(l, r),
                        AssignOp::ShlAssign => ctx.builder.ins().ishl(l, r),
                        AssignOp::ShrAssign => ctx.builder.ins().sshr(l, r),
                        AssignOp::AndAssign => ctx.builder.ins().band(l, r),
                        AssignOp::XorAssign => ctx.builder.ins().bxor(l, r),
                        AssignOp::OrAssign => ctx.builder.ins().bor(l, r),
                        _ => unreachable!("outer match constrains to Mod|Shl|Shr|And|Xor|Or Assign"),
                    }
                }
                AssignOp::Assign => unreachable!(),
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
    pub(super) fn safe_uextend(ctx: &mut FuncCtx, target: ir::Type, val: Value) -> Value {
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

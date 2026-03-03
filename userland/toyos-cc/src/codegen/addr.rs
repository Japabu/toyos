use super::*;

impl Codegen {
    pub(super) fn compile_addr(&mut self, ctx: &mut FuncCtx, expr: &Expr) -> Value {
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
                let arr_ty = self.expr_type(ctx, arr);
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
                let base_ty = self.expr_type(ctx, e);
                let base_ty = self.resolve_incomplete_type(base_ty);
                self.compile_field_addr(ctx, base, &base_ty, field)
            }
            Expr::Arrow(e, field) => {
                let ptr = self.compile_expr(ctx, e).raw();
                let ptr_ty = self.expr_type(ctx, e);
                let pointee_ty = match ptr_ty {
                    CType::Pointer(inner) => *inner,
                    other => panic!("compile_addr: arrow '->{field}' on non-pointer type {other:?}"),
                };
                let pointee_ty = self.resolve_incomplete_type(pointee_ty);
                self.compile_field_addr(ctx, ptr, &pointee_ty, field)
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
                if matches!(ty, CType::Struct(_) | CType::Union(_)) {
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
}

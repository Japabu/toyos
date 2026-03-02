use super::*;

impl Codegen {
    /// Pre-resolve types in an expression to register enum constants as side effects.
    /// This is needed for `for(;;sizeof(enum{x=1}))` where the step expression's
    /// enum must be visible in the body (all parts of a for share the same scope).
    fn preresolve_expr_types(&mut self, expr: &Expr) {
        match expr {
            Expr::Sizeof(arg) => match arg.as_ref() {
                SizeofArg::Type(tn) => { self.resolve_typename(tn); }
                SizeofArg::Expr(e) => self.preresolve_expr_types(e),
            }
            Expr::Alignof(tn) => { self.resolve_typename(tn); }
            Expr::Cast(tn, e) => {
                self.resolve_typename(tn);
                self.preresolve_expr_types(e);
            }
            Expr::Binary(_, l, r) | Expr::Comma(l, r) => {
                self.preresolve_expr_types(l);
                self.preresolve_expr_types(r);
            }
            Expr::Unary(_, e) | Expr::PostUnary(_, e) => self.preresolve_expr_types(e),
            Expr::Conditional(c, t, f) => {
                self.preresolve_expr_types(c);
                self.preresolve_expr_types(t);
                self.preresolve_expr_types(f);
            }
            Expr::Call(f, args) => {
                self.preresolve_expr_types(f);
                for a in args { self.preresolve_expr_types(a); }
            }
            Expr::Member(e, _) | Expr::Arrow(e, _) => self.preresolve_expr_types(e),
            Expr::Index(a, i) => {
                self.preresolve_expr_types(a);
                self.preresolve_expr_types(i);
            }
            Expr::CompoundLiteral(tn, _) => { self.resolve_typename(tn); }
            _ => {}
        }
    }

    /// Collect variable names whose address is taken (&var) anywhere in the body.
    pub(crate) fn collect_addr_taken(stmt: &Statement, out: &mut HashSet<String>) {
        match stmt {
            Statement::Compound(items) => {
                for item in items {
                    match item {
                        BlockItem::Stmt(s) => Self::collect_addr_taken(s, out),
                        BlockItem::Decl(d) => {
                            for id in &d.declarators {
                                if let Some(Initializer::Expr(e)) = &id.initializer {
                                    Self::collect_addr_taken_expr(e, out);
                                }
                            }
                        }
                    }
                }
            }
            Statement::Expr(Some(e)) => Self::collect_addr_taken_expr(e, out),
            Statement::If(c, t, f) => {
                Self::collect_addr_taken_expr(c, out);
                Self::collect_addr_taken(t, out);
                if let Some(f) = f { Self::collect_addr_taken(f, out); }
            }
            Statement::While(c, b) => {
                Self::collect_addr_taken_expr(c, out);
                Self::collect_addr_taken(b, out);
            }
            Statement::DoWhile(b, c) => {
                Self::collect_addr_taken(b, out);
                Self::collect_addr_taken_expr(c, out);
            }
            Statement::For(init, cond, update, body) => {
                if let Some(init) = init {
                    match init.as_ref() {
                        ForInit::Expr(e) => Self::collect_addr_taken_expr(e, out),
                        ForInit::Decl(d) => {
                            for id in &d.declarators {
                                if let Some(Initializer::Expr(e)) = &id.initializer {
                                    Self::collect_addr_taken_expr(e, out);
                                }
                            }
                        }
                    }
                }
                if let Some(c) = cond { Self::collect_addr_taken_expr(c, out); }
                if let Some(u) = update { Self::collect_addr_taken_expr(u, out); }
                Self::collect_addr_taken(body, out);
            }
            Statement::Switch(e, b) => {
                Self::collect_addr_taken_expr(e, out);
                Self::collect_addr_taken(b, out);
            }
            Statement::Case(_, s) | Statement::Default(s) | Statement::Label(_, s) => {
                Self::collect_addr_taken(s, out);
            }
            Statement::Return(Some(e)) => Self::collect_addr_taken_expr(e, out),
            _ => {}
        }
    }

    fn collect_addr_taken_expr(expr: &Expr, out: &mut HashSet<String>) {
        match expr {
            Expr::Unary(UnaryOp::AddrOf, e) => {
                if let Expr::Ident(name) = e.as_ref() {
                    out.insert(name.clone());
                }
                Self::collect_addr_taken_expr(e, out);
            }
            Expr::Binary(_, l, r) | Expr::Assign(_, l, r) | Expr::Comma(l, r) => {
                Self::collect_addr_taken_expr(l, out);
                Self::collect_addr_taken_expr(r, out);
            }
            Expr::Unary(_, e) | Expr::PostUnary(_, e) | Expr::Cast(_, e) => {
                Self::collect_addr_taken_expr(e, out);
            }
            Expr::Call(f, args) => {
                Self::collect_addr_taken_expr(f, out);
                for a in args { Self::collect_addr_taken_expr(a, out); }
            }
            Expr::Conditional(c, t, f) => {
                Self::collect_addr_taken_expr(c, out);
                Self::collect_addr_taken_expr(t, out);
                Self::collect_addr_taken_expr(f, out);
            }
            Expr::Index(a, i) => {
                Self::collect_addr_taken_expr(a, out);
                Self::collect_addr_taken_expr(i, out);
            }
            Expr::Member(e, _) | Expr::Arrow(e, _) => {
                Self::collect_addr_taken_expr(e, out);
            }
            Expr::StmtExpr(items) => {
                for item in items {
                    match item {
                        BlockItem::Stmt(s) => Self::collect_addr_taken(s, out),
                        BlockItem::Decl(d) => {
                            for id in &d.declarators {
                                if let Some(Initializer::Expr(e)) = &id.initializer {
                                    Self::collect_addr_taken_expr(e, out);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) fn compile_stmt(&mut self, ctx: &mut FuncCtx, stmt: &Statement) {
        let stmt_name = match stmt {
            Statement::Compound(_) => "Compound",
            Statement::Expr(_) => "Expr",
            Statement::If(..) => "If",
            Statement::While(..) => "While",
            Statement::DoWhile(..) => "DoWhile",
            Statement::For(..) => "For",
            Statement::Switch(..) => "Switch",
            Statement::Case(..) => "Case",
            Statement::Default(..) => "Default",
            Statement::Break => "Break",
            Statement::Continue => "Continue",
            Statement::Return(_) => "Return",
            Statement::Goto(_) => "Goto",
            Statement::Label(l, _) => l.as_str(),
            Statement::CaseRange(..) => "CaseRange",
            Statement::Asm(_) => "Asm",
        };
        verbose_enter!("compile_stmt", "{}", stmt_name);
        stacker::maybe_grow(128 * 1024, 2 * 1024 * 1024, || {
            self.compile_stmt_inner(ctx, stmt);
        });
        verbose_leave!();
    }

    /// If the current block is filled (has a terminator), create a new unreachable block
    /// so that subsequent instructions don't panic.
    pub(crate) fn ensure_unfilled(&self, ctx: &mut FuncCtx) {
        if ctx.filled {
            let orphan = ctx.builder.create_block();
            ctx.builder.switch_to_block(orphan);
            ctx.builder.seal_block(orphan);
            ctx.filled = false;
        }
    }

    fn compile_stmt_inner(&mut self, ctx: &mut FuncCtx, stmt: &Statement) {
        if ctx.filled {
            // Current block is terminated. Skip dead code unless it's or contains a target.
            match stmt {
                Statement::Label(..) | Statement::Case(..) | Statement::CaseRange(..) | Statement::Default(..) | Statement::Compound(..) => {}
                // Inside a switch, if/while/for/do may contain case labels
                Statement::If(..) | Statement::While(..) | Statement::DoWhile(..) | Statement::For(..) if ctx.switch_val.is_some() => {}
                _ => return,
            }
        }

        match stmt {
            Statement::Compound(items) => {
                // Save scope: locals, local_ptrs, spilled_locals, func_ids, func_sigs, func_ret_types
                let saved_locals = ctx.locals.clone();
                let saved_local_ptrs = ctx.local_ptrs.clone();
                let saved_spilled = ctx.spilled_locals.clone();
                let saved_enums = self.type_env.enum_constants.clone();

                for item in items {
                    if ctx.filled {
                        // Skip dead code after terminator, unless it might contain a label target
                        // or is a declaration (variables need to exist for code after labels)
                        match item {
                            BlockItem::Stmt(Statement::Label(..) | Statement::Case(..) | Statement::CaseRange(..) | Statement::Default(..) | Statement::Compound(..)) => {
                                // fall through — labels/compounds can be targets
                            }
                            BlockItem::Stmt(Statement::If(..) | Statement::While(..) | Statement::DoWhile(..) | Statement::For(..)) if ctx.switch_val.is_some() => {
                                // Inside a switch, control structures may contain case labels
                            }
                            BlockItem::Decl(_) => {
                                // fall through — declarations need to exist for code after labels
                            }
                            _ => continue,
                        }
                    }
                    match item {
                        BlockItem::Decl(d) => self.compile_local_decl(ctx, d),
                        BlockItem::Stmt(s) => self.compile_stmt(ctx, s),
                    }
                }

                // Restore scope
                ctx.locals = saved_locals;
                ctx.local_ptrs = saved_local_ptrs;
                ctx.spilled_locals = saved_spilled;
                self.type_env.enum_constants = saved_enums;
            }
            Statement::Expr(Some(e)) => { self.compile_expr(ctx, e); }
            Statement::Expr(None) => {}
            Statement::Return(val) => {
                if let Some(e) = val {
                    if let Some(sret) = ctx.sret_ptr {
                        // Struct return: copy data to sret pointer
                        let src = self.compile_expr(ctx, e);
                        let size = ctx.return_type.size();
                        let size_val = ctx.builder.ins().iconst(I64, size as i64);
                        self.emit_memcpy(ctx, sret, src, size_val);
                        ctx.builder.ins().return_(&[]);
                    } else {
                        let v = self.compile_expr(ctx, e);
                        let ret_clif = if self.is_float_type(&ctx.return_type) {
                            self.clif_float_type(&ctx.return_type)
                        } else {
                            self.clif_type(&ctx.return_type)
                        };
                        let v = self.coerce(ctx, v, ret_clif);
                        ctx.builder.ins().return_(&[v]);
                    }
                } else {
                    ctx.builder.ins().return_(&[]);
                }
                ctx.filled = true;
            }
            Statement::If(cond, then, else_) => {
                self.ensure_unfilled(ctx);
                let saved_enums = self.type_env.enum_constants.clone();
                let cond_val = self.compile_expr(ctx, cond);
                let cond_bool = self.to_bool(ctx, cond_val);

                let then_block = ctx.builder.create_block();
                let else_block = ctx.builder.create_block();
                let merge = ctx.builder.create_block();

                ctx.builder.ins().brif(cond_bool, then_block, &[], else_block, &[]);

                ctx.builder.switch_to_block(then_block);
                ctx.builder.seal_block(then_block);
                ctx.filled = false;
                self.compile_stmt(ctx, then);
                let then_filled = ctx.filled;
                if !ctx.filled { ctx.builder.ins().jump(merge, &[]); }

                ctx.builder.switch_to_block(else_block);
                ctx.builder.seal_block(else_block);
                ctx.filled = false;
                if let Some(else_body) = else_ {
                    self.compile_stmt(ctx, else_body);
                }
                let else_filled = ctx.filled;
                if !ctx.filled { ctx.builder.ins().jump(merge, &[]); }

                ctx.builder.switch_to_block(merge);
                ctx.builder.seal_block(merge);
                ctx.filled = then_filled && else_filled;
                self.type_env.enum_constants = saved_enums;
            }
            Statement::While(cond, body) => {
                let saved_enums = self.type_env.enum_constants.clone();
                let cond_block = ctx.builder.create_block();
                let body_block = ctx.builder.create_block();
                let exit_block = ctx.builder.create_block();

                ctx.builder.ins().jump(cond_block, &[]);
                ctx.builder.switch_to_block(cond_block);
                ctx.filled = false;

                let cond_val = self.compile_expr(ctx, cond);
                let cond_bool = self.to_bool(ctx, cond_val);
                ctx.builder.ins().brif(cond_bool, body_block, &[], exit_block, &[]);

                let prev_break = ctx.break_block.replace(exit_block);
                let prev_continue = ctx.continue_block.replace(cond_block);

                ctx.builder.switch_to_block(body_block);
                ctx.builder.seal_block(body_block);
                ctx.filled = false;
                self.compile_stmt(ctx, body);
                if !ctx.filled { ctx.builder.ins().jump(cond_block, &[]); }

                ctx.break_block = prev_break;
                ctx.continue_block = prev_continue;

                ctx.builder.seal_block(cond_block);
                ctx.builder.switch_to_block(exit_block);
                ctx.builder.seal_block(exit_block);
                ctx.filled = false;
                self.type_env.enum_constants = saved_enums;
            }
            Statement::DoWhile(body, cond) => {
                let saved_enums = self.type_env.enum_constants.clone();
                let body_block = ctx.builder.create_block();
                let cond_block = ctx.builder.create_block();
                let exit_block = ctx.builder.create_block();

                ctx.builder.ins().jump(body_block, &[]);

                let prev_break = ctx.break_block.replace(exit_block);
                let prev_continue = ctx.continue_block.replace(cond_block);

                ctx.builder.switch_to_block(body_block);
                ctx.filled = false;
                self.compile_stmt(ctx, body);
                if !ctx.filled { ctx.builder.ins().jump(cond_block, &[]); }

                ctx.builder.switch_to_block(cond_block);
                ctx.builder.seal_block(cond_block);
                ctx.filled = false;
                let cond_val = self.compile_expr(ctx, cond);
                let cond_bool = self.to_bool(ctx, cond_val);
                ctx.builder.ins().brif(cond_bool, body_block, &[], exit_block, &[]);

                ctx.break_block = prev_break;
                ctx.continue_block = prev_continue;

                ctx.builder.seal_block(body_block);
                ctx.builder.switch_to_block(exit_block);
                ctx.builder.seal_block(exit_block);
                ctx.filled = false;
                self.type_env.enum_constants = saved_enums;
            }
            Statement::For(init, cond, step, body) => {
                let saved_enums = self.type_env.enum_constants.clone();
                if let Some(init) = init {
                    match init.as_ref() {
                        ForInit::Decl(d) => self.compile_local_decl(ctx, d),
                        ForInit::Expr(e) => { self.compile_expr(ctx, e); }
                    }
                }
                // Pre-resolve types in cond/step so enum definitions are
                // visible in the body (all for parts share the same scope).
                if let Some(cond) = cond { self.preresolve_expr_types(cond); }
                if let Some(step) = step { self.preresolve_expr_types(step); }

                let cond_block = ctx.builder.create_block();
                let body_block = ctx.builder.create_block();
                let step_block = ctx.builder.create_block();
                let exit_block = ctx.builder.create_block();

                ctx.builder.ins().jump(cond_block, &[]);
                ctx.builder.switch_to_block(cond_block);
                ctx.filled = false;

                if let Some(cond) = cond {
                    let cond_val = self.compile_expr(ctx, cond);
                    let cond_bool = self.to_bool(ctx, cond_val);
                    ctx.builder.ins().brif(cond_bool, body_block, &[], exit_block, &[]);
                } else {
                    ctx.builder.ins().jump(body_block, &[]);
                }

                let prev_break = ctx.break_block.replace(exit_block);
                let prev_continue = ctx.continue_block.replace(step_block);

                ctx.builder.switch_to_block(body_block);
                ctx.builder.seal_block(body_block);
                ctx.filled = false;
                self.compile_stmt(ctx, body);
                if !ctx.filled { ctx.builder.ins().jump(step_block, &[]); }

                ctx.builder.switch_to_block(step_block);
                ctx.builder.seal_block(step_block);
                ctx.filled = false;
                if let Some(step) = step {
                    self.compile_expr(ctx, step);
                }
                ctx.builder.ins().jump(cond_block, &[]);

                ctx.break_block = prev_break;
                ctx.continue_block = prev_continue;

                ctx.builder.seal_block(cond_block);
                ctx.builder.switch_to_block(exit_block);
                ctx.builder.seal_block(exit_block);
                ctx.filled = false;
                self.type_env.enum_constants = saved_enums;
            }
            Statement::Switch(val, body) => {
                let saved_enums = self.type_env.enum_constants.clone();
                let is_unsigned = self.expr_type(ctx, val)
                    .map(|t| t.is_unsigned())
                    .unwrap_or(false);
                let switch_val = self.compile_expr(ctx, val);
                let switch_type = ctx.builder.func.dfg.value_type(switch_val);
                let exit_block = ctx.builder.create_block();

                let prev_break = ctx.break_block.replace(exit_block);
                let prev_switch = ctx.switch_val.replace(switch_val);
                let prev_unsigned = ctx.switch_unsigned;
                ctx.switch_unsigned = is_unsigned;
                let prev_exit = ctx.switch_exit.replace(exit_block);
                let prev_fallthrough = ctx.switch_pending_fallthrough.take();
                let prev_entries = std::mem::take(&mut ctx.switch_dispatch_entries);
                let prev_ranges = std::mem::take(&mut ctx.switch_dispatch_ranges);
                let prev_default = ctx.switch_default_block.take();
                let prev_case_blocks = std::mem::take(&mut ctx.switch_case_blocks);

                // Jump to a placeholder dispatch block (will be filled after body)
                let dispatch_entry = ctx.builder.create_block();
                ctx.builder.ins().jump(dispatch_entry, &[]);

                // Switch to an unreachable block for the body — actual code
                // is reached via case dispatch blocks
                let body_block = ctx.builder.create_block();
                ctx.builder.switch_to_block(body_block);
                ctx.builder.seal_block(body_block);
                ctx.filled = true; // unreachable until a case label

                self.compile_stmt(ctx, body);
                if !ctx.filled { ctx.builder.ins().jump(exit_block, &[]); }

                // If last case had a fallthrough, connect it to exit
                if let Some(ft) = ctx.switch_pending_fallthrough.take() {
                    ctx.builder.switch_to_block(ft);
                    ctx.builder.ins().jump(exit_block, &[]);
                    ctx.builder.seal_block(ft);
                }

                // Build dispatch chain from collected entries
                let entries = std::mem::take(&mut ctx.switch_dispatch_entries);
                let ranges = std::mem::take(&mut ctx.switch_dispatch_ranges);
                let default_block = ctx.switch_default_block.take();

                let mut current_dispatch = dispatch_entry;
                for (case_val, case_block) in &entries {
                    let next_dispatch = ctx.builder.create_block();
                    ctx.builder.switch_to_block(current_dispatch);
                    let cv = ctx.builder.ins().iconst(switch_type, *case_val as i64);
                    let sv = switch_val;
                    let cmp = ctx.builder.ins().icmp(IntCC::Equal, sv, cv);
                    ctx.builder.ins().brif(cmp, *case_block, &[], next_dispatch, &[]);
                    ctx.builder.seal_block(current_dispatch);
                    current_dispatch = next_dispatch;
                }
                for (lo, hi, case_block) in &ranges {
                    let next_dispatch = ctx.builder.create_block();
                    ctx.builder.switch_to_block(current_dispatch);
                    let lo_v = ctx.builder.ins().iconst(switch_type, *lo as i64);
                    let hi_v = ctx.builder.ins().iconst(switch_type, *hi as i64);
                    let (ge_cc, le_cc) = if is_unsigned {
                        (IntCC::UnsignedGreaterThanOrEqual, IntCC::UnsignedLessThanOrEqual)
                    } else {
                        (IntCC::SignedGreaterThanOrEqual, IntCC::SignedLessThanOrEqual)
                    };
                    let ge = ctx.builder.ins().icmp(ge_cc, switch_val, lo_v);
                    let le = ctx.builder.ins().icmp(le_cc, switch_val, hi_v);
                    let in_range = ctx.builder.ins().band(ge, le);
                    ctx.builder.ins().brif(in_range, *case_block, &[], next_dispatch, &[]);
                    ctx.builder.seal_block(current_dispatch);
                    current_dispatch = next_dispatch;
                }
                // End of dispatch chain: jump to default or exit
                ctx.builder.switch_to_block(current_dispatch);
                ctx.builder.ins().jump(default_block.unwrap_or(exit_block), &[]);
                ctx.builder.seal_block(current_dispatch);

                // Seal all case/default blocks now that all predecessors are known
                let case_blocks = std::mem::take(&mut ctx.switch_case_blocks);
                for block in case_blocks {
                    ctx.builder.seal_block(block);
                }

                ctx.break_block = prev_break;
                ctx.switch_val = prev_switch;
                ctx.switch_unsigned = prev_unsigned;
                ctx.switch_exit = prev_exit;
                ctx.switch_pending_fallthrough = prev_fallthrough;
                ctx.switch_dispatch_entries = prev_entries;
                ctx.switch_dispatch_ranges = prev_ranges;
                ctx.switch_case_blocks = prev_case_blocks;
                ctx.switch_default_block = prev_default;

                ctx.builder.switch_to_block(exit_block);
                ctx.builder.seal_block(exit_block);
                ctx.filled = false;
                self.type_env.enum_constants = saved_enums;
            }
            Statement::Case(val, body) => {
                // Body block: either from previous case's fallthrough or new
                let case_block = ctx.switch_pending_fallthrough.take()
                    .unwrap_or_else(|| ctx.builder.create_block());

                // Collect dispatch entry (case value evaluated at compile time)
                let case_val = self.eval_const(val).unwrap_or(0);
                ctx.switch_dispatch_entries.push((case_val as i128, case_block));
                ctx.switch_case_blocks.push(case_block);

                // Connect fallthrough from previous body code to case_block
                if !ctx.filled {
                    ctx.builder.ins().jump(case_block, &[]);
                    ctx.filled = true;
                }

                // Don't seal case_block yet — dispatch chain will add predecessors
                ctx.builder.switch_to_block(case_block);
                ctx.filled = false;
                self.compile_stmt(ctx, body);

                if !ctx.filled {
                    let ft = ctx.builder.create_block();
                    ctx.builder.ins().jump(ft, &[]);
                    ctx.switch_pending_fallthrough = Some(ft);
                }
                ctx.filled = true;
            }
            Statement::CaseRange(lo, hi, body) => {
                let case_block = ctx.switch_pending_fallthrough.take()
                    .unwrap_or_else(|| ctx.builder.create_block());

                let lo_val = self.eval_const(lo).unwrap_or(0);
                let hi_val = self.eval_const(hi).unwrap_or(0);
                ctx.switch_dispatch_ranges.push((lo_val as i128, hi_val as i128, case_block));
                ctx.switch_case_blocks.push(case_block);

                if !ctx.filled {
                    ctx.builder.ins().jump(case_block, &[]);
                    ctx.filled = true;
                }

                ctx.builder.switch_to_block(case_block);
                ctx.filled = false;
                self.compile_stmt(ctx, body);

                if !ctx.filled {
                    let ft = ctx.builder.create_block();
                    ctx.builder.ins().jump(ft, &[]);
                    ctx.switch_pending_fallthrough = Some(ft);
                }
                ctx.filled = true;
            }
            Statement::Default(body) => {
                let default_block = ctx.switch_pending_fallthrough.take()
                    .unwrap_or_else(|| ctx.builder.create_block());

                ctx.switch_default_block = Some(default_block);
                ctx.switch_case_blocks.push(default_block);

                if !ctx.filled {
                    ctx.builder.ins().jump(default_block, &[]);
                    ctx.filled = true;
                }

                ctx.builder.switch_to_block(default_block);
                ctx.filled = false;
                self.compile_stmt(ctx, body);
            }
            Statement::Break => {
                if let Some(brk) = ctx.break_block {
                    ctx.builder.ins().jump(brk, &[]);
                    ctx.filled = true;
                }
            }
            Statement::Continue => {
                if let Some(cont) = ctx.continue_block {
                    ctx.builder.ins().jump(cont, &[]);
                    ctx.filled = true;
                }
            }
            Statement::Goto(label) => {
                let block = if let Some(&existing) = ctx.labels.get(label) {
                    existing
                } else {
                    let b = ctx.builder.create_block();
                    ctx.labels.insert(label.clone(), b);
                    b
                };
                ctx.builder.ins().jump(block, &[]);
                ctx.filled = true;
            }
            Statement::Label(label, body) => {
                let block = if let Some(existing) = ctx.labels.get(label) {
                    *existing
                } else {
                    let b = ctx.builder.create_block();
                    ctx.labels.insert(label.clone(), b);
                    b
                };

                if !ctx.filled {
                    ctx.builder.ins().jump(block, &[]);
                }
                ctx.builder.switch_to_block(block);
                ctx.filled = false;
                // Don't seal yet — might have forward gotos
                self.compile_stmt(ctx, body);
            }
            Statement::Asm(_) => {}
        }
    }

    pub(crate) fn compile_local_decl(&mut self, ctx: &mut FuncCtx, decl: &Declaration) {
        let is_typedef = decl.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Typedef)));

        // Always resolve the type — this registers struct/union/enum tags as a side effect
        let base_ty = self.resolve_type(&decl.specifiers);

        // Ensure we have a valid block for emitting instructions (dead code after goto/return)
        self.ensure_unfilled(ctx);

        if is_typedef {
            // Resolve the actual type and store it in type_env
            for id in &decl.declarators {
                if let Some(name) = self.get_declarator_name(&id.declarator) {
                    let ty = self.apply_declarator(&base_ty, &id.declarator);
                    self.type_env.typedefs.insert(name, ty);
                }
            }
            return;
        }

        let is_static = decl.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Static)));
        let is_extern = decl.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Extern)));

        for id in &decl.declarators {
            let name = self.get_declarator_name(&id.declarator).unwrap_or_default();
            if name.is_empty() { continue; }

            let mut ty = self.apply_declarator(&base_ty, &id.declarator);
            // Infer incomplete array size from initializer
            if let CType::Array(ref elem, None) = ty {
                if let Some(init) = &id.initializer {
                    let count = self.count_initializer_elements(init, elem);
                    ty = CType::Array(elem.clone(), Some(count));
                }
            }
            verbose!("local_decl: {} : {:?} (init={})", name, ty, id.initializer.is_some());

            // Local function declarations (e.g. `float fy();` inside a function body)
            // are extern function forward declarations, not local variables.
            if matches!(ty, CType::Function(..)) {
                // Remove any local variable shadow so calls see the function
                ctx.locals.remove(&name);
                ctx.local_ptrs.remove(&name);
                ctx.spilled_locals.remove(&name);
                if let CType::Function(ref ret, ref params, variadic) = ty {
                    self.func_ret_types.insert(name.clone(), ret.as_ref().clone());
                    if params.is_empty() {
                        // Unspecified params f() — don't lock in 0-param signature
                        continue;
                    }
                    let sig = self.build_signature(ret, params, variadic);
                    if variadic {
                        self.variadic_funcs.insert(name.clone(), params.len());
                    }
                    self.func_sigs.insert(name.clone(), sig.clone());
                    if let Ok(id) = self.module.declare_function(&name, Linkage::Import, &sig) {
                        self.func_ids.insert(name.clone(), id);
                    }
                }
                continue;
            }

            // extern declarations inside a function body refer to globals
            if is_extern && !matches!(ty, CType::Function(..)) {
                // Remove any local shadows so compile_addr finds the global
                ctx.locals.remove(&name);
                ctx.local_ptrs.remove(&name);
                ctx.spilled_locals.remove(&name);
                self.global_types.insert(name.clone(), ty.clone());
                if !self.data_ids.contains_key(&name) {
                    if let Ok(data_id) = self.module.declare_data(&name, Linkage::Import, false, false) {
                        self.data_ids.insert(name.clone(), data_id);
                    }
                }
                continue;
            }

            if is_static {
                // Static local — treat as global with mangled name to avoid namespace conflicts
                let sid = self.static_counter;
                self.static_counter += 1;
                let mangled = format!("{}.{}.{}", ctx.name, name, sid);
                let data_id = self.module.declare_data(&mangled, Linkage::Local, true, false).unwrap();
                let mut desc = DataDescription::new();
                desc.set_align(ty.align() as u64);
                if let Some(init) = &id.initializer {
                    let size = Self::init_size(&ty, init).max(1);
                    self.init_global_data(&mut desc, size, &ty, init);
                } else {
                    desc.define_zeroinit(ty.size().max(1));
                }
                self.module.define_data(data_id, &desc).unwrap_or_else(|e| panic!("failed to define data: {e:?}"));
                let gv = self.module.declare_data_in_func(data_id, ctx.builder.func);
                let ptr = ctx.builder.ins().global_value(I64, gv);
                ctx.local_ptrs.insert(name.clone(), (ptr, ty.clone()));
                continue;
            }

            // Aggregates (struct/union/array) get stack-allocated
            if matches!(ty, CType::Struct(_) | CType::Union(_) | CType::Array(..)) {
                let size = ty.size().max(1);
                let ss = ctx.builder.create_sized_stack_slot(ir::StackSlotData::new(ir::StackSlotKind::ExplicitSlot, size as u32, 0));
                let ptr = ctx.builder.ins().stack_addr(I64, ss, 0);
                ctx.local_ptrs.insert(name.clone(), (ptr, ty.clone()));

                if let Some(init) = &id.initializer {
                    self.compile_aggregate_init(ctx, ptr, &ty, init);
                }
                continue;
            }

            let clif_ty = if self.is_float_type(&ty) {
                self.clif_float_type(&ty)
            } else {
                self.clif_type(&ty)
            };

            // If address is taken (&var anywhere in function), allocate on
            // stack from the start so the slot is valid in all basic blocks.
            if ctx.addr_taken.contains(&name) {
                let size = ty.size().max(1);
                let ss = ctx.builder.create_sized_stack_slot(ir::StackSlotData::new(
                    ir::StackSlotKind::ExplicitSlot, size as u32, 0,
                ));
                let ptr = ctx.builder.ins().stack_addr(I64, ss, 0);
                let make_zero = |b: &mut FunctionBuilder| -> Value {
                    if clif_ty == F32 { b.ins().f32const(0.0) }
                    else if clif_ty == F64 { b.ins().f64const(0.0) }
                    else { b.ins().iconst(clif_ty, 0) }
                };
                if let Some(init) = &id.initializer {
                    if let Initializer::Expr(e) = init {
                        let val = self.compile_expr(ctx, e);
                        let val = self.coerce(ctx, val, clif_ty);
                        ctx.builder.ins().store(MemFlags::new(), val, ptr, 0);
                    } else {
                        let zero = make_zero(&mut ctx.builder);
                        ctx.builder.ins().store(MemFlags::new(), zero, ptr, 0);
                    }
                } else {
                    let zero = make_zero(&mut ctx.builder);
                    ctx.builder.ins().store(MemFlags::new(), zero, ptr, 0);
                }
                ctx.spilled_locals.insert(name, (ss, ty));
                continue;
            }

            let var = ctx.builder.declare_var(clif_ty);

            if let Some(init) = &id.initializer {
                if let Initializer::Expr(e) = init {
                    let val = self.compile_expr(ctx, e);
                    let val = self.coerce(ctx, val, clif_ty);
                    ctx.builder.def_var(var, val);
                } else {
                    // Zero-init for brace initializer on scalars
                    let zero = if clif_ty == F32 {
                        ctx.builder.ins().f32const(0.0)
                    } else if clif_ty == F64 {
                        ctx.builder.ins().f64const(0.0)
                    } else {
                        ctx.builder.ins().iconst(clif_ty, 0)
                    };
                    ctx.builder.def_var(var, zero);
                }
            } else {
                let zero = if clif_ty == F32 {
                    ctx.builder.ins().f32const(0.0)
                } else if clif_ty == F64 {
                    ctx.builder.ins().f64const(0.0)
                } else {
                    ctx.builder.ins().iconst(clif_ty, 0)
                };
                ctx.builder.def_var(var, zero);
            }

            ctx.locals.insert(name, (var, ty));
        }
    }
}

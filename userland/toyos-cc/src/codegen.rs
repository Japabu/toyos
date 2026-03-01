use std::collections::HashMap;

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::*;
use cranelift_codegen::ir::{self, AbiParam, BlockArg, InstBuilder, MemFlags, Value};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, FuncId, FuncOrDataId, Linkage, Module};
use cranelift_object::ObjectModule;

use crate::ast::*;
use crate::types::{CType, FieldDef, StructDef, TypeEnv, ParamType, EnumDef};

pub struct Codegen {
    pub module: ObjectModule,
    type_env: TypeEnv,
    strings: Vec<(String, Vec<u8>)>, // (symbol name, data)
    string_counter: usize,
    func_sigs: HashMap<String, ir::Signature>, // declared function signatures
    func_ids: HashMap<String, FuncId>,         // declared function IDs
}

struct FuncCtx<'a> {
    builder: FunctionBuilder<'a>,
    name: String,
    locals: HashMap<String, (Variable, CType)>,
    local_ptrs: HashMap<String, (Value, CType)>, // stack-allocated aggregates
    return_type: CType,
    filled: bool, // current block has a terminator
    // Control flow for break/continue
    break_block: Option<ir::Block>,
    continue_block: Option<ir::Block>,
    // Switch support
    switch_val: Option<Value>,
    switch_exit: Option<ir::Block>,
    // Goto/labels
    labels: HashMap<String, ir::Block>,
    gotos: Vec<(String, ir::Block)>, // deferred gotos
}

impl Codegen {
    pub fn new(module: ObjectModule, type_env: TypeEnv) -> Self {
        Self {
            module,
            type_env,
            strings: Vec::new(),
            string_counter: 0,
            func_sigs: HashMap::new(),
            func_ids: HashMap::new(),
        }
    }

    fn clif_type(&self, ty: &CType) -> ir::Type {
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

    fn is_float_type(&self, ty: &CType) -> bool {
        matches!(ty, CType::Float | CType::Double | CType::LongDouble)
    }

    fn clif_float_type(&self, ty: &CType) -> ir::Type {
        match ty {
            CType::Float => F32,
            CType::Double | CType::LongDouble => F64,
            _ => panic!("not a float type"),
        }
    }

    pub fn compile_unit(&mut self, tu: &TranslationUnit) {
        // First pass: declare all functions and globals
        for decl in tu {
            match decl {
                ExternalDecl::Function(fdef) => {
                    self.declare_function(fdef);
                }
                ExternalDecl::Declaration(d) => {
                    self.compile_global_decl(d);
                }
            }
        }

        // Second pass: define functions
        for decl in tu {
            if let ExternalDecl::Function(fdef) = decl {
                self.compile_function(fdef);
            }
        }

        // Define string constants
        self.define_strings();
    }

    fn resolve_type(&self, specifiers: &[DeclSpecifier]) -> CType {
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
                        if let Some(ty) = self.type_env.typedefs.get(name) {
                            base = Some(ty.clone());
                        } else {
                            base = Some(CType::Int(true)); // fallback
                        }
                    }
                    TypeSpec::Struct(st) => base = Some(self.resolve_struct(st, false)),
                    TypeSpec::Union(st) => base = Some(self.resolve_struct(st, true)),
                    TypeSpec::Enum(et) => base = Some(self.resolve_enum(et)),
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

    fn resolve_struct(&self, st: &StructType, is_union: bool) -> CType {
        let packed = st.attributes.iter().any(|a| a.name == "packed");
        let fields = st.fields.as_ref().map(|fs| {
            fs.iter().flat_map(|f| {
                let ty = self.resolve_type(&f.specifiers);
                f.declarators.iter().map(move |fd| {
                    let field_ty = if let Some(d) = &fd.declarator {
                        self.apply_declarator(&ty, d)
                    } else {
                        ty.clone()
                    };
                    let name = fd.declarator.as_ref().and_then(|d| self.get_declarator_name(d));
                    let bit_width = fd.bit_width.as_ref().map(|_| 0u32); // TODO: evaluate constant
                    FieldDef { name, ty: field_ty, bit_width }
                }).collect::<Vec<_>>()
            }).collect::<Vec<_>>()
        });

        let def = StructDef {
            name: st.name.clone(),
            fields: fields.unwrap_or_default(),
            packed,
        };

        if is_union { CType::Union(def) } else { CType::Struct(def) }
    }

    fn resolve_enum(&self, et: &EnumType) -> CType {
        let mut variants = Vec::new();
        let mut next_val = 0i64;
        if let Some(vs) = &et.variants {
            for v in vs {
                let val = if v.value.is_some() { next_val } else { next_val }; // TODO: eval const expr
                variants.push((v.name.clone(), val));
                next_val = val + 1;
            }
        }
        CType::Enum(EnumDef { name: et.name.clone(), variants })
    }

    fn apply_declarator(&self, base: &CType, d: &Declarator) -> CType {
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

    fn apply_direct_declarator(&self, base: &CType, dd: &DirectDeclarator) -> CType {
        match dd {
            DirectDeclarator::Ident(_) => base.clone(),
            DirectDeclarator::Paren(inner) => self.apply_declarator(base, inner),
            DirectDeclarator::Array(inner, size) => {
                let elem = self.apply_direct_declarator(base, inner);
                CType::Array(Box::new(elem), None) // TODO: evaluate size
            }
            DirectDeclarator::Function(inner, params) => {
                let ret = self.apply_direct_declarator(base, inner);
                let param_types: Vec<ParamType> = params.params.iter().map(|p| {
                    let ty = self.resolve_type(&p.specifiers);
                    let ty = if let Some(d) = &p.declarator {
                        self.apply_declarator(&ty, d)
                    } else {
                        ty
                    };
                    let name = p.declarator.as_ref().and_then(|d| self.get_declarator_name(d));
                    ParamType { name, ty }
                }).collect();
                CType::Function(Box::new(ret), param_types, params.variadic)
            }
        }
    }

    fn get_declarator_name(&self, d: &Declarator) -> Option<String> {
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

    fn declare_function(&mut self, fdef: &FunctionDef) {
        let name = self.get_declarator_name(&fdef.declarator).unwrap_or_default();
        if name.is_empty() { return; }

        let base_ty = self.resolve_type(&fdef.specifiers);
        let func_ty = self.apply_declarator(&base_ty, &fdef.declarator);

        let (ret_ty, param_types, variadic) = match &func_ty {
            CType::Function(ret, params, variadic) => (ret.as_ref(), params, *variadic),
            _ => (&base_ty, &Vec::new() as &Vec<ParamType>, false),
        };

        let is_static = fdef.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Static)));
        let linkage = if is_static { Linkage::Local } else { Linkage::Export };

        let mut sig = self.module.make_signature();
        for p in param_types {
            let clif_ty = if self.is_float_type(&p.ty) {
                self.clif_float_type(&p.ty)
            } else {
                self.clif_type(&p.ty)
            };
            sig.params.push(AbiParam::new(clif_ty));
        }
        if !matches!(ret_ty, CType::Void) {
            let clif_ty = if self.is_float_type(ret_ty) {
                self.clif_float_type(ret_ty)
            } else {
                self.clif_type(ret_ty)
            };
            sig.returns.push(AbiParam::new(clif_ty));
        }

        self.func_sigs.insert(name.clone(), sig.clone());
        if let Ok(id) = self.module.declare_function(&name, linkage, &sig) {
            self.func_ids.insert(name, id);
        }
    }

    fn compile_function(&mut self, fdef: &FunctionDef) {
        let name = self.get_declarator_name(&fdef.declarator).unwrap_or_default();
        if name.is_empty() { return; }

        let base_ty = self.resolve_type(&fdef.specifiers);
        let func_ty = self.apply_declarator(&base_ty, &fdef.declarator);

        let (ret_ty, param_types, _variadic) = match &func_ty {
            CType::Function(ret, params, variadic) => (ret.as_ref().clone(), params.clone(), *variadic),
            _ => (base_ty, Vec::new(), false),
        };

        let is_static = fdef.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Static)));
        let linkage = if is_static { Linkage::Local } else { Linkage::Export };

        let mut sig = self.module.make_signature();
        for p in &param_types {
            let clif_ty = if self.is_float_type(&p.ty) {
                self.clif_float_type(&p.ty)
            } else {
                self.clif_type(&p.ty)
            };
            sig.params.push(AbiParam::new(clif_ty));
        }
        if !matches!(ret_ty, CType::Void) {
            let clif_ty = if self.is_float_type(&ret_ty) {
                self.clif_float_type(&ret_ty)
            } else {
                self.clif_type(&ret_ty)
            };
            sig.returns.push(AbiParam::new(clif_ty));
        }

        let func_id = match self.module.declare_function(&name, linkage, &sig) {
            Ok(id) => id,
            Err(_) => {
                // Already declared with incompatible sig (e.g. from a call before definition).
                // Look up existing id and compile using the module's locked-in signature.
                if let Some(&id) = self.func_ids.get(&name) {
                    id
                } else if let Some(FuncOrDataId::Func(id)) = self.module.get_name(&name) {
                    id
                } else {
                    panic!("cannot declare function '{}'", name);
                }
            }
        };
        self.func_ids.insert(name.clone(), func_id);
        self.func_sigs.insert(name.clone(), sig);
        let module_sig = self.module.declarations().get_function_decl(func_id).signature.clone();
        let mut func = ir::Function::with_name_signature(
            ir::UserFuncName::user(0, func_id.as_u32()),
            module_sig,
        );

        let mut fb_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut fb_ctx);

        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        // Insert the entry block into the layout before sealing, so that
        // is_unreachable() recognizes it as the entry block.
        builder.ensure_inserted_block();
        builder.seal_block(entry);

        let mut ctx = FuncCtx {
            builder,
            name: name.clone(),
            locals: HashMap::new(),
            local_ptrs: HashMap::new(),
            return_type: ret_ty,
            filled: false,
            break_block: None,
            continue_block: None,
            switch_val: None,
            switch_exit: None,
            labels: HashMap::new(),
            gotos: Vec::new(),
        };

        // Bind parameters
        let params_block = entry;
        for (i, p) in param_types.iter().enumerate() {
            if let Some(name) = &p.name {
                let val = ctx.builder.block_params(params_block)[i];
                let clif_ty = if self.is_float_type(&p.ty) {
                    self.clif_float_type(&p.ty)
                } else {
                    self.clif_type(&p.ty)
                };
                let var = ctx.builder.declare_var(clif_ty);
                ctx.builder.def_var(var, val);
                ctx.locals.insert(name.clone(), (var, p.ty.clone()));
            }
        }

        // Compile body
        self.compile_stmt(&mut ctx, &fdef.body);

        // If the function doesn't end with a return, add one
        if !ctx.filled {
            if matches!(ctx.return_type, CType::Void) {
                ctx.builder.ins().return_(&[]);
            } else {
                let clif_ty = if self.is_float_type(&ctx.return_type) {
                    self.clif_float_type(&ctx.return_type)
                } else {
                    self.clif_type(&ctx.return_type)
                };
                let zero = if clif_ty == F32 {
                    ctx.builder.ins().f32const(0.0)
                } else if clif_ty == F64 {
                    ctx.builder.ins().f64const(0.0)
                } else {
                    ctx.builder.ins().iconst(clif_ty, 0)
                };
                ctx.builder.ins().return_(&[zero]);
            }
        }

        ctx.builder.seal_all_blocks();
        ctx.builder.finalize();

        let mut cl_ctx = cranelift_codegen::Context::new();
        cl_ctx.func = func;
        if let Err(e) = self.module.define_function(func_id, &mut cl_ctx) {
            eprintln!("Cranelift IR for '{name}':\n{}", cl_ctx.func.display());
            panic!("failed to define function '{name}': {e}");
        }
    }

    fn compile_global_decl(&mut self, decl: &Declaration) {
        let is_typedef = decl.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Typedef)));
        if is_typedef { return; }

        let is_extern = decl.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Extern)));
        let base_ty = self.resolve_type(&decl.specifiers);

        for id in &decl.declarators {
            let name = self.get_declarator_name(&id.declarator).unwrap_or_default();
            if name.is_empty() { continue; }

            let ty = self.apply_declarator(&base_ty, &id.declarator);

            // Function declarations (not definitions)
            if matches!(ty, CType::Function(..)) {
                let linkage = if is_extern { Linkage::Import } else { Linkage::Import };
                let mut sig = self.module.make_signature();
                if let CType::Function(ret, params, _variadic) = &ty {
                    for p in params {
                        let clif_ty = if self.is_float_type(&p.ty) {
                            self.clif_float_type(&p.ty)
                        } else {
                            self.clif_type(&p.ty)
                        };
                        sig.params.push(AbiParam::new(clif_ty));
                    }
                    if !matches!(ret.as_ref(), CType::Void) {
                        let clif_ty = if self.is_float_type(ret) {
                            self.clif_float_type(ret)
                        } else {
                            self.clif_type(ret)
                        };
                        sig.returns.push(AbiParam::new(clif_ty));
                    }
                }
                self.func_sigs.insert(name.clone(), sig.clone());
                if let Ok(id) = self.module.declare_function(&name, linkage, &sig) {
                    self.func_ids.insert(name.clone(), id);
                }
                continue;
            }

            // Global variable
            if is_extern {
                let _ = self.module.declare_data(&name, Linkage::Import, false, false);
            } else {
                let is_static = decl.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Static)));
                let linkage = if is_static { Linkage::Local } else { Linkage::Export };
                let data_id = self.module.declare_data(&name, linkage, true, false).unwrap();

                let mut desc = DataDescription::new();
                let size = ty.size();
                desc.define_zeroinit(size.max(1));
                // Ignore DuplicateDefinition — C allows tentative definitions
                let _ = self.module.define_data(data_id, &desc);
            }
        }
    }

    fn compile_stmt(&mut self, ctx: &mut FuncCtx, stmt: &Statement) {
        if ctx.filled {
            // Current block is terminated. Skip dead code unless it's a target.
            match stmt {
                Statement::Label(..) | Statement::Case(..) | Statement::Default(..) => {}
                _ => return,
            }
        }

        match stmt {
            Statement::Compound(items) => {
                for item in items {
                    if ctx.filled {
                        // Skip dead code after terminator, unless it's a label target
                        if let BlockItem::Stmt(Statement::Label(..) | Statement::Case(..) | Statement::Default(..)) = item {
                            // fall through — labels can be targets
                        } else {
                            continue;
                        }
                    }
                    match item {
                        BlockItem::Decl(d) => self.compile_local_decl(ctx, d),
                        BlockItem::Stmt(s) => self.compile_stmt(ctx, s),
                    }
                }
            }
            Statement::Expr(Some(e)) => { self.compile_expr(ctx, e); }
            Statement::Expr(None) => {}
            Statement::Return(val) => {
                if let Some(e) = val {
                    let v = self.compile_expr(ctx, e);
                    let ret_clif = if self.is_float_type(&ctx.return_type) {
                        self.clif_float_type(&ctx.return_type)
                    } else {
                        self.clif_type(&ctx.return_type)
                    };
                    let v = self.coerce(ctx, v, ret_clif);
                    ctx.builder.ins().return_(&[v]);
                } else {
                    ctx.builder.ins().return_(&[]);
                }
                ctx.filled = true;
            }
            Statement::If(cond, then, else_) => {
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
            }
            Statement::While(cond, body) => {
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
            }
            Statement::DoWhile(body, cond) => {
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
            }
            Statement::For(init, cond, step, body) => {
                if let Some(init) = init {
                    match init.as_ref() {
                        ForInit::Decl(d) => self.compile_local_decl(ctx, d),
                        ForInit::Expr(e) => { self.compile_expr(ctx, e); }
                    }
                }

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
            }
            Statement::Switch(val, body) => {
                let switch_val = self.compile_expr(ctx, val);
                let exit_block = ctx.builder.create_block();

                let prev_break = ctx.break_block.replace(exit_block);
                let prev_switch = ctx.switch_val.replace(switch_val);
                let prev_exit = ctx.switch_exit.replace(exit_block);

                self.compile_stmt(ctx, body);
                if !ctx.filled { ctx.builder.ins().jump(exit_block, &[]); }

                ctx.break_block = prev_break;
                ctx.switch_val = prev_switch;
                ctx.switch_exit = prev_exit;

                ctx.builder.switch_to_block(exit_block);
                ctx.builder.seal_block(exit_block);
                ctx.filled = false;
            }
            Statement::Case(val, body) => {
                let case_block = ctx.builder.create_block();
                let next_block = ctx.builder.create_block();

                if !ctx.filled {
                    if let Some(switch_val) = ctx.switch_val {
                        let case_val = self.compile_expr(ctx, val);
                        // Coerce both to common type
                        let svt = ctx.builder.func.dfg.value_type(switch_val);
                        let cvt = ctx.builder.func.dfg.value_type(case_val);
                        let common = if svt.bits() >= cvt.bits() { svt } else { cvt };
                        let sv = self.coerce(ctx, switch_val, common);
                        let cv = self.coerce(ctx, case_val, common);
                        let cmp = ctx.builder.ins().icmp(IntCC::Equal, sv, cv);
                        ctx.builder.ins().brif(cmp, case_block, &[], next_block, &[]);
                    } else {
                        ctx.builder.ins().jump(case_block, &[]);
                    }
                }

                ctx.builder.switch_to_block(case_block);
                ctx.builder.seal_block(case_block);
                ctx.filled = false;
                self.compile_stmt(ctx, body);
                if !ctx.filled { ctx.builder.ins().jump(next_block, &[]); }

                ctx.builder.switch_to_block(next_block);
                ctx.builder.seal_block(next_block);
                ctx.filled = false;
            }
            Statement::Default(body) => {
                let default_block = ctx.builder.create_block();
                if !ctx.filled {
                    ctx.builder.ins().jump(default_block, &[]);
                }
                ctx.builder.switch_to_block(default_block);
                ctx.builder.seal_block(default_block);
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

    fn compile_local_decl(&mut self, ctx: &mut FuncCtx, decl: &Declaration) {
        let is_typedef = decl.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Typedef)));
        if is_typedef { return; }

        let is_static = decl.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Static)));
        let base_ty = self.resolve_type(&decl.specifiers);

        for id in &decl.declarators {
            let name = self.get_declarator_name(&id.declarator).unwrap_or_default();
            if name.is_empty() { continue; }

            let ty = self.apply_declarator(&base_ty, &id.declarator);

            if is_static {
                // Static local — treat as global with mangled name to avoid namespace conflicts
                let mangled = format!("{}.{}", ctx.name, name);
                let data_id = self.module.declare_data(&mangled, Linkage::Local, true, false).unwrap();
                let mut desc = DataDescription::new();
                desc.define_zeroinit(ty.size().max(1));
                let _ = self.module.define_data(data_id, &desc);
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

    fn compile_aggregate_init(&mut self, ctx: &mut FuncCtx, ptr: Value, ty: &CType, init: &Initializer) {
        // Zero the memory first
        let size = ty.size();
        if size > 0 {
            let zero = ctx.builder.ins().iconst(I8, 0);
            let size_val = ctx.builder.ins().iconst(I64, size as i64);
            // memset-like: store zeros byte by byte for small structs
            // For larger ones, we'd want a memset call
            if size <= 64 {
                for i in 0..size {
                    let offset = ctx.builder.ins().iconst(I64, i as i64);
                    let addr = ctx.builder.ins().iadd(ptr, offset);
                    ctx.builder.ins().store(MemFlags::new(), zero, addr, 0);
                }
            }
        }
        // TODO: compile individual initializer fields
    }

    fn compile_expr(&mut self, ctx: &mut FuncCtx, expr: &Expr) -> Value {
        match expr {
            Expr::IntLit(v) => ctx.builder.ins().iconst(I64, *v as i64),
            Expr::UIntLit(v) => ctx.builder.ins().iconst(I64, *v as i64),
            Expr::FloatLit(v) => ctx.builder.ins().f64const(*v),
            Expr::CharLit(v) => ctx.builder.ins().iconst(I8, *v as i64),
            Expr::StringLit(s) => {
                let sym = format!(".str.{}", self.string_counter);
                self.string_counter += 1;
                let mut data = s.clone();
                data.push(0); // null terminator
                self.strings.push((sym.clone(), data));

                let data_id = self.module.declare_data(&sym, Linkage::Local, false, false).unwrap();
                let gv = self.module.declare_data_in_func(data_id, ctx.builder.func);
                ctx.builder.ins().global_value(I64, gv)
            }

            Expr::Ident(name) => {
                // Check locals first
                if let Some((var, _ty)) = ctx.locals.get(name) {
                    let var = *var;
                    return ctx.builder.use_var(var);
                }
                // Check local pointers (stack-allocated aggregates)
                if let Some((ptr, _ty)) = ctx.local_ptrs.get(name) {
                    return *ptr;
                }
                if let Some(&val) = self.type_env.enum_constants.get(name) {
                    return ctx.builder.ins().iconst(I32, val);
                }
                // Check globals
                if let Ok(data_id) = self.module.declare_data(name, Linkage::Import, true, false) {
                    let gv = self.module.declare_data_in_func(data_id, ctx.builder.func);
                    return ctx.builder.ins().global_value(I64, gv);
                }
                panic!("unknown identifier '{name}'")
            }

            Expr::Binary(op, lhs, rhs) => {
                let l = self.compile_expr(ctx, lhs);
                let r = self.compile_expr(ctx, rhs);
                self.compile_binop(ctx, *op, l, r)
            }

            Expr::Unary(op, e) => {
                match op {
                    UnaryOp::Neg => {
                        let v = self.compile_expr(ctx, e);
                        let vt = ctx.builder.func.dfg.value_type(v);
                        if vt.is_float() {
                            ctx.builder.ins().fneg(v)
                        } else {
                            ctx.builder.ins().ineg(v)
                        }
                    }
                    UnaryOp::BitNot => {
                        let v = self.compile_expr(ctx, e);
                        ctx.builder.ins().bnot(v)
                    }
                    UnaryOp::LogNot => {
                        let v = self.compile_expr(ctx, e);
                        let vt = ctx.builder.func.dfg.value_type(v);
                        let zero = ctx.builder.ins().iconst(vt, 0);
                        let is_zero = ctx.builder.ins().icmp(IntCC::Equal, v, zero);
                        ctx.builder.ins().uextend(vt, is_zero)
                    }
                    UnaryOp::Deref => {
                        let ptr = self.compile_expr(ctx, e);
                        ctx.builder.ins().load(I64, MemFlags::new(), ptr, 0)
                    }
                    UnaryOp::AddrOf => {
                        self.compile_addr(ctx, e)
                    }
                    UnaryOp::PreInc => {
                        let addr = self.compile_addr(ctx, e);
                        let val = ctx.builder.ins().load(I64, MemFlags::new(), addr, 0);
                        let one = ctx.builder.ins().iconst(I64, 1);
                        let new_val = ctx.builder.ins().iadd(val, one);
                        ctx.builder.ins().store(MemFlags::new(), new_val, addr, 0);
                        new_val
                    }
                    UnaryOp::PreDec => {
                        let addr = self.compile_addr(ctx, e);
                        let val = ctx.builder.ins().load(I64, MemFlags::new(), addr, 0);
                        let one = ctx.builder.ins().iconst(I64, 1);
                        let new_val = ctx.builder.ins().isub(val, one);
                        ctx.builder.ins().store(MemFlags::new(), new_val, addr, 0);
                        new_val
                    }
                }
            }

            Expr::PostUnary(op, e) => {
                let addr = self.compile_addr(ctx, e);
                let val = ctx.builder.ins().load(I64, MemFlags::new(), addr, 0);
                let one = ctx.builder.ins().iconst(I64, 1);
                let new_val = match op {
                    PostOp::PostInc => ctx.builder.ins().iadd(val, one),
                    PostOp::PostDec => ctx.builder.ins().isub(val, one),
                };
                ctx.builder.ins().store(MemFlags::new(), new_val, addr, 0);
                val // return old value
            }

            Expr::Assign(op, lhs, rhs) => {
                let rhs_val = self.compile_expr(ctx, rhs);

                // Direct variable assignment
                if let Expr::Ident(name) = lhs.as_ref() {
                    if let Some((var, ty)) = ctx.locals.get(name) {
                        let var = *var;
                        let var_clif = if self.is_float_type(&ty) {
                            self.clif_float_type(&ty)
                        } else {
                            self.clif_type(&ty)
                        };
                        let val = if *op == AssignOp::Assign {
                            rhs_val
                        } else {
                            let lhs_val = ctx.builder.use_var(var);
                            self.compile_compound_assign(ctx, *op, lhs_val, rhs_val)
                        };
                        let val = self.coerce(ctx, val, var_clif);
                        ctx.builder.def_var(var, val);
                        return val;
                    }
                }

                // Memory assignment
                let addr = self.compile_addr(ctx, lhs);
                let val = if *op == AssignOp::Assign {
                    rhs_val
                } else {
                    let lhs_val = ctx.builder.ins().load(I64, MemFlags::new(), addr, 0);
                    self.compile_compound_assign(ctx, *op, lhs_val, rhs_val)
                };
                ctx.builder.ins().store(MemFlags::new(), val, addr, 0);
                val
            }

            Expr::Call(func, args) => {
                let arg_vals: Vec<Value> = args.iter().map(|a| self.compile_expr(ctx, a)).collect();

                let func_name = match func.as_ref() {
                    Expr::Ident(name) => Some(name.clone()),
                    _ => None,
                };

                if let Some(name) = func_name {
                    // Use previously declared signature, or create an I64-based fallback
                    let declared_sig = self.func_sigs.get(&name).cloned();

                    // Build call signature matching actual arguments
                    let mut call_sig = self.module.make_signature();
                    for (i, &val) in arg_vals.iter().enumerate() {
                        let val_ty = ctx.builder.func.dfg.value_type(val);
                        if let Some(ref ds) = declared_sig {
                            if i < ds.params.len() {
                                call_sig.params.push(AbiParam::new(ds.params[i].value_type));
                            } else {
                                call_sig.params.push(AbiParam::new(val_ty));
                            }
                        } else {
                            call_sig.params.push(AbiParam::new(I64));
                        }
                    }
                    if let Some(ref ds) = declared_sig {
                        call_sig.returns = ds.returns.clone();
                    } else {
                        call_sig.returns.push(AbiParam::new(I64));
                    }

                    // Coerce arguments to match call signature
                    let mut coerced_args = Vec::new();
                    for (i, &val) in arg_vals.iter().enumerate() {
                        coerced_args.push(self.coerce(ctx, val, call_sig.params[i].value_type));
                    }

                    // Look up existing func_id, or declare new
                    let func_id = if let Some(&id) = self.func_ids.get(&name) {
                        id
                    } else if let Some(FuncOrDataId::Func(id)) = self.module.get_name(&name) {
                        self.func_ids.insert(name.clone(), id);
                        id
                    } else {
                        let id = self.module.declare_function(&name, Linkage::Import, &call_sig).unwrap();
                        self.func_ids.insert(name.clone(), id);
                        self.func_sigs.insert(name.clone(), call_sig.clone());
                        id
                    };
                    let func_ref = self.module.declare_func_in_func(func_id, ctx.builder.func);

                    // If declared sig doesn't match call args, use indirect call via sig_ref
                    let decl_sig = &self.module.declarations().get_function_decl(func_id).signature;
                    let declared_param_count = decl_sig.params.len();
                    let call = if declared_param_count != coerced_args.len() {
                        let sig_ref = ctx.builder.import_signature(call_sig);
                        let func_addr = ctx.builder.ins().func_addr(I64, func_ref);
                        ctx.builder.ins().call_indirect(sig_ref, func_addr, &coerced_args)
                    } else {
                        // Also check for type mismatches in params
                        let types_match = decl_sig.params.iter().zip(coerced_args.iter())
                            .all(|(p, &a)| p.value_type == ctx.builder.func.dfg.value_type(a));
                        if !types_match {
                            // Re-coerce to match declared signature
                            for (i, param) in decl_sig.params.iter().enumerate() {
                                coerced_args[i] = self.coerce(ctx, coerced_args[i], param.value_type);
                            }
                        }
                        ctx.builder.ins().call(func_ref, &coerced_args)
                    };
                    let results = ctx.builder.inst_results(call);
                    if results.is_empty() {
                        ctx.builder.ins().iconst(I64, 0)
                    } else {
                        results[0]
                    }
                } else {
                    // Indirect call (function pointer)
                    let func_ptr = self.compile_expr(ctx, func);
                    let mut sig = self.module.make_signature();
                    let coerced: Vec<Value> = arg_vals.iter()
                        .map(|&v| self.coerce(ctx, v, I64))
                        .collect();
                    for _ in &coerced {
                        sig.params.push(AbiParam::new(I64));
                    }
                    sig.returns.push(AbiParam::new(I64));
                    let sig_ref = ctx.builder.import_signature(sig);
                    let call = ctx.builder.ins().call_indirect(sig_ref, func_ptr, &coerced);
                    let results = ctx.builder.inst_results(call);
                    if results.is_empty() {
                        ctx.builder.ins().iconst(I64, 0)
                    } else {
                        results[0]
                    }
                }
            }

            Expr::Cast(_tn, e) => {
                // Simplified cast - just compile the inner expression
                self.compile_expr(ctx, e)
            }

            Expr::Sizeof(arg) => {
                let size = match arg.as_ref() {
                    SizeofArg::Type(tn) => {
                        let ty = self.resolve_type(&tn.specifiers);
                        ty.size()
                    }
                    SizeofArg::Expr(_) => 8, // TODO: determine expression type
                };
                ctx.builder.ins().iconst(I64, size as i64)
            }

            Expr::Alignof(tn) => {
                let ty = self.resolve_type(&tn.specifiers);
                ctx.builder.ins().iconst(I64, ty.align() as i64)
            }

            Expr::Conditional(cond, then, else_) => {
                let cond_val = self.compile_expr(ctx, cond);
                let cond_bool = self.to_bool(ctx, cond_val);

                let then_block = ctx.builder.create_block();
                let else_block = ctx.builder.create_block();
                let merge = ctx.builder.create_block();

                ctx.builder.ins().brif(cond_bool, then_block, &[], else_block, &[]);

                ctx.builder.switch_to_block(then_block);
                ctx.builder.seal_block(then_block);
                let then_val = self.compile_expr(ctx, then);
                let val_ty = ctx.builder.func.dfg.value_type(then_val);
                ctx.builder.ins().jump(merge, &[BlockArg::Value(then_val)]);

                ctx.builder.switch_to_block(else_block);
                ctx.builder.seal_block(else_block);
                let else_val = self.compile_expr(ctx, else_);
                let else_val = self.coerce(ctx, else_val, val_ty);
                ctx.builder.ins().jump(merge, &[BlockArg::Value(else_val)]);

                ctx.builder.append_block_param(merge, val_ty);
                ctx.builder.switch_to_block(merge);
                ctx.builder.seal_block(merge);
                ctx.builder.block_params(merge)[0]
            }

            Expr::Comma(a, b) => {
                self.compile_expr(ctx, a);
                self.compile_expr(ctx, b)
            }

            Expr::Index(arr, idx) => {
                let arr_val = self.compile_expr(ctx, arr);
                let idx_val = self.compile_expr(ctx, idx);
                let idx_val = self.coerce(ctx, idx_val, I64);
                let offset = ctx.builder.ins().imul_imm(idx_val, 8); // TODO: element size
                let addr = ctx.builder.ins().iadd(arr_val, offset);
                ctx.builder.ins().load(I64, MemFlags::new(), addr, 0)
            }

            Expr::Member(e, field) => {
                let base = self.compile_addr(ctx, e);
                // TODO: compute field offset from type
                ctx.builder.ins().load(I64, MemFlags::new(), base, 0)
            }

            Expr::Arrow(e, field) => {
                let ptr = self.compile_expr(ctx, e);
                // TODO: compute field offset from type
                ctx.builder.ins().load(I64, MemFlags::new(), ptr, 0)
            }

            Expr::StmtExpr(items) => {
                let mut last = ctx.builder.ins().iconst(I64, 0);
                for item in items {
                    match item {
                        BlockItem::Decl(d) => self.compile_local_decl(ctx, d),
                        BlockItem::Stmt(Statement::Expr(Some(e))) => { last = self.compile_expr(ctx, e); }
                        BlockItem::Stmt(s) => self.compile_stmt(ctx, s),
                    }
                }
                last
            }

            Expr::CompoundLiteral(_tn, _items) => {
                panic!("compound literals not yet implemented")
            }

            Expr::VaArg(_, _) => {
                panic!("va_arg not yet implemented")
            }

            Expr::Offsetof(_tn, _fields) => {
                panic!("offsetof not yet implemented")
            }

            Expr::Builtin(name, _args) => {
                panic!("builtin '{name}' not yet implemented")
            }
        }
    }

    fn compile_addr(&mut self, ctx: &mut FuncCtx, expr: &Expr) -> Value {
        match expr {
            Expr::Ident(name) => {
                // For stack-allocated locals, return their pointer
                if let Some((ptr, _)) = ctx.local_ptrs.get(name) {
                    return *ptr;
                }
                // For Variable-based locals, we need a stack slot
                // This is a simplification - ideally we'd track addresses
                if let Some((var, ty)) = ctx.locals.get(name).cloned() {
                    let size = ty.size().max(1);
                    let ss = ctx.builder.create_sized_stack_slot(ir::StackSlotData::new(
                        ir::StackSlotKind::ExplicitSlot, size as u32, 0,
                    ));
                    let ptr = ctx.builder.ins().stack_addr(I64, ss, 0);
                    let val = ctx.builder.use_var(var);
                    ctx.builder.ins().store(MemFlags::new(), val, ptr, 0);
                    ptr
                } else {
                    // Global
                    let data_id = self.module.declare_data(name, Linkage::Import, true, false)
                        .unwrap_or_else(|e| panic!("unknown identifier '{name}': {e}"));
                    let gv = self.module.declare_data_in_func(data_id, ctx.builder.func);
                    ctx.builder.ins().global_value(I64, gv)
                }
            }
            Expr::Unary(UnaryOp::Deref, e) => {
                self.compile_expr(ctx, e) // *p address is just p
            }
            Expr::Index(arr, idx) => {
                let arr_val = self.compile_expr(ctx, arr);
                let idx_val = self.compile_expr(ctx, idx);
                let idx_val = self.coerce(ctx, idx_val, I64);
                let offset = ctx.builder.ins().imul_imm(idx_val, 8); // TODO: element size
                ctx.builder.ins().iadd(arr_val, offset)
            }
            Expr::Member(e, _field) => {
                let base = self.compile_addr(ctx, e);
                // TODO: add field offset
                base
            }
            Expr::Arrow(e, _field) => {
                let ptr = self.compile_expr(ctx, e);
                // TODO: add field offset
                ptr
            }
            _ => {
                // Expression doesn't have an address - create a temporary
                let val = self.compile_expr(ctx, expr);
                let ss = ctx.builder.create_sized_stack_slot(ir::StackSlotData::new(
                    ir::StackSlotKind::ExplicitSlot, 8, 0,
                ));
                let ptr = ctx.builder.ins().stack_addr(I64, ss, 0);
                ctx.builder.ins().store(MemFlags::new(), val, ptr, 0);
                ptr
            }
        }
    }

    fn compile_binop(&mut self, ctx: &mut FuncCtx, op: BinOp, l: Value, r: Value) -> Value {
        // Coerce both operands to the wider type (C integer promotion)
        let lt = ctx.builder.func.dfg.value_type(l);
        let rt = ctx.builder.func.dfg.value_type(r);
        let is_float = lt.is_float() || rt.is_float();
        let common = if is_float {
            // Float promotion: if either is float, promote both
            if lt == F64 || rt == F64 { F64 } else { F32 }
        } else {
            if lt.bits() >= rt.bits() { lt } else { rt }
        };
        let l = self.coerce(ctx, l, common);
        let r = self.coerce(ctx, r, common);

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
                    ctx.builder.ins().uextend(int_type, c)
                }
                BinOp::Ne => {
                    let c = ctx.builder.ins().fcmp(FloatCC::NotEqual, l, r);
                    ctx.builder.ins().uextend(int_type, c)
                }
                BinOp::Lt => {
                    let c = ctx.builder.ins().fcmp(FloatCC::LessThan, l, r);
                    ctx.builder.ins().uextend(int_type, c)
                }
                BinOp::Gt => {
                    let c = ctx.builder.ins().fcmp(FloatCC::GreaterThan, l, r);
                    ctx.builder.ins().uextend(int_type, c)
                }
                BinOp::Le => {
                    let c = ctx.builder.ins().fcmp(FloatCC::LessThanOrEqual, l, r);
                    ctx.builder.ins().uextend(int_type, c)
                }
                BinOp::Ge => {
                    let c = ctx.builder.ins().fcmp(FloatCC::GreaterThanOrEqual, l, r);
                    ctx.builder.ins().uextend(int_type, c)
                }
                BinOp::LogAnd => {
                    let l_bool = self.to_bool(ctx, l);
                    let r_bool = self.to_bool(ctx, r);
                    let result = ctx.builder.ins().band(l_bool, r_bool);
                    ctx.builder.ins().uextend(int_type, result)
                }
                BinOp::LogOr => {
                    let l_bool = self.to_bool(ctx, l);
                    let r_bool = self.to_bool(ctx, r);
                    let result = ctx.builder.ins().bor(l_bool, r_bool);
                    ctx.builder.ins().uextend(int_type, result)
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
                BinOp::Div => ctx.builder.ins().sdiv(l, r),
                BinOp::Mod => ctx.builder.ins().srem(l, r),
                BinOp::BitAnd => ctx.builder.ins().band(l, r),
                BinOp::BitOr => ctx.builder.ins().bor(l, r),
                BinOp::BitXor => ctx.builder.ins().bxor(l, r),
                BinOp::Shl => ctx.builder.ins().ishl(l, r),
                BinOp::Shr => ctx.builder.ins().sshr(l, r),
                BinOp::Eq => {
                    let c = ctx.builder.ins().icmp(IntCC::Equal, l, r);
                    ctx.builder.ins().uextend(cmp_result, c)
                }
                BinOp::Ne => {
                    let c = ctx.builder.ins().icmp(IntCC::NotEqual, l, r);
                    ctx.builder.ins().uextend(cmp_result, c)
                }
                BinOp::Lt => {
                    let c = ctx.builder.ins().icmp(IntCC::SignedLessThan, l, r);
                    ctx.builder.ins().uextend(cmp_result, c)
                }
                BinOp::Gt => {
                    let c = ctx.builder.ins().icmp(IntCC::SignedGreaterThan, l, r);
                    ctx.builder.ins().uextend(cmp_result, c)
                }
                BinOp::Le => {
                    let c = ctx.builder.ins().icmp(IntCC::SignedLessThanOrEqual, l, r);
                    ctx.builder.ins().uextend(cmp_result, c)
                }
                BinOp::Ge => {
                    let c = ctx.builder.ins().icmp(IntCC::SignedGreaterThanOrEqual, l, r);
                    ctx.builder.ins().uextend(cmp_result, c)
                }
                BinOp::LogAnd => {
                    let l_bool = self.to_bool(ctx, l);
                    let r_bool = self.to_bool(ctx, r);
                    let result = ctx.builder.ins().band(l_bool, r_bool);
                    ctx.builder.ins().uextend(cmp_result, result)
                }
                BinOp::LogOr => {
                    let l_bool = self.to_bool(ctx, l);
                    let r_bool = self.to_bool(ctx, r);
                    let result = ctx.builder.ins().bor(l_bool, r_bool);
                    ctx.builder.ins().uextend(cmp_result, result)
                }
            }
        }
    }

    fn compile_compound_assign(&mut self, ctx: &mut FuncCtx, op: AssignOp, lhs: Value, rhs: Value) -> Value {
        // Coerce both operands to the wider type (same as compile_binop)
        let lt = ctx.builder.func.dfg.value_type(lhs);
        let rt = ctx.builder.func.dfg.value_type(rhs);
        let is_float = lt.is_float() || rt.is_float();
        let common = if is_float {
            if lt == F64 || rt == F64 { F64 } else { F32 }
        } else {
            if lt.bits() >= rt.bits() { lt } else { rt }
        };
        let lhs = self.coerce(ctx, lhs, common);
        let rhs = self.coerce(ctx, rhs, common);

        if is_float {
            match op {
                AssignOp::AddAssign => ctx.builder.ins().fadd(lhs, rhs),
                AssignOp::SubAssign => ctx.builder.ins().fsub(lhs, rhs),
                AssignOp::MulAssign => ctx.builder.ins().fmul(lhs, rhs),
                AssignOp::DivAssign => ctx.builder.ins().fdiv(lhs, rhs),
                _ => {
                    // Bitwise/shift/mod ops on floats: convert to int first
                    let lhs = self.coerce(ctx, lhs, I64);
                    let rhs = self.coerce(ctx, rhs, I64);
                    match op {
                        AssignOp::ModAssign => ctx.builder.ins().srem(lhs, rhs),
                        AssignOp::ShlAssign => ctx.builder.ins().ishl(lhs, rhs),
                        AssignOp::ShrAssign => ctx.builder.ins().sshr(lhs, rhs),
                        AssignOp::AndAssign => ctx.builder.ins().band(lhs, rhs),
                        AssignOp::XorAssign => ctx.builder.ins().bxor(lhs, rhs),
                        AssignOp::OrAssign => ctx.builder.ins().bor(lhs, rhs),
                        _ => unreachable!(),
                    }
                }
            }
        } else {
            match op {
                AssignOp::AddAssign => ctx.builder.ins().iadd(lhs, rhs),
                AssignOp::SubAssign => ctx.builder.ins().isub(lhs, rhs),
                AssignOp::MulAssign => ctx.builder.ins().imul(lhs, rhs),
                AssignOp::DivAssign => ctx.builder.ins().sdiv(lhs, rhs),
                AssignOp::ModAssign => ctx.builder.ins().srem(lhs, rhs),
                AssignOp::ShlAssign => ctx.builder.ins().ishl(lhs, rhs),
                AssignOp::ShrAssign => ctx.builder.ins().sshr(lhs, rhs),
                AssignOp::AndAssign => ctx.builder.ins().band(lhs, rhs),
                AssignOp::XorAssign => ctx.builder.ins().bxor(lhs, rhs),
                AssignOp::OrAssign => ctx.builder.ins().bor(lhs, rhs),
                AssignOp::Assign => unreachable!(),
            }
        }
    }

    fn to_bool(&self, ctx: &mut FuncCtx, val: Value) -> Value {
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

    fn coerce(&self, ctx: &mut FuncCtx, val: Value, target: ir::Type) -> Value {
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
            return ctx.builder.ins().fcvt_to_sint(target, val);
        }
        if val_type.is_int() && target.is_float() {
            return ctx.builder.ins().fcvt_from_sint(target, val);
        }

        val
    }

    fn define_strings(&mut self) {
        for (sym, data) in std::mem::take(&mut self.strings) {
            let data_id = self.module.declare_data(&sym, Linkage::Local, false, false).unwrap();
            let mut desc = DataDescription::new();
            desc.define(data.into_boxed_slice());
            self.module.define_data(data_id, &desc).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use cranelift_codegen::ir::types::I32;
    use cranelift_codegen::ir::{self, AbiParam, InstBuilder};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    use cranelift_module::{Linkage, Module};
    use crate::emit;
    use crate::lex::Lexer;
    use crate::parse::Parser;
    use crate::preprocess::Preprocessor;
    use super::*;

    fn compile_to_obj(src: &str) -> Vec<u8> {
        let mut pp = Preprocessor::new(vec![], vec![]);
        let preprocessed = pp.preprocess(src, "<test>");
        let tokens = Lexer::new(&preprocessed, "<test>").tokenize();
        let parser = Parser::new(tokens);
        let (tu, type_env) = parser.parse();
        let module = emit::create_module("<test>");
        let mut cg = Codegen::new(module, type_env);
        cg.compile_unit(&tu);
        emit::finish(cg.module)
    }

    fn assert_compiles(src: &str) {
        let obj = compile_to_obj(src);
        assert!(!obj.is_empty(), "object file should not be empty");
        // Check it starts with ELF magic
        assert_eq!(&obj[..4], b"\x7fELF", "should be valid ELF");
    }

    // === Diagnostic tests to narrow down the codegen bug ===

    #[test]
    fn diag_preprocess_output() {
        let mut pp = Preprocessor::new(vec![], vec![]);
        let out = pp.preprocess("int main(void) { return 42; }", "<test>");
        eprintln!("preprocessed:\n{}", out);
        let clean: String = out.lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>().join("\n");
        assert!(clean.contains("int main(void) { return 42; }"), "preprocessor should pass through: got {}", clean);
    }

    #[test]
    fn diag_lex_output() {
        let tokens = Lexer::new("int main(void) { return 42; }", "<test>").tokenize();
        let kinds: Vec<_> = tokens.iter().map(|t| format!("{:?}", t.kind)).collect();
        eprintln!("tokens: {}", kinds.join(", "));
        assert!(kinds.contains(&"Int".to_string()));
        assert!(kinds.contains(&"Return".to_string()));
        assert!(kinds.contains(&"IntLit(42)".to_string()));
    }

    #[test]
    fn diag_parse_output() {
        let mut pp = Preprocessor::new(vec![], vec![]);
        let preprocessed = pp.preprocess("int main(void) { return 42; }", "<test>");
        let tokens = Lexer::new(&preprocessed, "<test>").tokenize();
        let parser = Parser::new(tokens);
        let (tu, _type_env) = parser.parse();

        eprintln!("tu has {} items", tu.len());
        for (i, item) in tu.iter().enumerate() {
            match item {
                ExternalDecl::Function(f) => eprintln!("  [{}] Function: {:?}", i, f.declarator.direct),
                ExternalDecl::Declaration(d) => eprintln!("  [{}] Declaration: {} declarators", i, d.declarators.len()),
            }
        }

        assert_eq!(tu.len(), 1, "should have 1 external decl");
        match &tu[0] {
            ExternalDecl::Function(f) => {
                if let Statement::Compound(items) = &f.body {
                    eprintln!("  body has {} items", items.len());
                    for item in items {
                        eprintln!("    {:?}", item);
                    }
                    assert!(!items.is_empty(), "body should have statements");
                } else {
                    panic!("expected compound body");
                }
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn diag_resolve_type() {
        let mut pp = Preprocessor::new(vec![], vec![]);
        let preprocessed = pp.preprocess("int main(void) { return 42; }", "<test>");
        let tokens = Lexer::new(&preprocessed, "<test>").tokenize();
        let parser = Parser::new(tokens);
        let (tu, type_env) = parser.parse();

        let module = emit::create_module("<test>");
        let cg = Codegen::new(module, type_env);

        if let ExternalDecl::Function(f) = &tu[0] {
            let base_ty = cg.resolve_type(&f.specifiers);
            eprintln!("base type: {:?}", base_ty);
            let func_ty = cg.apply_declarator(&base_ty, &f.declarator);
            eprintln!("func type: {:?}", func_ty);
            match &func_ty {
                CType::Function(ret, params, variadic) => {
                    eprintln!("  ret: {:?}, params: {:?}, variadic: {}", ret, params, variadic);
                }
                _ => panic!("expected function type, got {:?}", func_ty),
            }
        }
    }

    #[test]
    fn diag_compile_function_direct() {
        // Bypass compile_function - do the codegen step by step manually
        use crate::types::TypeEnv;

        let module = emit::create_module("<test>");
        let type_env = TypeEnv::new();
        let mut cg = Codegen::new(module, type_env);

        // 1. Declare
        let mut sig = cg.module.make_signature();
        sig.returns.push(AbiParam::new(I32));
        let func_id = cg.module.declare_function("test_fn", Linkage::Export, &sig).unwrap();

        // 2. Build function with FunctionBuilder
        let mut func = ir::Function::with_name_signature(
            ir::UserFuncName::user(0, func_id.as_u32()),
            sig,
        );
        let mut fb_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut fb_ctx);

        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.ensure_inserted_block();
        builder.seal_block(entry);

        // Now simulate what compile_stmt does for Return(IntLit(42))
        let mut ctx = FuncCtx {
            builder,
            name: "test_fn".to_string(),
            locals: HashMap::new(),
            local_ptrs: HashMap::new(),
            return_type: CType::Int(true),
            filled: false,
            break_block: None,
            continue_block: None,
            switch_val: None,
            switch_exit: None,
            labels: HashMap::new(),
            gotos: Vec::new(),
        };

        // Check: is builder unreachable?
        eprintln!("is_unreachable before stmt: {}", ctx.builder.is_unreachable());
        eprintln!("current_block: {:?}", ctx.builder.current_block());

        // Compile: return 42;
        let body = Statement::Compound(vec![
            BlockItem::Stmt(Statement::Return(Some(Expr::IntLit(42)))),
        ]);
        cg.compile_stmt(&mut ctx, &body);

        eprintln!("is_unreachable after stmt: {}", ctx.builder.is_unreachable());

        ctx.builder.seal_all_blocks();
        ctx.builder.finalize();

        eprintln!("layout blocks: {}", func.layout.blocks().count());
        eprintln!("IR:\n{}", func.display());

        let mut cl_ctx = cranelift_codegen::Context::new();
        cl_ctx.func = func;
        cg.module.define_function(func_id, &mut cl_ctx).unwrap();

        let obj = emit::finish(cg.module);
        assert_eq!(&obj[..4], b"\x7fELF", "should produce valid ELF");
    }

    /// Raw Cranelift test to verify the API works directly.
    #[test]
    fn raw_cranelift_return_42() {
        let mut module = emit::create_module("<raw_test>");

        let mut sig = module.make_signature();
        sig.returns.push(AbiParam::new(I32));
        let func_id = module.declare_function("main", Linkage::Export, &sig).unwrap();

        let mut func = ir::Function::with_name_signature(
            ir::UserFuncName::user(0, func_id.as_u32()),
            sig,
        );

        let mut fb_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut fb_ctx);

        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let val = builder.ins().iconst(I32, 42);
        builder.ins().return_(&[val]);

        builder.seal_all_blocks();
        builder.finalize();

        eprintln!("raw cranelift IR:\n{}", func.display());

        let mut cl_ctx = cranelift_codegen::Context::new();
        cl_ctx.func = func;
        module.define_function(func_id, &mut cl_ctx).unwrap();

        let product = module.finish();
        let obj = product.emit().unwrap();
        assert_eq!(&obj[..4], b"\x7fELF");
    }

    #[test]
    fn return_constant() {
        assert_compiles("int main(void) { return 42; }");
    }

    #[test]
    fn empty_void_function() {
        assert_compiles("void f(void) {}");
    }

    #[test]
    fn return_zero() {
        assert_compiles("int main(void) { return 0; }");
    }

    #[test]
    fn simple_addition() {
        assert_compiles("int f(void) { return 1 + 2; }");
    }

    #[test]
    fn arithmetic_ops() {
        assert_compiles("int f(int a, int b) { return a + b - a * b / (b + 1); }");
    }

    #[test]
    fn local_variable() {
        assert_compiles("int f(void) { int x = 42; return x; }");
    }

    #[test]
    fn multiple_locals() {
        assert_compiles("int f(void) { int x = 1; int y = 2; return x + y; }");
    }

    #[test]
    fn if_statement() {
        assert_compiles("int f(int x) { if (x) return 1; return 0; }");
    }

    #[test]
    fn if_else_statement() {
        assert_compiles("int f(int x) { if (x) return 1; else return 0; }");
    }

    #[test]
    fn while_loop() {
        assert_compiles("int f(int n) { int s = 0; while (n > 0) { s = s + n; n = n - 1; } return s; }");
    }

    #[test]
    fn for_loop() {
        assert_compiles("int f(void) { int s = 0; for (int i = 0; i < 10; i = i + 1) { s = s + i; } return s; }");
    }

    #[test]
    fn do_while_loop() {
        assert_compiles("int f(void) { int x = 0; do { x = x + 1; } while (x < 10); return x; }");
    }

    #[test]
    fn nested_if() {
        assert_compiles("int f(int a, int b) { if (a) { if (b) return 1; return 2; } return 3; }");
    }

    #[test]
    fn comparison_operators() {
        assert_compiles(r#"
            int f(int a, int b) {
                if (a == b) return 0;
                if (a != b) return 1;
                if (a < b) return 2;
                if (a > b) return 3;
                if (a <= b) return 4;
                if (a >= b) return 5;
                return -1;
            }
        "#);
    }

    #[test]
    fn logical_operators() {
        assert_compiles("int f(int a, int b) { return (a && b) || (!a); }");
    }

    #[test]
    fn bitwise_operators() {
        assert_compiles("int f(int a, int b) { return (a & b) | (a ^ b) | (~a) | (a << 2) | (b >> 1); }");
    }

    #[test]
    fn multiple_functions() {
        assert_compiles(r#"
            int helper(int x) { return x * 2; }
            int main(void) { return helper(21); }
        "#);
    }

    #[test]
    fn function_call() {
        assert_compiles(r#"
            int add(int a, int b) { return a + b; }
            int main(void) { return add(1, 2); }
        "#);
    }

    #[test]
    fn global_variable() {
        assert_compiles(r#"
            int g;
            int f(void) { return g; }
        "#);
    }

    #[test]
    fn string_literal() {
        assert_compiles(r#"
            void f(void) { const char *s = "hello"; }
        "#);
    }

    #[test]
    fn pointer_ops() {
        assert_compiles(r#"
            int f(void) { int x = 42; int *p = &x; return *p; }
        "#);
    }

    #[test]
    fn break_continue() {
        assert_compiles(r#"
            int f(void) {
                int s = 0;
                for (int i = 0; i < 100; i = i + 1) {
                    if (i == 50) break;
                    if (i % 2 == 0) continue;
                    s = s + i;
                }
                return s;
            }
        "#);
    }

    #[test]
    fn switch_statement() {
        assert_compiles(r#"
            int f(int x) {
                switch (x) {
                    case 0: return 10;
                    case 1: return 20;
                    default: return 30;
                }
            }
        "#);
    }

    #[test]
    fn goto_label() {
        assert_compiles(r#"
            int f(void) {
                int x = 0;
                goto end;
                x = 42;
                end:
                return x;
            }
        "#);
    }

    #[test]
    fn conditional_expr() {
        assert_compiles("int f(int x) { return x ? 1 : 0; }");
    }

    #[test]
    fn cast_expr() {
        assert_compiles("long f(int x) { return (long)x; }");
    }

    #[test]
    fn unsigned_types() {
        assert_compiles("unsigned int f(unsigned int x) { return x + 1; }");
    }

    #[test]
    fn char_type() {
        assert_compiles("char f(char c) { return c + 1; }");
    }

    #[test]
    fn void_function_no_return() {
        assert_compiles("void f(void) { int x = 42; }");
    }

    #[test]
    fn static_function() {
        assert_compiles("static int helper(void) { return 42; } int main(void) { return helper(); }");
    }

    #[test]
    fn nested_loops() {
        assert_compiles(r#"
            int f(void) {
                int s = 0;
                for (int i = 0; i < 10; i = i + 1) {
                    for (int j = 0; j < 10; j = j + 1) {
                        s = s + 1;
                    }
                }
                return s;
            }
        "#);
    }

    #[test]
    fn unreachable_code_after_return() {
        assert_compiles(r#"
            int f(void) {
                return 1;
                int x = 2;
                return x;
            }
        "#);
    }

    #[test]
    fn all_paths_return() {
        assert_compiles(r#"
            int f(int x) {
                if (x > 0) {
                    return 1;
                } else {
                    return -1;
                }
            }
        "#);
    }

    #[test]
    fn complex_program() {
        assert_compiles(r#"
            int fibonacci(int n) {
                if (n <= 1) return n;
                return fibonacci(n - 1) + fibonacci(n - 2);
            }
            int main(void) {
                return fibonacci(10);
            }
        "#);
    }
}

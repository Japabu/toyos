use std::collections::{HashMap, HashSet};

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::*;
use cranelift_codegen::ir::{self, AbiParam, BlockArg, InstBuilder, MemFlags, StackSlotData, StackSlotKind, Value};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, FuncId, FuncOrDataId, Linkage, Module};
use cranelift_object::ObjectModule;
use crate::ast::*;
use crate::types::{CType, FieldDef, StructDef, TypeEnv, ParamType, EnumDef, Signedness};

mod resolve;
mod expr;
mod bitfield;
mod expr_type;
mod addr;
mod ops;
mod stmt;
mod init;

/// Cranelift Value paired with its C-level signedness.
/// Prevents signedness bugs by carrying the sign through all expression results.
#[derive(Clone, Copy)]
#[must_use = "TypedValue carries signedness — use .coerce() or .raw()"]
pub(crate) struct TypedValue {
    val: Value,
    sign: Signedness,
}

impl TypedValue {
    pub fn new(val: Value, sign: Signedness) -> Self { Self { val, sign } }
    pub fn signed(val: Value) -> Self { Self { val, sign: Signedness::Signed } }
    pub fn unsigned(val: Value) -> Self { Self { val, sign: Signedness::Unsigned } }
    pub fn raw(self) -> Value { self.val }
    pub fn signedness(self) -> Signedness { self.sign }
    pub fn is_unsigned(self) -> bool { self.sign == Signedness::Unsigned }
    pub fn with_sign(self, sign: Signedness) -> Self { Self { val: self.val, sign } }
}

/// Cranelift has no native variadic support. We pad signatures with extra I64
/// params to capture va_args — same approach as rustc_codegen_cranelift (#1500).
const VARIADIC_EXTRA_PARAMS: usize = 10;

/// Relocation to apply after defining global data bytes.
enum GlobalReloc {
    FuncAddr { offset: u32, func_id: FuncId },
    DataAddr { offset: u32, data_id: cranelift_module::DataId, addend: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arch {
    Aarch64,
    X86_64,
}

pub struct Codegen {
    pub module: ObjectModule,
    arch: Arch,
    type_env: TypeEnv,
    strings: Vec<(String, Vec<u8>)>, // (symbol name, data)
    string_counter: usize,
    static_counter: usize,
    func_sigs: HashMap<String, ir::Signature>,           // declared function signatures
    func_ids: HashMap<String, FuncId>,                   // declared function IDs
    data_ids: HashMap<String, cranelift_module::DataId>,  // declared global data IDs
    defined_data: HashSet<cranelift_module::DataId>,      // data IDs that have been defined
    tentative_data: Vec<(cranelift_module::DataId, usize)>, // tentative defs: (id, size)
    global_types: HashMap<String, CType>,                 // C types of global variables
    local_types: HashMap<String, CType>,                  // C types of in-scope local variables (for sizeof in array sizes)
    func_ret_types: HashMap<String, CType>,               // C return types of functions
    func_ctypes: HashMap<String, CType>,                   // full C function types (for expr_type)
    variadic_funcs: HashMap<String, usize>,                   // variadic func name → fixed param count
    static_funcs: HashSet<String>,                             // functions declared static
    extern_provision: HashSet<String>,                         // functions with non-inline external provision
}

/// How a local variable is stored during code generation.
/// Unifies the old `locals`, `local_ptrs`, and `spilled_locals` maps into a single
/// map, eliminating duplicated 3-way lookup chains across the codegen.
#[derive(Clone, Copy)]
pub(super) enum LocalStorage {
    /// SSA variable (cranelift Variable) — scalars that haven't had their address taken
    Ssa(Variable),
    /// Stack-allocated pointer (cranelift Value) — aggregates, arrays, VLAs, static locals
    Ptr(Value),
    /// Stack slot (address was taken) — scalars spilled because `&var` appears somewhere
    Spilled(ir::StackSlot),
}

/// State for compiling a switch statement. Extracted from FuncCtx so that
/// save/restore on nested switches is a single `take()`/assign, and the
/// "no switch active" state is unambiguously `None`.
pub(super) struct SwitchCtx {
    pub pending_fallthrough: Option<ir::Block>,
    pub dispatch_entries: Vec<(i128, ir::Block)>,
    pub dispatch_ranges: Vec<(i128, i128, ir::Block)>,
    pub default_block: Option<ir::Block>,
    pub case_blocks: Vec<ir::Block>,
}

pub(super) struct FuncCtx<'a> {
    builder: FunctionBuilder<'a>,
    name: String,
    locals: HashMap<String, (LocalStorage, CType)>,
    addr_taken: HashSet<String>, // names of variables whose address is taken anywhere in the function
    return_type: CType,
    filled: bool, // current block has a terminator
    // Control flow for break/continue
    break_block: Option<ir::Block>,
    continue_block: Option<ir::Block>,
    // Switch support
    switch: Option<SwitchCtx>,
    // Goto/labels
    labels: HashMap<String, ir::Block>,
    // Variadic function support
    va_area: Option<ir::StackSlot>,   // stack slot holding saved variadic args
    sret_ptr: Option<Value>,       // hidden return pointer for struct-returning functions
    // VLA support
    vla_sizes: HashMap<String, Value>,  // var name → runtime element count
    vla_allocs: Vec<Value>,             // malloc'd pointers to free at function exit
}

impl FuncCtx<'_> {
    /// Store a value into a local variable, regardless of storage kind.
    pub(super) fn store_to_local(&mut self, name: &str, val: Value) {
        let storage = self.locals.get(name)
            .map(|(s, _)| *s)
            .unwrap_or_else(|| panic!("store_to_local: unknown variable '{name}'"));
        match storage {
            LocalStorage::Ssa(var) => self.builder.def_var(var, val),
            LocalStorage::Spilled(slot) => {
                let ptr = self.builder.ins().stack_addr(I64, slot, 0);
                self.builder.ins().store(MemFlags::new(), val, ptr, 0);
            }
            LocalStorage::Ptr(ptr) => {
                self.builder.ins().store(MemFlags::new(), val, ptr, 0);
            }
        }
    }
}

impl Codegen {
    pub fn new(module: ObjectModule, type_env: TypeEnv) -> Self {
        use target_lexicon::Architecture;
        let arch = match module.isa().triple().architecture {
            Architecture::Aarch64(_) => Arch::Aarch64,
            Architecture::X86_64 => Arch::X86_64,
            other => panic!("unsupported architecture: {other}"),
        };
        Self {
            module,
            arch,
            type_env,
            strings: Vec::new(),
            string_counter: 0,
            static_counter: 0,
            func_sigs: HashMap::new(),
            func_ids: HashMap::new(),
            data_ids: HashMap::new(),
            defined_data: HashSet::new(),
            tentative_data: Vec::new(),
            global_types: HashMap::new(),
            local_types: HashMap::new(),
            func_ret_types: HashMap::new(),
            func_ctypes: HashMap::new(),
            variadic_funcs: HashMap::new(),
            static_funcs: HashSet::new(),
            extern_provision: HashSet::new(),
        }
    }

    /// Determine linkage for a function using pre-scanned declaration info.
    /// C99: function has external linkage unless all declarations are `inline` without
    /// `extern`, or any declaration is `static`.
    fn function_linkage(&self, name: &str) -> Linkage {
        if self.static_funcs.contains(name) {
            Linkage::Local
        } else if self.extern_provision.contains(name) {
            Linkage::Export
        } else {
            // All declarations were inline-only
            Linkage::Local
        }
    }

    /// On aarch64 the variadic calling convention requires all variadic args
    /// on the stack. We pad the remaining integer registers (8 total) with
    /// dummy zero args so the real variadic args spill to the stack.
    fn variadic_padding(&self, fixed_count: usize) -> usize {
        match self.arch {
            Arch::Aarch64 => 8usize.saturating_sub(fixed_count),
            Arch::X86_64 => 0,
        }
    }

    fn needs_sret(ret: &CType) -> bool {
        ret.is_aggregate()
    }

    fn build_signature(&self, ret: &CType, params: &[ParamType], variadic: bool) -> ir::Signature {
        let mut sig = self.module.make_signature();
        // Struct/union returns use a hidden first pointer parameter (sret)
        if Self::needs_sret(ret) {
            sig.params.push(AbiParam::new(I64));
        }
        for p in params {
            // Struct/union params are passed by pointer
            let clif_ty = if p.ty.is_aggregate() { I64 } else { self.clif_type(&p.ty) };
            sig.params.push(AbiParam::new(clif_ty));
        }
        if variadic {
            let padding = self.variadic_padding(params.len());
            for _ in 0..padding {
                sig.params.push(AbiParam::new(I64));
            }
            for _ in 0..VARIADIC_EXTRA_PARAMS {
                sig.params.push(AbiParam::new(I64));
            }
        }
        // No return value for sret functions
        if !Self::needs_sret(ret) && !matches!(ret, CType::Void) {
            let clif_ty = self.clif_type(ret);
            sig.returns.push(AbiParam::new(clif_ty));
        }
        sig
    }

    pub fn compile_unit(&mut self, tu: &TranslationUnit) {
        verbose!("compile_unit: {} declarations", tu.len());

        // Pre-scan: determine function linkage from all declarations/definitions.
        // C99: a function has external linkage unless ALL declarations use `inline` without
        // `extern`, or any declaration uses `static`.
        for decl in tu {
            let (name, specs) = match decl {
                ExternalDecl::Function(fdef) => {
                    let n = self.get_declarator_name(&fdef.declarator);
                    (n, &fdef.specifiers)
                }
                ExternalDecl::Declaration(d) => {
                    // Check if this is a function declaration
                    if let Some(id) = d.declarators.first() {
                        let n = self.get_declarator_name(&id.declarator);
                        (n, &d.specifiers)
                    } else { continue; }
                }
            };
            if name.is_empty() { continue; }
            let is_static = specs.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Static)));
            let is_inline = specs.iter().any(|s| matches!(s, DeclSpecifier::FuncSpec(FuncSpec::Inline)));
            let is_extern = specs.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Extern)));
            if is_static {
                self.static_funcs.insert(name);
            } else if !is_inline || is_extern {
                // Non-inline or explicit extern: provides external definition
                self.extern_provision.insert(name);
            }
        }

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

        verbose!("declarations done, compiling function bodies...");

        // Second pass: define functions
        for decl in tu {
            if let ExternalDecl::Function(fdef) = decl {
                self.compile_function(fdef);
            }
        }

        // Define string constants
        self.define_strings();

        // Finalize tentative definitions: zero-init any globals that were never given a real initializer.
        // Use the maximum size across all tentative entries for each data_id (handles incomplete
        // arrays like `int b[]` later completed by `int b[3]`).
        let mut tentative_sizes: std::collections::HashMap<_, usize> = std::collections::HashMap::new();
        for (data_id, size) in std::mem::take(&mut self.tentative_data) {
            let entry = tentative_sizes.entry(data_id).or_default();
            *entry = (*entry).max(size);
        }
        for (data_id, size) in tentative_sizes {
            if !self.defined_data.contains(&data_id) {
                let mut desc = DataDescription::new();
                desc.define_zeroinit(size.max(1));
                self.module.define_data(data_id, &desc).unwrap_or_else(|e| panic!("failed to define data: {e:?}"));
                self.defined_data.insert(data_id);
            }
        }
    }

    fn declare_function(&mut self, fdef: &FunctionDef) {
        let name = self.get_declarator_name(&fdef.declarator);
        if name.is_empty() { return; }
        verbose!("declare_function: {}", name);

        let base_ty = self.resolve_type(&fdef.specifiers);
        let func_ty = self.apply_declarator(&base_ty, &fdef.declarator);

        let CType::Function(ret_ty, param_types, variadic) = &func_ty else {
            panic!("declare_function '{name}': declarator resolved to non-function type {func_ty:?}");
        };
        let (ret_ty, param_types, variadic) = (ret_ty.as_ref(), param_types, *variadic);

        let linkage = self.function_linkage(&name);
        let sig = self.build_signature(ret_ty, param_types, variadic);
        if variadic {
            self.variadic_funcs.insert(name.clone(), param_types.len());
        }

        self.func_sigs.insert(name.clone(), sig.clone());
        let id = self.module.declare_function(&name, linkage, &sig)
            .unwrap_or_else(|e| panic!("failed to declare function '{name}': {e}"));
        self.func_ids.insert(name, id);
    }

    fn compile_function(&mut self, fdef: &FunctionDef) {
        let name = self.get_declarator_name(&fdef.declarator);
        if name.is_empty() { return; }
        crate::verbose::reset_depth();
        eprintln!("compiling: {name}");

        let base_ty = self.resolve_type(&fdef.specifiers);
        let func_ty = self.apply_declarator(&base_ty, &fdef.declarator);

        let CType::Function(ref ret_box, ref param_types_ref, variadic) = func_ty else {
            panic!("compile_function '{name}': declarator resolved to non-function type {func_ty:?}");
        };
        let (ret_ty, param_types, variadic) = (ret_box.as_ref().clone(), param_types_ref.clone(), variadic);

        let linkage = self.function_linkage(&name);
        let sig = self.build_signature(&ret_ty, &param_types, variadic);
        let variadic_pad = if variadic {
            self.variadic_funcs.insert(name.clone(), param_types.len());
            self.variadic_padding(param_types.len())
        } else { 0 };

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
        self.func_ret_types.insert(name.clone(), ret_ty.clone());
        self.func_ctypes.insert(name.clone(), func_ty.clone());
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

        // Scan the body for variables whose address is taken
        let mut addr_taken = HashSet::new();
        Self::collect_addr_taken(&fdef.body, &mut addr_taken);

        let mut ctx = FuncCtx {
            builder,
            name: name.clone(),
            locals: HashMap::new(),
            addr_taken,
            return_type: ret_ty,
            filled: false,
            break_block: None,
            continue_block: None,
            switch: None,
            labels: HashMap::new(),
            va_area: None,
            sret_ptr: None,
            vla_sizes: HashMap::new(),
            vla_allocs: Vec::new(),
        };

        // Bind sret pointer for struct-returning functions
        let params_block = entry;
        let has_sret = Self::needs_sret(&ctx.return_type);
        let param_offset = if has_sret { 1 } else { 0 };
        if has_sret {
            ctx.sret_ptr = Some(ctx.builder.block_params(params_block)[0]);
        }

        // Bind parameters
        let block_param_count = ctx.builder.block_params(params_block).len();
        for (i, p) in param_types.iter().enumerate() {
            let blk_idx = i + param_offset;
            if blk_idx >= block_param_count {
                break; // forward declaration had fewer params than definition
            }
            if let Some(name) = &p.name {
                self.local_types.insert(name.clone(), p.ty.clone());
                let val = ctx.builder.block_params(params_block)[blk_idx];
                // Struct/union params are passed by pointer: copy to local storage
                if p.ty.is_aggregate() {
                    let size = p.ty.size().max(1);
                    let ss = ctx.builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot, size as u32, 0));
                    let local_ptr = ctx.builder.ins().stack_addr(I64, ss, 0);
                    // Copy from caller's address to local storage
                    let size_val = ctx.builder.ins().iconst(I64, size as i64);
                    self.emit_memcpy(&mut ctx, local_ptr, val, size_val);
                    ctx.locals.insert(name.clone(), (LocalStorage::Ptr(local_ptr), p.ty.clone()));
                } else {
                    let clif_ty = self.clif_type(&p.ty);
                    let var = ctx.builder.declare_var(clif_ty);
                    ctx.builder.def_var(var, val);
                    ctx.locals.insert(name.clone(), (LocalStorage::Ssa(var), p.ty.clone()));
                }
            }
        }

        // For variadic functions, save extra params into a contiguous stack slot
        // (skip padding params on aarch64 — they just fill registers)
        if variadic {
            let slot_size = (VARIADIC_EXTRA_PARAMS * 8) as u32;
            let slot = ctx.builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, slot_size, 0));
            let fixed_count = param_types.len() + param_offset;
            for i in 0..VARIADIC_EXTRA_PARAMS {
                let param_idx = fixed_count + variadic_pad + i;
                let val = ctx.builder.block_params(params_block)[param_idx];
                let offset = (i * 8) as i32;
                ctx.builder.ins().stack_store(val, slot, offset);
            }
            ctx.va_area = Some(slot);
        }

        // Pre-spill parameters whose address is taken (&var) so the
        // stack slot is initialized in the entry block (dominates everything).
        let names: Vec<_> = ctx.addr_taken.iter().cloned().collect();
        for name in &names {
            if let Some((LocalStorage::Ssa(var), ty)) = ctx.locals.get(name).cloned() {
                let size = ty.size().max(1);
                let ss = ctx.builder.create_sized_stack_slot(ir::StackSlotData::new(
                    ir::StackSlotKind::ExplicitSlot, size as u32, 0,
                ));
                let ptr = ctx.builder.ins().stack_addr(I64, ss, 0);
                let val = ctx.builder.use_var(var);
                ctx.builder.ins().store(MemFlags::new(), val, ptr, 0);
                ctx.locals.insert(name.clone(), (LocalStorage::Spilled(ss), ty));
            }
        }

        // Compile body
        self.compile_stmt(&mut ctx, &fdef.body);

        // If the function doesn't end with a return, add one
        if !ctx.filled {
            for &ptr in &ctx.vla_allocs.clone() {
                self.emit_free(&mut ctx, ptr);
            }
            if matches!(ctx.return_type, CType::Void) || ctx.sret_ptr.is_some() {
                ctx.builder.ins().return_(&[]);
            } else {
                let clif_ty = self.clif_type(&ctx.return_type);
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
        self.local_types.clear();

        let mut cl_ctx = cranelift_codegen::Context::new();
        cl_ctx.func = func;
        if let Err(e) = self.module.define_function(func_id, &mut cl_ctx) {
            panic!("failed to define function '{name}': {e:?}");
        }
    }

    fn compile_global_decl(&mut self, decl: &Declaration) {
        let is_typedef = decl.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Typedef)));

        // Always resolve the type — this registers struct/union/enum tags as a side effect
        let base_ty = self.resolve_type(&decl.specifiers);

        if is_typedef {
            for id in &decl.declarators {
                let name = self.get_declarator_name(&id.declarator);
                if !name.is_empty() {
                    verbose!("typedef: {} = {:?}", name, self.apply_declarator(&base_ty, &id.declarator));
                }
            }
            // Resolve the actual type and store it in type_env
            for id in &decl.declarators {
                let name = self.get_declarator_name(&id.declarator);
                if !name.is_empty() {
                    let ty = self.apply_declarator(&base_ty, &id.declarator);
                    self.type_env.typedefs.insert(name, ty);
                }
            }
            return;
        }

        let is_extern = decl.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Extern)));

        for id in &decl.declarators {
            let name = self.get_declarator_name(&id.declarator);
            if name.is_empty() { continue; }

            let ty = self.apply_declarator(&base_ty, &id.declarator);
            verbose!("global_decl: {} : {:?}{}", name, ty, if is_extern { " (extern)" } else { "" });

            // Function declarations (not definitions)
            if ty.is_function() {
                if let CType::Function(ret, params, _) = &ty {
                    self.func_ret_types.insert(name.clone(), ret.as_ref().clone());
                    self.func_ctypes.insert(name.clone(), ty.clone());
                    if params.is_empty() {
                        // Unspecified params f() — don't lock in 0-param signature;
                        // the real signature will come from the definition or first call
                        continue;
                    }
                }
                if let CType::Function(ret, params, variadic) = &ty {
                    let sig = self.build_signature(ret, params, *variadic);
                    if *variadic {
                        self.variadic_funcs.insert(name.clone(), params.len());
                    }
                    self.func_sigs.insert(name.clone(), sig.clone());
                    self.func_ret_types.insert(name.clone(), ret.as_ref().clone());
                    let id = self.module.declare_function(&name, Linkage::Import, &sig)
                        .unwrap_or_else(|e| panic!("failed to declare function '{name}': {e}"));
                    self.func_ids.insert(name.clone(), id);
                }
                continue;
            }

            // Global variable
            let prior_ty = self.global_types.get(&name).cloned();
            self.global_types.insert(name.clone(), ty.clone());
            if is_extern {
                let data_id = self.module.declare_data(&name, Linkage::Import, false, false)
                    .unwrap_or_else(|e| panic!("failed to declare data '{name}': {e}"));
                self.data_ids.insert(name.clone(), data_id);
            } else {
                let is_static = decl.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Static)));
                let linkage = if is_static { Linkage::Local } else { Linkage::Export };
                let data_id = self.module.declare_data(&name, linkage, true, false).unwrap();
                self.data_ids.insert(name.clone(), data_id);

                let mut desc = DataDescription::new();
                desc.set_align(ty.align() as u64);

                // For incomplete arrays (e.g. `int arr[] = {1,2,3}` or `char s[] = "..."`), infer size from initializer.
                // If a prior declaration gave a larger size (e.g. `extern int arr[10]; int arr[] = {1,2}`),
                // use the declared size so remaining elements are zero-initialized.
                let (ty, size) = if let (CType::Array(elem, None), Some(Initializer::List(items))) = (&ty, &id.initializer) {
                    // Account for brace elision: if items are flat scalars and
                    // the element type is aggregate, divide by scalars per element
                    let scalars_per = elem.flat_init_count();
                    let mut n = if scalars_per > 1 && items.iter().all(|it| matches!(&it.initializer, Initializer::Expr(_))) {
                        (items.len() + scalars_per - 1) / scalars_per
                    } else {
                        items.len()
                    };
                    // Use prior declaration size if larger
                    if let Some(CType::Array(_, Some(prior_n))) = &prior_ty {
                        n = n.max(*prior_n);
                    }
                    let completed = CType::Array(elem.clone(), Some(n));
                    let sz = completed.size();
                    (completed, sz)
                } else if let (CType::Array(elem, None), Some(Initializer::Expr(e @ (Expr::StringLit(_) | Expr::WideStringLit(_))))) = (&ty, &id.initializer) {
                    let n = init::string_lit_elem_count(e);
                    let completed = CType::Array(elem.clone(), Some(n));
                    let sz = completed.size();
                    (completed, sz)
                } else {
                    let sz = if let Some(init) = &id.initializer {
                        Self::init_size(&ty, init)
                    } else {
                        ty.size()
                    };
                    (ty, sz)
                };

                // Update global_types with completed array type
                self.global_types.insert(name.clone(), ty.clone());

                if let Some(init) = &id.initializer {
                    self.init_global_data(&mut desc, size, &ty, init);
                    self.defined_data.insert(data_id);
                    self.module.define_data(data_id, &desc).unwrap_or_else(|e| panic!("failed to define data: {e:?}"));
                } else {
                    // Tentative definition — defer zeroinit in case a real definition follows
                    self.tentative_data.push((data_id, size));
                }
            }
        }
    }
}

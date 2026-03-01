use std::collections::{HashMap, HashSet};

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::*;
use cranelift_codegen::ir::{self, AbiParam, BlockArg, InstBuilder, MemFlags, StackSlotData, StackSlotKind, Value};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, FuncId, FuncOrDataId, Linkage, Module};
use cranelift_object::ObjectModule;
use target_lexicon::Architecture;

use crate::ast::*;
use crate::types::{CType, FieldDef, StructDef, TypeEnv, ParamType, EnumDef};

/// Cranelift has no native variadic support. We pad signatures with extra I64
/// params to capture va_args — same approach as rustc_codegen_cranelift (#1500).
const VARIADIC_EXTRA_PARAMS: usize = 10;

/// Relocation to apply after defining global data bytes.
enum GlobalReloc {
    FuncAddr { offset: u32, func_id: FuncId },
    DataAddr { offset: u32, data_id: cranelift_module::DataId },
}

pub struct Codegen {
    pub module: ObjectModule,
    type_env: TypeEnv,
    strings: Vec<(String, Vec<u8>)>, // (symbol name, data)
    string_counter: usize,
    func_sigs: HashMap<String, ir::Signature>,           // declared function signatures
    func_ids: HashMap<String, FuncId>,                   // declared function IDs
    data_ids: HashMap<String, cranelift_module::DataId>,  // declared global data IDs
    defined_data: HashSet<cranelift_module::DataId>,      // data IDs that have been defined
    tentative_data: Vec<(cranelift_module::DataId, usize)>, // tentative defs: (id, size)
    global_types: HashMap<String, CType>,                 // C types of global variables
    func_ret_types: HashMap<String, CType>,               // C return types of functions
    variadic_funcs: HashMap<String, usize>,                   // variadic func name → fixed param count
}

struct FuncCtx<'a> {
    builder: FunctionBuilder<'a>,
    name: String,
    locals: HashMap<String, (Variable, CType)>,
    local_ptrs: HashMap<String, (Value, CType)>, // stack-allocated aggregates
    spilled_locals: HashMap<String, (ir::StackSlot, CType)>, // locals whose address was taken
    addr_taken: HashSet<String>, // names of variables whose address is taken anywhere in the function
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
    _gotos: Vec<(String, ir::Block)>, // deferred gotos
    // Variadic function support
    va_area: Option<ir::StackSlot>, // stack slot holding saved variadic args
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
            data_ids: HashMap::new(),
            defined_data: HashSet::new(),
            tentative_data: Vec::new(),
            global_types: HashMap::new(),
            func_ret_types: HashMap::new(),
            variadic_funcs: HashMap::new(),
        }
    }

    /// True when targeting AArch64 (Apple Silicon or Linux arm64).
    fn is_aarch64(&self) -> bool {
        matches!(self.module.isa().triple().architecture, Architecture::Aarch64(_))
    }

    /// On aarch64 the variadic calling convention requires all variadic args
    /// on the stack. We pad the remaining integer registers (8 total) with
    /// dummy zero args so the real variadic args spill to the stack.
    fn variadic_padding(&self, fixed_count: usize) -> usize {
        if self.is_aarch64() { 8usize.saturating_sub(fixed_count) } else { 0 }
    }

    /// Evaluate a constant expression, resolving enum constants from the type environment.
    fn eval_const_with_enums(&self, expr: &Expr) -> Option<i64> {
        if let Some(val) = crate::parse::Parser::eval_const_expr_static(expr) {
            return Some(val);
        }
        match expr {
            Expr::UIntLit(v) => Some(*v as i64),
            Expr::Ident(name) => self.type_env.enum_constants.get(name).copied(),
            Expr::Cast(_, inner) => self.eval_const_with_enums(inner),
            Expr::Unary(UnaryOp::Neg, e) => self.eval_const_with_enums(e).map(|v| -v),
            Expr::Unary(UnaryOp::BitNot, e) => self.eval_const_with_enums(e).map(|v| !v),
            Expr::Binary(op, l, r) => {
                let l = self.eval_const_with_enums(l)?;
                let r = self.eval_const_with_enums(r)?;
                Some(match op {
                    BinOp::Add => l + r,
                    BinOp::Sub => l - r,
                    BinOp::Mul => l * r,
                    BinOp::Div => if r != 0 { l / r } else { 0 },
                    BinOp::Mod => if r != 0 { l % r } else { 0 },
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
            Expr::Conditional(cond, then, els) => {
                let c = self.eval_const_with_enums(cond)?;
                if c != 0 { self.eval_const_with_enums(then) } else { self.eval_const_with_enums(els) }
            }
            _ => None,
        }
    }

    /// Compute the byte offset of field at `target_idx` in a struct.
    fn struct_field_offset(def: &StructDef, target_idx: usize) -> usize {
        let mut offset = 0usize;
        for (i, field) in def.fields.iter().enumerate() {
            let align = if def.packed { 1 } else { field.ty.align() };
            offset = (offset + align - 1) & !(align - 1);
            if i == target_idx { return offset; }
            offset += field.ty.size();
        }
        offset
    }

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
        Self::elem_stride(&ty).unwrap_or(1)
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

    /// Extract function name from a call expression, seeing through
    /// comma expressions like `(side_effect(), func_name)(args...)`.
    fn extract_func_name(expr: &Expr) -> Option<String> {
        match expr {
            Expr::Ident(name) => Some(name.clone()),
            Expr::Comma(_, rhs) => Self::extract_func_name(rhs),
            _ => None,
        }
    }

    pub fn compile_unit(&mut self, tu: &TranslationUnit) {
        verbose!("compile_unit: {} declarations", tu.len());

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

        // Finalize tentative definitions: zero-init any globals that were never given a real initializer
        for (data_id, size) in std::mem::take(&mut self.tentative_data) {
            if !self.defined_data.contains(&data_id) {
                let mut desc = DataDescription::new();
                desc.define_zeroinit(size.max(1));
                let _ = self.module.define_data(data_id, &desc);
            }
        }
    }

    fn resolve_type(&mut self, specifiers: &[DeclSpecifier]) -> CType {
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
                let bit_width = fd.bit_width.as_ref().map(|_| 0u32); // TODO: evaluate constant
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
                if let Some(val) = crate::parse::Parser::eval_const_expr_static(expr) {
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

    fn apply_declarator(&mut self, base: &CType, d: &Declarator) -> CType {
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
                let n = size.as_ref().and_then(|e| crate::parse::Parser::eval_const_expr_static(e).map(|v| v as usize));
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
    fn resolve_typename(&mut self, tn: &TypeName) -> CType {
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
                let n = size.as_ref().and_then(|e| crate::parse::Parser::eval_const_expr_static(e).map(|v| v as usize));
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
        verbose!("declare_function: {}", name);

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
        // Variadic functions: pad registers on aarch64, then add extra params
        if variadic {
            let padding = self.variadic_padding(param_types.len());
            for _ in 0..padding {
                sig.params.push(AbiParam::new(I64));
            }
            for _ in 0..VARIADIC_EXTRA_PARAMS {
                sig.params.push(AbiParam::new(I64));
            }
            self.variadic_funcs.insert(name.clone(), param_types.len());
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

    /// Collect variable names whose address is taken (&var) anywhere in the body.
    fn collect_addr_taken(stmt: &Statement, out: &mut HashSet<String>) {
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

    fn compile_function(&mut self, fdef: &FunctionDef) {
        let name = self.get_declarator_name(&fdef.declarator).unwrap_or_default();
        if name.is_empty() { return; }
        crate::verbose::reset_depth();
        eprintln!("compiling: {name}");

        let base_ty = self.resolve_type(&fdef.specifiers);
        let func_ty = self.apply_declarator(&base_ty, &fdef.declarator);

        let (ret_ty, param_types, variadic) = match &func_ty {
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
        let variadic_pad = if variadic {
            let padding = self.variadic_padding(param_types.len());
            for _ in 0..padding {
                sig.params.push(AbiParam::new(I64));
            }
            for _ in 0..VARIADIC_EXTRA_PARAMS {
                sig.params.push(AbiParam::new(I64));
            }
            self.variadic_funcs.insert(name.clone(), param_types.len());
            padding
        } else { 0 };
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
        self.func_ret_types.insert(name.clone(), ret_ty.clone());
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
            local_ptrs: HashMap::new(),
            spilled_locals: HashMap::new(),
            addr_taken,
            return_type: ret_ty,
            filled: false,
            break_block: None,
            continue_block: None,
            switch_val: None,
            switch_exit: None,
            labels: HashMap::new(),
            _gotos: Vec::new(),
            va_area: None,
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

        // For variadic functions, save extra params into a contiguous stack slot
        // (skip padding params on aarch64 — they just fill registers)
        if variadic {
            let slot_size = (VARIADIC_EXTRA_PARAMS * 8) as u32;
            let slot = ctx.builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, slot_size, 0));
            let fixed_count = param_types.len();
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
            panic!("failed to define function '{name}': {e:?}");
        }
    }

    fn compile_global_decl(&mut self, decl: &Declaration) {
        let is_typedef = decl.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Typedef)));

        // Always resolve the type — this registers struct/union/enum tags as a side effect
        let base_ty = self.resolve_type(&decl.specifiers);

        if is_typedef {
            for id in &decl.declarators {
                if let Some(name) = self.get_declarator_name(&id.declarator) {
                    verbose!("typedef: {} = {:?}", name, self.apply_declarator(&base_ty, &id.declarator));
                }
            }
            // Resolve the actual type and store it in type_env
            for id in &decl.declarators {
                if let Some(name) = self.get_declarator_name(&id.declarator) {
                    let ty = self.apply_declarator(&base_ty, &id.declarator);
                    self.type_env.typedefs.insert(name, ty);
                }
            }
            return;
        }

        let is_extern = decl.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Extern)));

        for id in &decl.declarators {
            let name = self.get_declarator_name(&id.declarator).unwrap_or_default();
            if name.is_empty() { continue; }

            let ty = self.apply_declarator(&base_ty, &id.declarator);
            verbose!("global_decl: {} : {:?}{}", name, ty, if is_extern { " (extern)" } else { "" });

            // Function declarations (not definitions)
            if matches!(ty, CType::Function(..)) {
                let linkage = if is_extern { Linkage::Import } else { Linkage::Import };
                let mut sig = self.module.make_signature();
                if let CType::Function(ret, params, variadic) = &ty {
                    for p in params {
                        let clif_ty = if self.is_float_type(&p.ty) {
                            self.clif_float_type(&p.ty)
                        } else {
                            self.clif_type(&p.ty)
                        };
                        sig.params.push(AbiParam::new(clif_ty));
                    }
                    if *variadic {
                        let padding = self.variadic_padding(params.len());
                        for _ in 0..padding {
                            sig.params.push(AbiParam::new(I64));
                        }
                        for _ in 0..VARIADIC_EXTRA_PARAMS {
                            sig.params.push(AbiParam::new(I64));
                        }
                        self.variadic_funcs.insert(name.clone(), params.len());
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
                if let CType::Function(ret, ..) = &ty {
                    self.func_ret_types.insert(name.clone(), ret.as_ref().clone());
                }
                if let Ok(id) = self.module.declare_function(&name, linkage, &sig) {
                    self.func_ids.insert(name.clone(), id);
                }
                continue;
            }

            // Global variable
            self.global_types.insert(name.clone(), ty.clone());
            if is_extern {
                if let Ok(data_id) = self.module.declare_data(&name, Linkage::Import, false, false) {
                    self.data_ids.insert(name.clone(), data_id);
                }
            } else {
                let is_static = decl.specifiers.iter().any(|s| matches!(s, DeclSpecifier::StorageClass(StorageClass::Static)));
                let linkage = if is_static { Linkage::Local } else { Linkage::Export };
                let data_id = self.module.declare_data(&name, linkage, true, false).unwrap();
                self.data_ids.insert(name.clone(), data_id);

                let mut desc = DataDescription::new();
                desc.set_align(ty.align() as u64);

                // For incomplete arrays (e.g. `int arr[] = {1,2,3}` or `char s[] = "..."`), infer size from initializer.
                let (ty, size) = match (&ty, &id.initializer) {
                    (CType::Array(elem, None), Some(Initializer::List(items))) => {
                        let n = items.len();
                        let completed = CType::Array(elem.clone(), Some(n));
                        let sz = completed.size();
                        (completed, sz)
                    }
                    (CType::Array(elem, None), Some(Initializer::Expr(Expr::StringLit(data)))) => {
                        let n = data.len() + 1; // +1 for null terminator
                        let completed = CType::Array(elem.clone(), Some(n));
                        let sz = completed.size();
                        (completed, sz)
                    }
                    _ => {
                        let sz = ty.size();
                        (ty, sz)
                    }
                };

                if let Some(init) = &id.initializer {
                    self.init_global_data(&mut desc, size, &ty, init);
                    self.defined_data.insert(data_id);
                    let _ = self.module.define_data(data_id, &desc);
                } else {
                    // Tentative definition — defer zeroinit in case a real definition follows
                    self.tentative_data.push((data_id, size));
                }
            }
        }
    }

    fn compile_stmt(&mut self, ctx: &mut FuncCtx, stmt: &Statement) {
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
            Statement::Asm(_) => "Asm",
        };
        verbose_enter!("compile_stmt", "{}", stmt_name);
        stacker::maybe_grow(128 * 1024, 2 * 1024 * 1024, || {
            self.compile_stmt_inner(ctx, stmt);
        });
        verbose_leave!();
    }

    fn compile_stmt_inner(&mut self, ctx: &mut FuncCtx, stmt: &Statement) {
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

        // Always resolve the type — this registers struct/union/enum tags as a side effect
        let base_ty = self.resolve_type(&decl.specifiers);

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

        for id in &decl.declarators {
            let name = self.get_declarator_name(&id.declarator).unwrap_or_default();
            if name.is_empty() { continue; }

            let ty = self.apply_declarator(&base_ty, &id.declarator);
            verbose!("local_decl: {} : {:?} (init={})", name, ty, id.initializer.is_some());

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

            // If address is taken (&var anywhere in function), allocate on
            // stack from the start so the slot is valid in all basic blocks.
            if ctx.addr_taken.contains(&name) {
                let size = ty.size().max(1);
                let ss = ctx.builder.create_sized_stack_slot(ir::StackSlotData::new(
                    ir::StackSlotKind::ExplicitSlot, size as u32, 0,
                ));
                let ptr = ctx.builder.ins().stack_addr(I64, ss, 0);
                if let Some(init) = &id.initializer {
                    if let Initializer::Expr(e) = init {
                        let val = self.compile_expr(ctx, e);
                        let val = self.coerce(ctx, val, clif_ty);
                        ctx.builder.ins().store(MemFlags::new(), val, ptr, 0);
                    } else {
                        // Zero-init
                        let zero = ctx.builder.ins().iconst(clif_ty, 0);
                        ctx.builder.ins().store(MemFlags::new(), zero, ptr, 0);
                    }
                } else {
                    let zero = ctx.builder.ins().iconst(clif_ty, 0);
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

    fn compile_aggregate_init(&mut self, ctx: &mut FuncCtx, ptr: Value, ty: &CType, _init: &Initializer) {
        // Zero the memory first
        let size = ty.size();
        if size > 0 {
            let zero = ctx.builder.ins().iconst(I8, 0);
            let _size_val = ctx.builder.ins().iconst(I64, size as i64);
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
        let expr_name = match expr {
            Expr::IntLit(v) => format!("IntLit({v})"),
            Expr::UIntLit(v) => format!("UIntLit({v})"),
            Expr::FloatLit(v) => format!("FloatLit({v})"),
            Expr::CharLit(v) => format!("CharLit({v})"),
            Expr::StringLit(_) => "StringLit".into(),
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
            Expr::Offsetof(..) => "Offsetof".into(),
            Expr::Builtin(n, _) => format!("Builtin({n})"),
        };
        verbose_enter!("compile_expr", "{}", expr_name);
        let result = stacker::maybe_grow(128 * 1024, 2 * 1024 * 1024, || {
            self.compile_expr_inner(ctx, expr)
        });
        verbose_leave!();
        result
    }

    fn compile_expr_inner(&mut self, ctx: &mut FuncCtx, expr: &Expr) -> Value {
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

            Expr::Ident(name) if name == "__func__" || name == "__FUNCTION__" => {
                // C99 __func__ / GCC __FUNCTION__: string literal with current function name
                let func_name = ctx.name.clone();
                let sym = format!(".str.{}", self.string_counter);
                self.string_counter += 1;
                let mut data: Vec<u8> = func_name.into_bytes();
                data.push(0);
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
                // Check spilled locals (address was taken — load from stack)
                if let Some((slot, ty)) = ctx.spilled_locals.get(name) {
                    let ptr = ctx.builder.ins().stack_addr(I64, *slot, 0);
                    let load_ty = if self.is_float_type(ty) {
                        self.clif_float_type(ty)
                    } else {
                        self.clif_type(ty)
                    };
                    return ctx.builder.ins().load(load_ty, MemFlags::new(), ptr, 0);
                }
                // Check local pointers (stack-allocated aggregates)
                if let Some((ptr, ty)) = ctx.local_ptrs.get(name) {
                    return match ty {
                        CType::Struct(_) | CType::Union(_) | CType::Array(_, _) => *ptr,
                        _ => {
                            let load_ty = if self.is_float_type(ty) {
                                self.clif_float_type(ty)
                            } else {
                                self.clif_type(ty)
                            };
                            ctx.builder.ins().load(load_ty, MemFlags::new(), *ptr, 0)
                        }
                    };
                }
                if let Some(&val) = self.type_env.enum_constants.get(name) {
                    return ctx.builder.ins().iconst(I32, val);
                }
                // Check declared functions — return as function pointer
                if let Some(func_id) = self.func_ids.get(name) {
                    let func_ref = self.module.declare_func_in_func(*func_id, ctx.builder.func);
                    return ctx.builder.ins().func_addr(I64, func_ref);
                }
                // Check previously declared global data
                if let Some(data_id) = self.data_ids.get(name) {
                    let gv = self.module.declare_data_in_func(*data_id, ctx.builder.func);
                    let addr = ctx.builder.ins().global_value(I64, gv);
                    // For scalar types, load the value; arrays/structs return the address
                    if let Some(ty) = self.global_types.get(name) {
                        if !matches!(ty, CType::Array(..) | CType::Struct(_) | CType::Union(_)) {
                            let load_ty = if self.is_float_type(ty) {
                                self.clif_float_type(ty)
                            } else {
                                self.clif_type(ty)
                            };
                            return ctx.builder.ins().load(load_ty, MemFlags::new(), addr, 0);
                        }
                    }
                    return addr;
                }
                // Undeclared global — import it (C89 implicit declaration)
                if let Ok(data_id) = self.module.declare_data(name, Linkage::Import, true, false) {
                    self.data_ids.insert(name.clone(), data_id);
                    let gv = self.module.declare_data_in_func(data_id, ctx.builder.func);
                    let addr = ctx.builder.ins().global_value(I64, gv);
                    // Unknown type — assume scalar, load as I64
                    return ctx.builder.ins().load(I64, MemFlags::new(), addr, 0);
                }
                panic!("unknown identifier '{name}'")
            }

            Expr::Binary(op, lhs, rhs) => {
                // Pointer arithmetic: ptr + n => ptr + n*sizeof(*ptr)
                if matches!(op, BinOp::Add | BinOp::Sub) {
                    let lty = self.expr_type(ctx, lhs);
                    let rty = self.expr_type(ctx, rhs);
                    let l_stride = Self::elem_stride(&lty);
                    let r_stride = Self::elem_stride(&rty);

                    if l_stride.is_some() && r_stride.is_none() {
                        let stride = l_stride.unwrap();
                        let l = self.compile_expr(ctx, lhs);
                        let r = self.compile_expr(ctx, rhs);
                        let r = self.coerce(ctx, r, I64);
                        let r = if stride != 1 {
                            let s = ctx.builder.ins().iconst(I64, stride);
                            ctx.builder.ins().imul(r, s)
                        } else { r };
                        return self.compile_binop(ctx, *op, l, r);
                    }
                    if r_stride.is_some() && l_stride.is_none() && matches!(op, BinOp::Add) {
                        let stride = r_stride.unwrap();
                        let l = self.compile_expr(ctx, lhs);
                        let r = self.compile_expr(ctx, rhs);
                        let l = self.coerce(ctx, l, I64);
                        let l = if stride != 1 {
                            let s = ctx.builder.ins().iconst(I64, stride);
                            ctx.builder.ins().imul(l, s)
                        } else { l };
                        return self.compile_binop(ctx, *op, l, r);
                    }
                    // ptr - ptr => (ptr - ptr) / sizeof(*ptr)
                    if l_stride.is_some() && r_stride.is_some() && matches!(op, BinOp::Sub) {
                        let stride = l_stride.unwrap();
                        let l = self.compile_expr(ctx, lhs);
                        let r = self.compile_expr(ctx, rhs);
                        let diff = self.compile_binop(ctx, *op, l, r);
                        if stride != 1 {
                            let s = ctx.builder.ins().iconst(I64, stride);
                            return ctx.builder.ins().sdiv(diff, s);
                        }
                        return diff;
                    }
                }
                // Short-circuit evaluation for && and ||
                if matches!(op, BinOp::LogAnd | BinOp::LogOr) {
                    let l = self.compile_expr(ctx, lhs);
                    let l_bool = self.to_bool(ctx, l);

                    let rhs_block = ctx.builder.create_block();
                    let merge = ctx.builder.create_block();

                    if *op == BinOp::LogAnd {
                        // &&: if lhs is false, result is 0; otherwise evaluate rhs
                        let false_val = ctx.builder.ins().iconst(I64, 0);
                        ctx.builder.ins().brif(l_bool, rhs_block, &[], merge, &[BlockArg::Value(false_val)]);
                    } else {
                        // ||: if lhs is true, result is 1; otherwise evaluate rhs
                        let true_val = ctx.builder.ins().iconst(I64, 1);
                        ctx.builder.ins().brif(l_bool, merge, &[BlockArg::Value(true_val)], rhs_block, &[]);
                    }

                    ctx.builder.switch_to_block(rhs_block);
                    ctx.builder.seal_block(rhs_block);
                    let r = self.compile_expr(ctx, rhs);
                    let r_bool = self.to_bool(ctx, r);
                    let r_i64 = Self::safe_uextend(ctx, I64, r_bool);
                    ctx.builder.ins().jump(merge, &[BlockArg::Value(r_i64)]);

                    ctx.builder.append_block_param(merge, I64);
                    ctx.builder.switch_to_block(merge);
                    ctx.builder.seal_block(merge);
                    return ctx.builder.block_params(merge)[0];
                }
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
                        Self::safe_uextend(ctx,vt, is_zero)
                    }
                    UnaryOp::Deref => {
                        let ptr = self.compile_expr(ctx, e);
                        // Determine load type from the pointed-to type
                        let deref_ty = self.expr_type(ctx, e).and_then(|ty| match ty {
                            CType::Pointer(inner) => Some(*inner),
                            _ => None,
                        });
                        if let Some(ref ty) = deref_ty {
                            if matches!(ty, CType::Struct(_) | CType::Union(_) | CType::Array(..)) {
                                return ptr;
                            }
                        }
                        let load_ty = deref_ty
                            .map(|ty| if self.is_float_type(&ty) { self.clif_float_type(&ty) } else { self.clif_type(&ty) })
                            .unwrap_or(I64);
                        ctx.builder.ins().load(load_ty, MemFlags::new(), ptr, 0)
                    }
                    UnaryOp::AddrOf => {
                        self.compile_addr(ctx, e)
                    }
                    UnaryOp::PreInc => {
                        let stride = self.pointer_stride(ctx, e);
                        if let Expr::Ident(name) = e.as_ref() {
                            if let Some((var, _)) = ctx.locals.get(name) {
                                let var = *var;
                                let val = ctx.builder.use_var(var);
                                let vt = ctx.builder.func.dfg.value_type(val);
                                let step = ctx.builder.ins().iconst(vt, stride);
                                let new_val = ctx.builder.ins().iadd(val, step);
                                ctx.builder.def_var(var, new_val);
                                return new_val;
                            }
                        }
                        let mem_ty = self.expr_type(ctx, e).map(|ty| {
                            if self.is_float_type(&ty) { self.clif_float_type(&ty) } else { self.clif_type(&ty) }
                        }).unwrap_or(I64);
                        let addr = self.compile_addr(ctx, e);
                        let val = ctx.builder.ins().load(mem_ty, MemFlags::new(), addr, 0);
                        let step = ctx.builder.ins().iconst(mem_ty, stride);
                        let new_val = ctx.builder.ins().iadd(val, step);
                        ctx.builder.ins().store(MemFlags::new(), new_val, addr, 0);
                        new_val
                    }
                    UnaryOp::PreDec => {
                        let stride = self.pointer_stride(ctx, e);
                        if let Expr::Ident(name) = e.as_ref() {
                            if let Some((var, _)) = ctx.locals.get(name) {
                                let var = *var;
                                let val = ctx.builder.use_var(var);
                                let vt = ctx.builder.func.dfg.value_type(val);
                                let step = ctx.builder.ins().iconst(vt, stride);
                                let new_val = ctx.builder.ins().isub(val, step);
                                ctx.builder.def_var(var, new_val);
                                return new_val;
                            }
                        }
                        let mem_ty = self.expr_type(ctx, e).map(|ty| {
                            if self.is_float_type(&ty) { self.clif_float_type(&ty) } else { self.clif_type(&ty) }
                        }).unwrap_or(I64);
                        let addr = self.compile_addr(ctx, e);
                        let val = ctx.builder.ins().load(mem_ty, MemFlags::new(), addr, 0);
                        let step = ctx.builder.ins().iconst(mem_ty, stride);
                        let new_val = ctx.builder.ins().isub(val, step);
                        ctx.builder.ins().store(MemFlags::new(), new_val, addr, 0);
                        new_val
                    }
                }
            }

            Expr::PostUnary(op, e) => {
                let stride = self.pointer_stride(ctx, e);
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
                        return val; // return old value
                    }
                }
                let mem_ty = self.expr_type(ctx, e).map(|ty| {
                    if self.is_float_type(&ty) { self.clif_float_type(&ty) } else { self.clif_type(&ty) }
                }).unwrap_or(I64);
                let addr = self.compile_addr(ctx, e);
                let val = ctx.builder.ins().load(mem_ty, MemFlags::new(), addr, 0);
                let step = ctx.builder.ins().iconst(mem_ty, stride);
                let new_val = match op {
                    PostOp::PostInc => ctx.builder.ins().iadd(val, step),
                    PostOp::PostDec => ctx.builder.ins().isub(val, step),
                };
                ctx.builder.ins().store(MemFlags::new(), new_val, addr, 0);
                val // return old value
            }

            Expr::Assign(op, lhs, rhs) => {
                let mut rhs_val = self.compile_expr(ctx, rhs);

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
                    // Spilled locals: store through stack slot
                    if let Some((slot, ty)) = ctx.spilled_locals.get(name) {
                        let slot = *slot;
                        let var_clif = if self.is_float_type(&ty) {
                            self.clif_float_type(&ty)
                        } else {
                            self.clif_type(&ty)
                        };
                        let ptr = ctx.builder.ins().stack_addr(I64, slot, 0);
                        let val = if *op == AssignOp::Assign {
                            rhs_val
                        } else {
                            let lhs_val = ctx.builder.ins().load(var_clif, MemFlags::new(), ptr, 0);
                            self.compile_compound_assign(ctx, *op, lhs_val, rhs_val)
                        };
                        let val = self.coerce(ctx, val, var_clif);
                        ctx.builder.ins().store(MemFlags::new(), val, ptr, 0);
                        return val;
                    }
                }

                // Memory assignment — determine LHS type for correct store size
                let lhs_ty = self.expr_type(ctx, lhs);
                let addr = self.compile_addr(ctx, lhs);
                let store_clif = lhs_ty.as_ref().map(|ty| {
                    if self.is_float_type(ty) { self.clif_float_type(ty) } else { self.clif_type(ty) }
                }).unwrap_or(I64);
                let val = if *op == AssignOp::Assign {
                    rhs_val
                } else {
                    let lhs_val = ctx.builder.ins().load(store_clif, MemFlags::new(), addr, 0);
                    self.compile_compound_assign(ctx, *op, lhs_val, rhs_val)
                };
                let val = self.coerce(ctx, val, store_clif);
                ctx.builder.ins().store(MemFlags::new(), val, addr, 0);
                val
            }

            Expr::Call(func, args) => {
                let arg_vals: Vec<Value> = args.iter().map(|a| self.compile_expr(ctx, a)).collect();

                let func_name = Self::extract_func_name(func);

                // If the function expression has side effects (e.g. comma expr
                // like `(tcc_enter_state(s1), func)(args)`), compile them now.
                if func_name.is_some() && !matches!(func.as_ref(), Expr::Ident(_)) {
                    self.compile_expr(ctx, func);
                }

                if let Some(ref name) = func_name {
                    // Check if this is actually a variable (function pointer), not a function
                    let is_var = ctx.locals.contains_key(name)
                        || ctx.spilled_locals.contains_key(name)
                        || ctx.local_ptrs.contains_key(name)
                        || self.data_ids.contains_key(name);
                    if is_var {
                        // Indirect call through function pointer variable
                        // compile_expr loads the value for scalar globals,
                        // and local_ptrs returns the stack address (need to load)
                        let func_ptr = if ctx.local_ptrs.contains_key(name) {
                            let addr = self.compile_expr(ctx, func);
                            ctx.builder.ins().load(I64, MemFlags::new(), addr, 0)
                        } else {
                            // locals and globals: compile_expr returns the value
                            self.compile_expr(ctx, func)
                        };
                        let mut call_sig = self.module.make_signature();
                        for &val in &arg_vals {
                            let val_ty = ctx.builder.func.dfg.value_type(val);
                            call_sig.params.push(AbiParam::new(val_ty));
                        }
                        call_sig.returns.push(AbiParam::new(I64));
                        let sig_ref = ctx.builder.import_signature(call_sig);
                        let call = ctx.builder.ins().call_indirect(sig_ref, func_ptr, &arg_vals);
                        let results = ctx.builder.inst_results(call);
                        return if results.is_empty() {
                            ctx.builder.ins().iconst(I64, 0)
                        } else {
                            results[0]
                        };
                    }

                    // Use previously declared signature, or create an I64-based fallback
                    let declared_sig = self.func_sigs.get(name).cloned();

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

                    // On aarch64, variadic args must go on the stack.
                    // Insert dummy zero args to fill the 8 integer registers,
                    // pushing the real variadic args to the stack.
                    if let Some(&fixed_count) = self.variadic_funcs.get(name) {
                        let padding = self.variadic_padding(fixed_count);
                        if padding > 0 {
                            let zero = ctx.builder.ins().iconst(I64, 0);
                            for j in 0..padding {
                                coerced_args.insert(fixed_count + j, zero);
                                call_sig.params.insert(fixed_count + j, AbiParam::new(I64));
                            }
                        }
                    }

                    // Look up existing func_id, or declare new
                    let func_id = if let Some(&id) = self.func_ids.get(name) {
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
                        let ty = self.resolve_typename(tn);
                        ty.size()
                    }
                    SizeofArg::Expr(e) => {
                        let ty = self.expr_type(ctx, e).unwrap_or(CType::Int(true));
                        ty.size()
                    }
                };
                ctx.builder.ins().iconst(I64, size as i64)
            }

            Expr::Alignof(tn) => {
                let ty = self.resolve_typename(tn);
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
                let elem_size = self.expr_type(ctx, arr)
                    .and_then(|ty| match ty {
                        CType::Pointer(inner) | CType::Array(inner, _) => Some(inner.size()),
                        _ => None,
                    })
                    .unwrap_or(8);
                let arr_val = self.compile_expr(ctx, arr);
                let idx_val = self.compile_expr(ctx, idx);
                let idx_val = self.coerce(ctx, idx_val, I64);
                let offset = ctx.builder.ins().imul_imm(idx_val, elem_size as i64);
                let addr = ctx.builder.ins().iadd(arr_val, offset);
                let load_ty = self.expr_type(ctx, expr)
                    .map(|ty| if self.is_float_type(&ty) { self.clif_float_type(&ty) } else { self.clif_type(&ty) })
                    .unwrap_or(I64);
                ctx.builder.ins().load(load_ty, MemFlags::new(), addr, 0)
            }

            Expr::Member(e, field) => {
                let base = self.compile_addr(ctx, e);
                let (byte_offset, field_ty) = self.expr_type(ctx, e)
                    .and_then(|ty| ty.field_offset(field))
                    .unwrap_or((0, CType::Long(true)));
                let load_ty = if self.is_float_type(&field_ty) {
                    self.clif_float_type(&field_ty)
                } else {
                    self.clif_type(&field_ty)
                };
                // For aggregate fields, return the address
                if matches!(field_ty, CType::Struct(_) | CType::Union(_) | CType::Array(..)) {
                    if byte_offset != 0 {
                        return ctx.builder.ins().iadd_imm(base, byte_offset as i64);
                    }
                    return base;
                }
                ctx.builder.ins().load(load_ty, MemFlags::new(), base, byte_offset as i32)
            }

            Expr::Arrow(e, field) => {
                let ptr = self.compile_expr(ctx, e);
                let pointee_ty = self.expr_type(ctx, e)
                    .and_then(|ty| match ty {
                        CType::Pointer(inner) => Some(*inner),
                        _ => None,
                    });
                let (byte_offset, field_ty) = pointee_ty
                    .and_then(|ty| ty.field_offset(field))
                    .unwrap_or((0, CType::Long(true)));
                let load_ty = if self.is_float_type(&field_ty) {
                    self.clif_float_type(&field_ty)
                } else {
                    self.clif_type(&field_ty)
                };
                // For aggregate fields, return the address
                if matches!(field_ty, CType::Struct(_) | CType::Union(_) | CType::Array(..)) {
                    if byte_offset != 0 {
                        return ctx.builder.ins().iadd_imm(ptr, byte_offset as i64);
                    }
                    return ptr;
                }
                ctx.builder.ins().load(load_ty, MemFlags::new(), ptr, byte_offset as i32)
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

            Expr::VaArg(ap_expr, type_name) => {
                // va_arg(ap, type): load value at *ap, advance ap by 8
                let ap_val = self.compile_expr(ctx, ap_expr);
                let ty = self.resolve_typename(type_name);
                let load_ty = if self.is_float_type(&ty) {
                    self.clif_float_type(&ty)
                } else {
                    self.clif_type(&ty)
                };
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
                result
            }

            Expr::Offsetof(_tn, _fields) => {
                panic!("offsetof not yet implemented")
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
                        let (offset, _) = ty.field_offset(&field_name)
                            .unwrap_or_else(|| {
                                let field_names: Vec<_> = match &ty {
                                    CType::Struct(def) => def.fields.iter().map(|f| f.name.clone().unwrap_or("<anon>".into())).collect(),
                                    _ => vec!["<not a struct>".into()],
                                };
                                panic!("__builtin_offsetof: no field '{field_name}' in '{type_name}' (type has {} fields: {:?})", field_names.len(), &field_names[..field_names.len().min(10)])
                            });
                        ctx.builder.ins().iconst(I64, offset as i64)
                    }
                    "__builtin_expect" => {
                        // __builtin_expect(expr, expected) — just return expr
                        self.compile_expr(ctx, &args[0])
                    }
                    "__builtin_constant_p" => {
                        // Conservative: always return 0 (not a compile-time constant).
                        // This is correct per GCC semantics — 0 means "don't optimize as constant".
                        ctx.builder.ins().iconst(I32, 0)
                    }
                    "__builtin_choose_expr" => {
                        // __builtin_choose_expr(const_expr, expr1, expr2)
                        let val = crate::parse::Parser::eval_const_expr_static(&args[0])
                            .expect("__builtin_choose_expr: first argument must be a constant expression");
                        if val != 0 {
                            self.compile_expr(ctx, &args[1])
                        } else {
                            self.compile_expr(ctx, &args[2])
                        }
                    }
                    "__builtin_types_compatible_p" => {
                        eprintln!("warning: __builtin_types_compatible_p not yet implemented, will trap at runtime");
                        self.emit_trap_with_value(ctx, I32)
                    }
                    "__builtin_frame_address" | "__builtin_return_address" => {
                        eprintln!("warning: {name} not yet implemented, will trap at runtime");
                        self.emit_trap_with_value(ctx, I64)
                    }
                    "__builtin_unreachable" => {
                        self.emit_trap_with_value(ctx, I64)
                    }
                    "__builtin_va_end" => {
                        // va_end is a no-op
                        ctx.builder.ins().iconst(I64, 0)
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
                        va_addr
                    }
                    "__builtin_va_copy" => {
                        // va_copy(dest, src): copy the va_list pointer
                        let src_val = self.compile_expr(ctx, &args[1]);
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
                        src_val
                    }
                    "__builtin_va_arg" => {
                        // Same as Expr::VaArg but called as a builtin
                        let ap_val = self.compile_expr(ctx, &args[0]);
                        let result = ctx.builder.ins().load(I64, MemFlags::new(), ap_val, 0);
                        let new_ap = ctx.builder.ins().iadd_imm(ap_val, 8);
                        if let Expr::Ident(ap_name) = &args[0] {
                            if let Some((var, _)) = ctx.locals.get(ap_name) {
                                let var = *var;
                                ctx.builder.def_var(var, new_ap);
                            }
                        }
                        result
                    }
                    _ => panic!("builtin '{name}' not yet implemented"),
                }
            }
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

    fn expr_type(&mut self, ctx: &FuncCtx, expr: &Expr) -> Option<CType> {
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
                None
            }
            Expr::Arrow(e, field) => {
                let base_ty = self.expr_type(ctx, e)?;
                let pointee = match &base_ty {
                    CType::Pointer(inner) => inner.as_ref(),
                    _ => return None,
                };
                pointee.field_offset(field).map(|(_, ty)| ty)
            }
            Expr::Member(e, field) => {
                let base_ty = self.expr_type(ctx, e)?;
                base_ty.field_offset(field).map(|(_, ty)| ty)
            }
            Expr::Unary(UnaryOp::Deref, e) => {
                let ty = self.expr_type(ctx, e)?;
                match ty {
                    CType::Pointer(inner) => Some(*inner),
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
            Expr::Unary(UnaryOp::PreInc | UnaryOp::PreDec | UnaryOp::Neg | UnaryOp::BitNot, e) => {
                self.expr_type(ctx, e)
            }
            Expr::Sizeof(_) | Expr::Alignof(_) => Some(CType::Long(false)),
            Expr::StringLit(_) => Some(CType::Pointer(Box::new(CType::Char(true)))),
            Expr::IntLit(_) => Some(CType::Int(true)),
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
                self.compile_expr(ctx, e) // *p address is just p
            }
            Expr::Index(arr, idx) => {
                let elem_size = self.expr_type(ctx, arr)
                    .and_then(|ty| match ty {
                        CType::Pointer(inner) | CType::Array(inner, _) => Some(inner.size()),
                        _ => None,
                    })
                    .unwrap_or(8);
                let arr_val = self.compile_expr(ctx, arr);
                let idx_val = self.compile_expr(ctx, idx);
                let idx_val = self.coerce(ctx, idx_val, I64);
                let offset = ctx.builder.ins().imul_imm(idx_val, elem_size as i64);
                ctx.builder.ins().iadd(arr_val, offset)
            }
            Expr::Member(e, field) => {
                let base = self.compile_addr(ctx, e);
                let byte_offset = self.expr_type(ctx, e)
                    .and_then(|ty| ty.field_offset(field))
                    .map(|(off, _)| off)
                    .unwrap_or(0);
                if byte_offset != 0 {
                    ctx.builder.ins().iadd_imm(base, byte_offset as i64)
                } else {
                    base
                }
            }
            Expr::Arrow(e, field) => {
                let ptr = self.compile_expr(ctx, e);
                let byte_offset = self.expr_type(ctx, e)
                    .and_then(|ty| match ty {
                        CType::Pointer(inner) => Some(*inner),
                        _ => None,
                    })
                    .and_then(|ty| ty.field_offset(field))
                    .map(|(off, _)| off)
                    .unwrap_or(0);
                if byte_offset != 0 {
                    ctx.builder.ins().iadd_imm(ptr, byte_offset as i64)
                } else {
                    ptr
                }
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
                BinOp::Div => ctx.builder.ins().sdiv(l, r),
                BinOp::Mod => ctx.builder.ins().srem(l, r),
                BinOp::BitAnd => ctx.builder.ins().band(l, r),
                BinOp::BitOr => ctx.builder.ins().bor(l, r),
                BinOp::BitXor => ctx.builder.ins().bxor(l, r),
                BinOp::Shl => ctx.builder.ins().ishl(l, r),
                BinOp::Shr => ctx.builder.ins().sshr(l, r),
                BinOp::Eq => {
                    let c = ctx.builder.ins().icmp(IntCC::Equal, l, r);
                    Self::safe_uextend(ctx,cmp_result, c)
                }
                BinOp::Ne => {
                    let c = ctx.builder.ins().icmp(IntCC::NotEqual, l, r);
                    Self::safe_uextend(ctx,cmp_result, c)
                }
                BinOp::Lt => {
                    let c = ctx.builder.ins().icmp(IntCC::SignedLessThan, l, r);
                    Self::safe_uextend(ctx,cmp_result, c)
                }
                BinOp::Gt => {
                    let c = ctx.builder.ins().icmp(IntCC::SignedGreaterThan, l, r);
                    Self::safe_uextend(ctx,cmp_result, c)
                }
                BinOp::Le => {
                    let c = ctx.builder.ins().icmp(IntCC::SignedLessThanOrEqual, l, r);
                    Self::safe_uextend(ctx,cmp_result, c)
                }
                BinOp::Ge => {
                    let c = ctx.builder.ins().icmp(IntCC::SignedGreaterThanOrEqual, l, r);
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

    /// uextend that handles the no-op case where source and target types match
    fn safe_uextend(ctx: &mut FuncCtx, target: ir::Type, val: Value) -> Value {
        let val_type = ctx.builder.func.dfg.value_type(val);
        if val_type == target || val_type.bits() >= target.bits() { val }
        else { ctx.builder.ins().uextend(target, val) }
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

    fn init_global_data(&mut self, desc: &mut DataDescription, size: usize, ty: &CType, init: &Initializer) {
        let mut bytes = vec![0u8; size.max(1)];
        let mut relocs: Vec<GlobalReloc> = Vec::new();
        self.fill_init_item(&mut bytes, &mut relocs, 0, ty, init);
        desc.define(bytes.into_boxed_slice());
        for reloc in relocs {
            match reloc {
                GlobalReloc::FuncAddr { offset, func_id } => {
                    let func_ref = self.module.declare_func_in_data(func_id, desc);
                    desc.write_function_addr(offset, func_ref);
                }
                GlobalReloc::DataAddr { offset, data_id } => {
                    let gv = self.module.declare_data_in_data(data_id, desc);
                    desc.write_data_addr(offset, gv, 0);
                }
            }
        }
    }

    fn fill_init_item(&mut self, bytes: &mut [u8], relocs: &mut Vec<GlobalReloc>,
                      offset: usize, ty: &CType, init: &Initializer) {
        match init {
            Initializer::Expr(expr) => self.fill_init_scalar(bytes, relocs, offset, ty, expr),
            Initializer::List(items) => self.fill_init_list(bytes, relocs, offset, ty, items),
        }
    }

    fn fill_init_list(&mut self, bytes: &mut [u8], relocs: &mut Vec<GlobalReloc>,
                      base: usize, ty: &CType, items: &[InitializerItem]) {
        match ty {
            CType::Array(elem_ty, _) => {
                let elem_size = elem_ty.size();
                let mut idx = 0;
                for item in items {
                    if let Some(Designator::Index(expr)) = item.designators.first() {
                        if let Some(val) = self.eval_const_with_enums(expr) {
                            idx = val as usize;
                        }
                    }
                    let offset = base + idx * elem_size;
                    self.fill_init_item(bytes, relocs, offset, elem_ty, &item.initializer);
                    idx += 1;
                }
            }
            CType::Struct(def) => {
                let mut field_idx = 0;
                for item in items {
                    if let Some(Designator::Field(name)) = item.designators.first() {
                        if let Some(pos) = def.fields.iter().position(|f| f.name.as_deref() == Some(name.as_str())) {
                            field_idx = pos;
                        }
                    }
                    if field_idx >= def.fields.len() { break; }
                    let offset = base + Self::struct_field_offset(def, field_idx);
                    self.fill_init_item(bytes, relocs, offset, &def.fields[field_idx].ty, &item.initializer);
                    field_idx += 1;
                }
            }
            CType::Union(def) => {
                if let Some(item) = items.first() {
                    let fidx = if let Some(Designator::Field(name)) = item.designators.first() {
                        def.fields.iter().position(|f| f.name.as_deref() == Some(name.as_str())).unwrap_or(0)
                    } else { 0 };
                    if fidx < def.fields.len() {
                        self.fill_init_item(bytes, relocs, base, &def.fields[fidx].ty, &item.initializer);
                    }
                }
            }
            // Scalar wrapped in braces: e.g. int x = { 42 };
            _ => {
                if let Some(item) = items.first() {
                    self.fill_init_item(bytes, relocs, base, ty, &item.initializer);
                }
            }
        }
    }

    fn fill_init_scalar(&mut self, bytes: &mut [u8], relocs: &mut Vec<GlobalReloc>,
                        offset: usize, ty: &CType, expr: &Expr) {
        // Constant integer (including enum constants and uint literals)
        if let Some(val) = self.eval_const_with_enums(expr) {
            let field_size = ty.size();
            if offset + field_size > bytes.len() {
                // Field extends past buffer — skip (e.g. type mismatch or flexible array member)
                return;
            }
            let val_bytes = val.to_le_bytes();
            let copy_len = field_size.min(val_bytes.len());
            bytes[offset..offset + copy_len].copy_from_slice(&val_bytes[..copy_len]);
            return;
        }

        // String literal
        if let Expr::StringLit(data) = expr {
            if ty.is_pointer() {
                // Pointer to string: create data symbol + relocation
                let sym = format!(".str.{}", self.string_counter);
                self.string_counter += 1;
                let mut str_data = data.clone();
                str_data.push(0); // null terminator
                let data_id = self.module.declare_data(&sym, Linkage::Local, false, false).unwrap();
                self.strings.push((sym, str_data));
                relocs.push(GlobalReloc::DataAddr { offset: offset as u32, data_id });
            } else {
                // Char array: copy bytes directly
                let copy_len = ty.size().min(data.len());
                bytes[offset..offset + copy_len].copy_from_slice(&data[..copy_len]);
            }
            return;
        }

        // Function or data pointer
        if let Expr::Ident(ref_name) = expr {
            if let Some(&func_id) = self.func_ids.get(ref_name) {
                relocs.push(GlobalReloc::FuncAddr { offset: offset as u32, func_id });
                return;
            }
            if let Some(&data_id) = self.data_ids.get(ref_name) {
                relocs.push(GlobalReloc::DataAddr { offset: offset as u32, data_id });
                return;
            }
        }

        // Cast wrapping an identifier (e.g. `(void*)func`)
        if let Expr::Cast(_, inner) = expr {
            if let Expr::Ident(ref_name) = inner.as_ref() {
                if let Some(&func_id) = self.func_ids.get(ref_name) {
                    relocs.push(GlobalReloc::FuncAddr { offset: offset as u32, func_id });
                    return;
                }
                if let Some(&data_id) = self.data_ids.get(ref_name) {
                    relocs.push(GlobalReloc::DataAddr { offset: offset as u32, data_id });
                    return;
                }
            }
        }

        // Fallback: leave as zero
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

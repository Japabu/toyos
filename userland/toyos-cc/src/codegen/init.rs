use super::*;

/// Returns the final byte representation of a string literal (with null terminator).
/// For narrow strings: raw bytes + 0x00.
/// For wide strings: each codepoint as 4 LE bytes + 4 zero bytes.
pub(crate) fn string_lit_bytes(expr: &Expr) -> Vec<u8> {
    match expr {
        Expr::StringLit(data) => {
            let mut bytes = data.clone();
            bytes.push(0);
            bytes
        }
        Expr::WideStringLit(data) => {
            let mut bytes = Vec::new();
            for c in std::str::from_utf8(data).unwrap().chars() {
                bytes.extend_from_slice(&(c as i32).to_le_bytes());
            }
            bytes.extend_from_slice(&0i32.to_le_bytes());
            bytes
        }
        _ => unreachable!(),
    }
}

/// Returns the number of elements (including null terminator) for array sizing.
pub(crate) fn string_lit_elem_count(expr: &Expr) -> usize {
    match expr {
        Expr::StringLit(data) => data.len() + 1,
        Expr::WideStringLit(data) => std::str::from_utf8(data).unwrap().chars().count() + 1,
        _ => unreachable!(),
    }
}

impl Codegen {
    /// Compute the byte offset of field at `target_idx` in a struct.
    /// Returns (byte_offset, bit_offset) for a field by index.
    /// For non-bitfield fields, bit_offset is 0.
    fn struct_field_offset(def: &StructDef, target_idx: usize) -> (usize, u32) {
        let mut offset = 0usize;
        let mut bit_pos = 0u32;
        let mut bit_unit_size = 0usize;
        for (i, field) in def.fields.iter().enumerate() {
            if let Some(bw) = field.bit_width {
                let unit_size = field.ty.size();
                let unit_bits = (unit_size * 8) as u32;
                let align = field.ty.align();
                if bw == 0 {
                    if bit_unit_size > 0 {
                        offset += bit_unit_size;
                        bit_pos = 0;
                        bit_unit_size = 0;
                    }
                    offset = (offset + align - 1) & !(align - 1);
                } else if bit_unit_size == unit_size && bit_pos + bw <= unit_bits {
                    // Fits in current unit
                    if i == target_idx { return (offset, bit_pos); }
                    bit_pos += bw;
                } else {
                    // New storage unit
                    offset += bit_unit_size;
                    offset = (offset + align - 1) & !(align - 1);
                    bit_unit_size = unit_size;
                    if i == target_idx { return (offset, 0); }
                    bit_pos = bw;
                }
                continue;
            }
            // Flush bitfield unit
            offset += bit_unit_size;
            bit_pos = 0;
            bit_unit_size = 0;

            let align = field.ty.align();
            offset = (offset + align - 1) & !(align - 1);
            if i == target_idx { return (offset, 0); }
            offset += field.ty.size();
        }
        (offset, 0)
    }

    pub(crate) fn compile_aggregate_init(&mut self, ctx: &mut FuncCtx, ptr: Value, ty: &CType, init: &Initializer) {
        // Zero the memory first
        let size = ty.size();
        if size > 0 {
            let zero = ctx.builder.ins().iconst(I8, 0);
            if size <= 64 {
                for i in 0..size {
                    ctx.builder.ins().store(MemFlags::new(), zero, ptr, i as i32);
                }
            } else {
                // Use memset for larger blocks
                let size_val = ctx.builder.ins().iconst(I64, size as i64);
                self.call_memset(ctx, ptr, zero, size_val);
            }
        }
        // Apply initializer values
        match init {
            Initializer::List(items) => {
                self.compile_aggregate_init_list(ctx, ptr, ty, items);
            }
            // Compound literal: init directly into target memory (avoids intermediate stack slot)
            Initializer::Expr(Expr::CompoundLiteral(_, items)) => {
                self.compile_aggregate_init_list(ctx, ptr, ty, items);
            }
            Initializer::Expr(e @ (Expr::StringLit(_) | Expr::WideStringLit(_))) => {
                let data = string_lit_bytes(e);
                for (i, &byte) in data.iter().enumerate() {
                    let val = ctx.builder.ins().iconst(I8, byte as i64);
                    ctx.builder.ins().store(MemFlags::new(), val, ptr, i as i32);
                }
            }
            Initializer::Expr(e) => {
                // Single expression for whole aggregate (e.g., struct copy)
                let tv = self.compile_expr(ctx, e);
                if matches!(ty, CType::Struct(_) | CType::Union(_)) {
                    // val is an address; copy the struct data
                    let size_val = ctx.builder.ins().iconst(I64, ty.size() as i64);
                    self.emit_memcpy(ctx, ptr, tv.raw(), size_val);
                } else {
                    let target_ty = self.clif_type(ty);
                    let val = self.coerce_typed(ctx, tv, target_ty);
                    ctx.builder.ins().store(MemFlags::new(), val, ptr, 0);
                }
            }
        }
    }

    fn compile_aggregate_init_list(&mut self, ctx: &mut FuncCtx, base_ptr: Value, ty: &CType, items: &[InitializerItem]) {
        let mut cursor = 0;
        self.compile_aggregate_init_cursor(ctx, base_ptr, ty, items, &mut cursor);
    }

    /// Cursor-based local aggregate initialization with brace elision support.
    fn compile_aggregate_init_cursor(&mut self, ctx: &mut FuncCtx, base_ptr: Value, ty: &CType,
                                     items: &[InitializerItem], cursor: &mut usize) {
        match ty {
            CType::Array(elem_ty, size) => {
                let elem_ty = elem_ty.as_ref().clone();
                let elem_size = elem_ty.size();
                let max_elems = size.unwrap_or(usize::MAX);
                let mut idx = 0;
                while *cursor < items.len() && idx < max_elems {
                    let offset = idx * elem_size;
                    let elem_ptr = if offset == 0 { base_ptr }
                    else { ctx.builder.ins().iadd_imm(base_ptr, offset as i64) };
                    self.compile_init_one(ctx, elem_ptr, &elem_ty, items, cursor);
                    idx += 1;
                }
            }
            CType::Struct(def) => {
                let fields = def.fields.clone();
                let mut field_idx = 0;
                while *cursor < items.len() {
                    // Skip anonymous bitfield members
                    while field_idx < fields.len() && fields[field_idx].name.is_none() && fields[field_idx].bit_width.is_some() {
                        field_idx += 1;
                    }
                    // Handle designators (can reset field_idx to earlier fields)
                    if let Some(Designator::Field(name)) = items[*cursor].designators.first() {
                        if let Some(pos) = fields.iter().position(|f| f.name.as_deref() == Some(name.as_str())) {
                            field_idx = pos;
                        }
                        // Handle sub-member designators like .a.j = 5
                        if items[*cursor].designators.len() > 1 {
                            let fname = fields[field_idx].name.as_deref().expect("designator on anonymous field");
                            let fi = ty.field_offset(fname)
                                .unwrap_or_else(|| panic!("init: no field '{fname}' in {ty:?}"));
                            let mut off = fi.byte_offset;
                            let mut sub_ty = fields[field_idx].ty.clone();
                            for d in &items[*cursor].designators[1..] {
                                match d {
                                    Designator::Field(sub_name) => {
                                        let fi = sub_ty.field_offset(sub_name).unwrap();
                                        off += fi.byte_offset;
                                        sub_ty = fi.ty;
                                    }
                                    Designator::Index(idx_expr) => {
                                        let idx = self.eval_const(idx_expr).unwrap() as usize;
                                        if let CType::Array(elem, _) = &sub_ty {
                                            off += idx * elem.size();
                                            sub_ty = elem.as_ref().clone();
                                        }
                                    }
                                    Designator::IndexRange(..) => panic!("IndexRange designator not supported in sub-member init"),
                                }
                            }
                            let sub_ptr = if off == 0 { base_ptr }
                            else { ctx.builder.ins().iadd_imm(base_ptr, off as i64) };
                            // Compile the value and store it
                            match &items[*cursor].initializer {
                                Initializer::Expr(e) => {
                                    let tv = self.compile_expr(ctx, e);
                                    let target_ty = self.clif_type(&sub_ty);
                                    let val = self.coerce_typed(ctx, tv, target_ty);
                                    ctx.builder.ins().store(MemFlags::new(), val, sub_ptr, 0);
                                }
                                Initializer::List(sub_items) => {
                                    self.compile_aggregate_init_list(ctx, sub_ptr, &sub_ty, sub_items);
                                }
                            }
                            *cursor += 1;
                            field_idx += 1;
                            continue;
                        }
                    }
                    if field_idx >= fields.len() { break; }
                    let fname = fields[field_idx].name.as_deref().expect("init: unnamed field in struct init");
                    let fi = ty.field_offset(fname)
                        .unwrap_or_else(|| panic!("init: no field '{fname}' in {ty:?}"));
                    let field_ptr = if fi.byte_offset == 0 { base_ptr }
                    else { ctx.builder.ins().iadd_imm(base_ptr, fi.byte_offset as i64) };
                    let field_ty = fields[field_idx].ty.clone();
                    self.compile_init_one(ctx, field_ptr, &field_ty, items, cursor);
                    field_idx += 1;
                }
            }
            CType::Union(def) => {
                let def = def.clone();
                if *cursor < items.len() {
                    let fidx = if let Some(Designator::Field(name)) = items[*cursor].designators.first() {
                        def.fields.iter().position(|f| f.name.as_deref() == Some(name.as_str()))
                            .unwrap_or_else(|| panic!("init: no field '{name}' in union"))
                    } else { 0 };
                    let field_ty = def.fields[fidx].ty.clone();
                    self.compile_init_one(ctx, base_ptr, &field_ty, items, cursor);
                }
            }
            // Scalar (int, float, pointer, enum, bool) — store single value
            _ => {
                if *cursor < items.len() {
                    if let Initializer::Expr(e) = &items[*cursor].initializer {
                        let tv = self.compile_expr(ctx, e);
                        let target_ty = self.clif_type(ty);
                        let val = self.coerce_typed(ctx, tv, target_ty);
                        ctx.builder.ins().store(MemFlags::new(), val, base_ptr, 0);
                    }
                    *cursor += 1;
                }
            }
        }
    }

    /// Fill one element from a flat init list at runtime, handling brace elision.
    fn compile_init_one(&mut self, ctx: &mut FuncCtx, ptr: Value, ty: &CType,
                        items: &[InitializerItem], cursor: &mut usize) {
        if *cursor >= items.len() { return; }
        match &items[*cursor].initializer {
            Initializer::List(_) => {
                let init = items[*cursor].initializer.clone();
                *cursor += 1;
                if let Initializer::List(sub_items) = &init {
                    self.compile_aggregate_init_list(ctx, ptr, ty, sub_items);
                }
            }
            Initializer::Expr(e) => {
                // String literals are whole-value for array targets (copy bytes),
                // but for struct targets they should brace-elide into the first array field.
                let is_string_for_array = matches!(e, Expr::StringLit(_) | Expr::WideStringLit(_)) && matches!(ty, CType::Array(..));
                let is_compound_literal = matches!(e, Expr::CompoundLiteral(..));
                let expr_is_aggregate = is_compound_literal || {
                    let ety = self.expr_type(ctx, e);
                    matches!(ety, CType::Struct(_) | CType::Union(_))
                };
                match ty {
                    _ if is_string_for_array => {
                        let data = string_lit_bytes(e);
                        for (i, &byte) in data.iter().enumerate() {
                            let val = ctx.builder.ins().iconst(I8, byte as i64);
                            ctx.builder.ins().store(MemFlags::new(), val, ptr, i as i32);
                        }
                        *cursor += 1;
                    }
                    _ if is_compound_literal && matches!(ty, CType::Struct(_) | CType::Union(_) | CType::Array(..)) => {
                        // Compound literal: init directly into target (avoids intermediate stack slot)
                        if let Expr::CompoundLiteral(_, cl_items) = e {
                            self.compile_aggregate_init_list(ctx, ptr, ty, cl_items);
                        }
                        *cursor += 1;
                    }
                    CType::Struct(_) | CType::Union(_) if expr_is_aggregate => {
                        // Whole-value struct init (struct copy, function return, etc.)
                        let val = self.compile_expr(ctx, e).raw();
                        let size_val = ctx.builder.ins().iconst(I64, ty.size() as i64);
                        self.emit_memcpy(ctx, ptr, val, size_val);
                        *cursor += 1;
                    }
                    CType::Array(..) | CType::Struct(_) | CType::Union(_) => {
                        // Brace elision: recursively fill sub-aggregate from flat list
                        self.compile_aggregate_init_cursor(ctx, ptr, ty, items, cursor);
                    }
                    // Scalar (int, float, pointer, enum, bool)
                    _ => {
                        let tv = self.compile_expr(ctx, e);
                        let target_ty = self.clif_type(ty);
                        let val = self.coerce_typed(ctx, tv, target_ty);
                        ctx.builder.ins().store(MemFlags::new(), val, ptr, 0);
                        *cursor += 1;
                    }
                }
            }
        }
    }

    fn call_memset(&mut self, ctx: &mut FuncCtx, ptr: Value, val: Value, size: Value) {
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(I64));
        sig.params.push(AbiParam::new(I32));
        sig.params.push(AbiParam::new(I64));
        sig.returns.push(AbiParam::new(I64));
        let func_id = self.module.declare_function("memset", Linkage::Import, &sig).unwrap_or_else(|_| {
            if let Some(FuncOrDataId::Func(id)) = self.module.get_name("memset") { id }
            else { panic!("cannot declare memset") }
        });
        let func_ref = self.module.declare_func_in_func(func_id, ctx.builder.func);
        let val32 = self.coerce(ctx, val, I32);
        ctx.builder.ins().call(func_ref, &[ptr, val32, size]);
    }

    pub(crate) fn emit_memcpy(&mut self, ctx: &mut FuncCtx, dst: Value, src: Value, size: Value) {
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(I64));
        sig.params.push(AbiParam::new(I64));
        sig.params.push(AbiParam::new(I64));
        sig.returns.push(AbiParam::new(I64));
        let func_id = self.module.declare_function("memcpy", Linkage::Import, &sig).unwrap_or_else(|_| {
            if let Some(FuncOrDataId::Func(id)) = self.module.get_name("memcpy") { id }
            else { panic!("cannot declare memcpy") }
        });
        let func_ref = self.module.declare_func_in_func(func_id, ctx.builder.func);
        ctx.builder.ins().call(func_ref, &[dst, src, size]);
    }

    pub(crate) fn count_initializer_elements(&self, init: &Initializer, _elem_ty: &CType) -> usize {
        match init {
            Initializer::List(items) => {
                let mut max_idx = items.len();
                for item in items {
                    for d in &item.designators {
                        if let Designator::Index(e) = d {
                            if let Some(v) = crate::ast::eval_const_expr(e, Some(&self.type_env.enum_constants)) {
                                max_idx = max_idx.max(v as usize + 1);
                            }
                        }
                    }
                }
                max_idx
            }
            Initializer::Expr(e @ (Expr::StringLit(_) | Expr::WideStringLit(_))) => string_lit_elem_count(e),
            Initializer::Expr(_) => 1,
        }
    }

    /// Compute the required allocation size for a type+initializer pair,
    /// accounting for flexible array members in structs.
    pub(crate) fn init_size(ty: &CType, init: &Initializer) -> usize {
        if let CType::Struct(def) = ty {
            if let Some(last_field) = def.fields.last() {
                if let CType::Array(elem, None) = &last_field.ty {
                    // Struct has a flexible array member — count elements from initializer
                    if let Initializer::List(items) = init {
                        let non_flex = def.fields.len() - 1;
                        if items.len() > non_flex {
                            let flex_init = &items[non_flex].initializer;
                            let flex_count = match flex_init {
                                Initializer::List(sub) => sub.len(),
                                Initializer::Expr(_) => 1,
                            };
                            return ty.size() + flex_count * elem.size();
                        }
                    }
                }
            }
        }
        ty.size()
    }

    pub(crate) fn init_global_data(&mut self, desc: &mut DataDescription, size: usize, ty: &CType, init: &Initializer) {
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
        let mut cursor = 0;
        self.fill_init_aggregate(bytes, relocs, base, ty, items, &mut cursor);
    }

    /// Cursor-based aggregate initialization that supports brace elision.
    fn fill_init_aggregate(&mut self, bytes: &mut [u8], relocs: &mut Vec<GlobalReloc>,
                           base: usize, ty: &CType, items: &[InitializerItem], cursor: &mut usize) {
        match ty {
            CType::Array(elem_ty, size) => {
                let elem_ty = elem_ty.as_ref().clone();
                let elem_size = elem_ty.size();
                let max_elems = size.unwrap_or(usize::MAX);
                let mut idx = 0;
                while *cursor < items.len() && idx < max_elems {
                    if let Some(Designator::Index(expr)) = items[*cursor].designators.first() {
                        if let Some(val) = self.eval_const(expr) {
                            idx = val as usize;
                        }
                    }
                    let offset = base + idx * elem_size;
                    self.fill_init_one(bytes, relocs, offset, &elem_ty, items, cursor);
                    idx += 1;
                }
            }
            CType::Struct(def) => {
                let fields = def.fields.clone();
                let def = def.clone();
                let mut field_idx = 0;
                while *cursor < items.len() {
                    // Skip anonymous bitfield members
                    while field_idx < fields.len() && fields[field_idx].name.is_none() && fields[field_idx].bit_width.is_some() {
                        field_idx += 1;
                    }
                    // Handle designators (can reset field_idx to earlier fields)
                    if let Some(Designator::Field(name)) = items[*cursor].designators.first() {
                        if let Some(pos) = fields.iter().position(|f| f.name.as_deref() == Some(name.as_str())) {
                            field_idx = pos;
                        }
                        // Handle sub-member designators like .a.j = 5
                        if items[*cursor].designators.len() > 1 {
                            let (fo, _) = Self::struct_field_offset(&def, field_idx);
                            let mut off = base + fo;
                            let mut sub_ty = fields[field_idx].ty.clone();
                            for d in &items[*cursor].designators[1..] {
                                match d {
                                    Designator::Field(sub_name) => {
                                        let fi = sub_ty.field_offset(sub_name).unwrap();
                                        off += fi.byte_offset;
                                        sub_ty = fi.ty;
                                    }
                                    Designator::Index(idx_expr) => {
                                        let idx = self.eval_const(idx_expr).unwrap() as usize;
                                        if let CType::Array(elem, _) = &sub_ty {
                                            off += idx * elem.size();
                                            sub_ty = elem.as_ref().clone();
                                        }
                                    }
                                    Designator::IndexRange(..) => panic!("IndexRange designator not supported in sub-member init"),
                                }
                            }
                            self.fill_init_item(bytes, relocs, off, &sub_ty, &items[*cursor].initializer);
                            *cursor += 1;
                            field_idx += 1;
                            continue;
                        }
                    }
                    if field_idx >= fields.len() { break; }
                    let (fo, bit_off) = Self::struct_field_offset(&def, field_idx);
                    let offset = base + fo;
                    let field = &fields[field_idx];
                    if let Some(bw) = field.bit_width {
                        // Bitfield: pack value into storage unit at bit_off
                        if let Initializer::Expr(expr) = &items[*cursor].initializer {
                            if let Some(val) = self.eval_const(expr) {
                                let mask = (1u64 << bw) - 1;
                                let shifted = (val as u64 & mask) << bit_off;
                                let unit_size = field.ty.size();
                                for b in 0..unit_size {
                                    bytes[offset + b] |= ((shifted >> (b * 8)) & 0xff) as u8;
                                }
                            }
                        }
                        *cursor += 1;
                    } else {
                        let field_ty = field.ty.clone();
                        self.fill_init_one(bytes, relocs, offset, &field_ty, items, cursor);
                    }
                    field_idx += 1;
                }
            }
            CType::Union(def) => {
                let def = def.clone();
                if *cursor < items.len() {
                    let fidx = if let Some(Designator::Field(name)) = items[*cursor].designators.first() {
                        def.fields.iter().position(|f| f.name.as_deref() == Some(name.as_str()))
                            .unwrap_or_else(|| panic!("init: no field '{name}' in union"))
                    } else { 0 };
                    let field_ty = def.fields[fidx].ty.clone();
                    self.fill_init_one(bytes, relocs, base, &field_ty, items, cursor);
                }
            }
            // Scalar (int, float, pointer, enum, bool) — fill single value
            _ => {
                if *cursor < items.len() {
                    self.fill_init_item(bytes, relocs, base, ty, &items[*cursor].initializer);
                    *cursor += 1;
                }
            }
        }
    }

    /// Fill one element from a flat initializer list, handling brace elision.
    fn fill_init_one(&mut self, bytes: &mut [u8], relocs: &mut Vec<GlobalReloc>,
                     offset: usize, ty: &CType, items: &[InitializerItem], cursor: &mut usize) {
        if *cursor >= items.len() { return; }
        match &items[*cursor].initializer {
            Initializer::List(_) => {
                // Braced sub-initializer: use it entirely for this element
                let init = items[*cursor].initializer.clone();
                *cursor += 1;
                self.fill_init_item(bytes, relocs, offset, ty, &init);
            }
            Initializer::Expr(e) => {
                // String literals are whole-value only for array/pointer targets;
                // for struct targets they should brace-elide into the first array field.
                // Compound literals are always whole-value.
                let is_whole_value = match e {
                    Expr::StringLit(_) | Expr::WideStringLit(_) => matches!(ty, CType::Array(..)) || ty.is_pointer(),
                    Expr::CompoundLiteral(..) => true,
                    // Other expressions are not whole-value initializers
                    _ => false,
                };
                match ty {
                    CType::Array(..) | CType::Struct(_) | CType::Union(_) if !is_whole_value => {
                        // Brace elision: recursively fill sub-aggregate from the flat list
                        self.fill_init_aggregate(bytes, relocs, offset, ty, items, cursor);
                    }
                    // Scalar, or whole-value aggregate — fill single item
                    _ => {
                        self.fill_init_item(bytes, relocs, offset, ty, &items[*cursor].initializer);
                        *cursor += 1;
                    }
                }
            }
        }
    }

    fn eval_const_float(&self, expr: &Expr) -> Option<f64> {
        match expr {
            Expr::FloatLit(v, _) => Some(*v),
            Expr::IntLit(v) => Some(*v as f64),
            Expr::UIntLit(v) => Some(*v as f64),
            Expr::CharLit(v) => Some(*v as f64),
            Expr::Unary(UnaryOp::Neg, e) => self.eval_const_float(e).map(|v| -v),
            Expr::Unary(UnaryOp::LogNot, e) => self.eval_const_float(e).map(|v| if v == 0.0 { 1.0 } else { 0.0 }),
            Expr::Cast(_, e) => self.eval_const_float(e),
            Expr::Binary(op, l, r) => {
                let l = self.eval_const_float(l)?;
                let r = self.eval_const_float(r)?;
                Some(match op {
                    BinOp::Add => l + r,
                    BinOp::Sub => l - r,
                    BinOp::Mul => l * r,
                    BinOp::Div => if r != 0.0 { l / r } else { 0.0 },
                    BinOp::Eq => (l == r) as i64 as f64,
                    BinOp::Ne => (l != r) as i64 as f64,
                    BinOp::Lt => (l < r) as i64 as f64,
                    BinOp::Gt => (l > r) as i64 as f64,
                    BinOp::Le => (l <= r) as i64 as f64,
                    BinOp::Ge => (l >= r) as i64 as f64,
                    BinOp::LogAnd => ((l != 0.0) && (r != 0.0)) as i64 as f64,
                    BinOp::LogOr => ((l != 0.0) || (r != 0.0)) as i64 as f64,
                    // Bitwise/shift/mod ops are integer-only, not applicable to float const eval
                    _ => return None,
                })
            }
            Expr::Conditional(c, t, f) => {
                let c = self.eval_const_float(c)?;
                if c != 0.0 { self.eval_const_float(t) } else { self.eval_const_float(f) }
            }
            // Non-constant expressions (Call, Index, Member, etc.) can't be evaluated at compile time
            _ => None,
        }
    }

    fn fill_init_scalar(&mut self, bytes: &mut [u8], relocs: &mut Vec<GlobalReloc>,
                        offset: usize, ty: &CType, expr: &Expr) {
        // Compound literal: treat as aggregate initializer
        if let Expr::CompoundLiteral(_, items) = expr {
            self.fill_init_list(bytes, relocs, offset, ty, items);
            return;
        }

        // Float target type: try float evaluation first (handles int-to-float conversion)
        if ty.is_float() {
            if let Some(val) = self.eval_const_float(expr) {
                let field_size = ty.size();
                assert!(offset + field_size <= bytes.len(), "initializer overflow: offset {offset} + size {field_size} exceeds {} bytes", bytes.len());
                match ty {
                    CType::Float => {
                        let bits = (val as f32).to_le_bytes();
                        bytes[offset..offset + 4].copy_from_slice(&bits);
                    }
                    // Guard `ty.is_float()` ensures only Float/Double/LongDouble reach here
                    CType::Double | CType::LongDouble => {
                        let bits = val.to_le_bytes();
                        bytes[offset..offset + 8].copy_from_slice(&bits);
                        if field_size > 8 {
                            for b in &mut bytes[offset + 8..offset + field_size] { *b = 0; }
                        }
                    }
                    _ => unreachable!(),
                }
                return;
            }
        }

        // Constant integer (including enum constants and uint literals)
        if let Some(val) = self.eval_const(expr) {
            let field_size = ty.size();
            assert!(offset + field_size <= bytes.len(), "initializer overflow: offset {offset} + size {field_size} exceeds {} bytes", bytes.len());
            let val_bytes = val.to_le_bytes();
            let copy_len = field_size.min(val_bytes.len());
            bytes[offset..offset + copy_len].copy_from_slice(&val_bytes[..copy_len]);
            return;
        }

        // Float constant expression
        if let Some(val) = self.eval_const_float(expr) {
            let field_size = ty.size();
            if offset + field_size > bytes.len() { return; }
            match ty {
                CType::Float => {
                    let bits = (val as f32).to_le_bytes();
                    bytes[offset..offset + 4].copy_from_slice(&bits);
                }
                CType::Double | CType::LongDouble => {
                    let bits = val.to_le_bytes();
                    bytes[offset..offset + 8].copy_from_slice(&bits);
                    if field_size > 8 {
                        for b in &mut bytes[offset + 8..offset + field_size] { *b = 0; }
                    }
                }
                // Float constant assigned to integer type: C truncation semantics
                _ => {
                    let ival = val as i64;
                    let val_bytes = ival.to_le_bytes();
                    let copy_len = field_size.min(val_bytes.len());
                    bytes[offset..offset + copy_len].copy_from_slice(&val_bytes[..copy_len]);
                }
            }
            return;
        }

        // String literal (narrow or wide)
        if matches!(expr, Expr::StringLit(_) | Expr::WideStringLit(_)) {
            let str_bytes = string_lit_bytes(expr);
            if ty.is_pointer() {
                let sym = format!(".str.{}", self.string_counter);
                self.string_counter += 1;
                let data_id = self.module.declare_data(&sym, Linkage::Local, false, false).unwrap();
                self.strings.push((sym, str_bytes));
                relocs.push(GlobalReloc::DataAddr { offset: offset as u32, data_id });
            } else {
                let copy_len = ty.size().min(str_bytes.len());
                bytes[offset..offset + copy_len].copy_from_slice(&str_bytes[..copy_len]);
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

        // &func or &data
        if let Expr::Unary(UnaryOp::AddrOf, inner) = expr {
            if let Expr::Ident(ref_name) = inner.as_ref() {
                if let Some(&func_id) = self.func_ids.get(ref_name) {
                    relocs.push(GlobalReloc::FuncAddr { offset: offset as u32, func_id });
                    return;
                }
                if let Some(&data_id) = self.data_ids.get(ref_name) {
                    relocs.push(GlobalReloc::DataAddr { offset: offset as u32, data_id });
                    return;
                }
                // Unknown function: declare as import
                let mut sig = self.module.make_signature();
                sig.returns.push(AbiParam::new(I64));
                let func_id = self.module.declare_function(ref_name, Linkage::Import, &sig)
                    .unwrap_or_else(|e| panic!("failed to declare function '{ref_name}' in global init: {e}"));
                self.func_ids.insert(ref_name.clone(), func_id);
                relocs.push(GlobalReloc::FuncAddr { offset: offset as u32, func_id });
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

        // No handler matched
        panic!("fill_init_scalar: unhandled global initializer expression: {expr:?} for type {ty:?}");
    }

    pub(crate) fn define_strings(&mut self) {
        for (sym, data) in std::mem::take(&mut self.strings) {
            let data_id = self.module.declare_data(&sym, Linkage::Local, false, false).unwrap();
            let mut desc = DataDescription::new();
            desc.define(data.into_boxed_slice());
            self.module.define_data(data_id, &desc).unwrap();
        }
    }
}

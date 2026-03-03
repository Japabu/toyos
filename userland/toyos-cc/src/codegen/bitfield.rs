use super::*;

impl Codegen {
    /// Extract a bitfield value: shift right by bit_offset, mask to bit_width.
    /// For non-bitfield fields (bw == None), returns val unchanged.
    pub(super) fn extract_bitfield(&self, ctx: &mut FuncCtx, val: Value, bit_offset: u32, bw: Option<u32>, field_ty: &CType) -> Value {
        let bw = match bw {
            Some(w) if w > 0 => w,
            None | Some(0) => return val,
            Some(_) => unreachable!("w > 0 handled above"),
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
    pub(super) fn field_storage_type(&mut self, ctx: &FuncCtx, expr: &Expr) -> Option<CType> {
        match expr {
            Expr::Member(e, field) => {
                let base_ty = self.expr_type(ctx, e);
                let base_ty = self.resolve_incomplete_type(base_ty);
                base_ty.field_offset(field).map(|fi| fi.ty)
            }
            Expr::Arrow(e, field) => {
                let ptr_ty = self.expr_type(ctx, e);
                let pointee = match ptr_ty {
                    CType::Pointer(inner) => *inner,
                    CType::Void | CType::Bool | CType::Char(_) | CType::Short(_) | CType::Int(_)
                    | CType::Long(_) | CType::LongLong(_) | CType::Int128(_) | CType::Float
                    | CType::Double | CType::LongDouble | CType::Array(..) | CType::Enum(_)
                    | CType::Function(..) | CType::Struct(_) | CType::Union(_) => return None,
                };
                let pointee = self.resolve_incomplete_type(pointee);
                pointee.field_offset(field).map(|fi| fi.ty)
            }
            Expr::IntLit(_) | Expr::UIntLit(_) | Expr::FloatLit(..) | Expr::CharLit(_)
            | Expr::StringLit(_) | Expr::WideStringLit(_) | Expr::Ident(_)
            | Expr::Binary(..) | Expr::Unary(..) | Expr::PostUnary(..) | Expr::Cast(..)
            | Expr::Sizeof(_) | Expr::Alignof(_) | Expr::Conditional(..)
            | Expr::Call(..) | Expr::Index(..) | Expr::Assign(..) | Expr::Comma(..)
            | Expr::CompoundLiteral(..) | Expr::StmtExpr(_) | Expr::VaArg(..)
            | Expr::Builtin(..) => None,
        }
    }

    /// Check if an expression is a bitfield member access.
    /// Returns (bit_offset, bit_width, storage_type) if it is.
    pub(super) fn bitfield_info(&mut self, ctx: &FuncCtx, expr: &Expr) -> Option<(u32, u32, CType)> {
        let fi = match expr {
            Expr::Member(e, field) => {
                let base_ty = self.expr_type(ctx, e);
                let base_ty = self.resolve_incomplete_type(base_ty);
                base_ty.field_offset(field)?
            }
            Expr::Arrow(e, field) => {
                let ptr_ty = self.expr_type(ctx, e);
                let pointee_ty = match ptr_ty {
                    CType::Pointer(inner) => *inner,
                    CType::Void | CType::Bool | CType::Char(_) | CType::Short(_) | CType::Int(_)
                    | CType::Long(_) | CType::LongLong(_) | CType::Int128(_) | CType::Float
                    | CType::Double | CType::LongDouble | CType::Array(..) | CType::Enum(_)
                    | CType::Function(..) | CType::Struct(_) | CType::Union(_) => return None,
                };
                let pointee_ty = self.resolve_incomplete_type(pointee_ty);
                pointee_ty.field_offset(field)?
            }
            Expr::IntLit(_) | Expr::UIntLit(_) | Expr::FloatLit(..) | Expr::CharLit(_)
            | Expr::StringLit(_) | Expr::WideStringLit(_) | Expr::Ident(_)
            | Expr::Binary(..) | Expr::Unary(..) | Expr::PostUnary(..) | Expr::Cast(..)
            | Expr::Sizeof(_) | Expr::Alignof(_) | Expr::Conditional(..)
            | Expr::Call(..) | Expr::Index(..) | Expr::Assign(..) | Expr::Comma(..)
            | Expr::CompoundLiteral(..) | Expr::StmtExpr(_) | Expr::VaArg(..)
            | Expr::Builtin(..) => return None,
        };
        let bw = fi.bit_width.filter(|&w| w > 0)?;
        Some((fi.bit_offset, bw, fi.ty))
    }

    /// Store a value into a bitfield using read-modify-write.
    pub(super) fn store_bitfield(&self, ctx: &mut FuncCtx, addr: Value, new_val: Value, bit_offset: u32, bw: u32, storage_ty: &CType) -> Value {
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
}

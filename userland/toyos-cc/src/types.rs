use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signedness {
    Signed,
    Unsigned,
}

impl Signedness {
    pub fn is_unsigned(self) -> bool { self == Signedness::Unsigned }
    pub fn is_signed(self) -> bool { self == Signedness::Signed }
}

impl From<bool> for Signedness {
    /// Convert from the old convention: `true` = signed, `false` = unsigned.
    fn from(signed: bool) -> Self {
        if signed { Signedness::Signed } else { Signedness::Unsigned }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CType {
    Void,
    Bool,
    Char(Signedness),
    Short(Signedness),
    Int(Signedness),
    Long(Signedness),
    LongLong(Signedness),
    Int128(Signedness),
    Float,
    Double,
    LongDouble,
    Pointer(Box<CType>),
    Array(Box<CType>, Option<usize>), // element type, optional size
    Function(Box<CType>, Vec<ParamType>, bool), // return type, params, variadic
    Struct(StructDef),
    Union(StructDef),
    Enum(EnumDef),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParamType {
    pub name: Option<String>,
    pub ty: CType,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StructDef {
    pub name: Option<String>,
    pub fields: Vec<FieldDef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldDef {
    pub name: Option<String>,
    pub ty: CType,
    pub bit_width: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumDef {
    pub name: Option<String>,
    pub variants: Vec<(String, i64)>,
}

impl CType {
    pub fn size(&self) -> usize {
        match self {
            CType::Void => 0,
            CType::Bool => 1,
            CType::Char(_) => 1,
            CType::Short(_) => 2,
            CType::Int(_) | CType::Enum(_) | CType::Float => 4,
            CType::Long(_) | CType::LongLong(_) | CType::Double | CType::Pointer(_) => 8,
            CType::LongDouble => 16,
            CType::Int128(_) => 16,
            CType::Array(elem, Some(n)) => elem.size() * n,
            CType::Array(_, None) => 8, // incomplete array = pointer-sized
            CType::Function(..) => 8,   // function pointer
            CType::Struct(def) => self.struct_size(def),
            CType::Union(def) => self.union_size(def),
        }
    }

    pub fn align(&self) -> usize {
        match self {
            CType::Void => 1,
            CType::Bool | CType::Char(_) => 1,
            CType::Short(_) => 2,
            CType::Int(_) | CType::Enum(_) | CType::Float => 4,
            CType::Long(_) | CType::LongLong(_) | CType::Double | CType::Pointer(_) => 8,
            CType::LongDouble | CType::Int128(_) => 16,
            CType::Array(elem, _) => elem.align(),
            CType::Function(..) => 8,
            CType::Struct(def) => def.fields.iter().map(|f| f.ty.align()).max().unwrap_or(1),
            CType::Union(def) => def.fields.iter().map(|f| f.ty.align()).max().unwrap_or(1),
        }
    }

    pub fn signedness(&self) -> Signedness {
        use Signedness::*;
        match self {
            CType::Bool | CType::Pointer(_) => Unsigned,
            CType::Char(s) | CType::Short(s) | CType::Int(s)
            | CType::Long(s) | CType::LongLong(s) | CType::Int128(s) => *s,
            _ => Signed, // floats, void, aggregates
        }
    }

    pub fn is_unsigned(&self) -> bool { self.signedness().is_unsigned() }

    pub fn is_signed(&self) -> bool { self.signedness().is_signed() }

    pub fn is_float(&self) -> bool {
        matches!(self, CType::Float | CType::Double | CType::LongDouble)
    }

    pub fn is_pointer(&self) -> bool {
        matches!(self, CType::Pointer(_))
    }

    fn struct_size(&self, def: &StructDef) -> usize {
        let mut offset = 0usize;
        let mut bit_pos = 0u32; // bits used in current bitfield storage unit
        let mut bit_unit_size = 0usize; // size of current bitfield storage unit (0 = not in bitfield)
        for field in &def.fields {
            if let Some(bw) = field.bit_width {
                let unit_size = field.ty.size();
                let unit_bits = (unit_size * 8) as u32;
                let align = field.ty.align();
                if bw == 0 {
                    // Zero-width bitfield: flush current unit, align to next boundary
                    if bit_unit_size > 0 {
                        offset += bit_unit_size;
                        bit_pos = 0;
                        bit_unit_size = 0;
                    }
                    offset = (offset + align - 1) & !(align - 1);
                } else if bit_unit_size == unit_size && bit_pos + bw <= unit_bits {
                    // Fits in current storage unit
                    bit_pos += bw;
                } else {
                    // Start new storage unit
                    offset += bit_unit_size;
                    offset = (offset + align - 1) & !(align - 1);
                    bit_unit_size = unit_size;
                    bit_pos = bw;
                }
                continue;
            }
            // Flush any pending bitfield storage unit
            offset += bit_unit_size;
            bit_pos = 0;
            bit_unit_size = 0;
            // Flexible array member (incomplete array at end of struct) has size 0
            if matches!(&field.ty, CType::Array(_, None)) {
                continue;
            }
            let align = field.ty.align();
            offset = (offset + align - 1) & !(align - 1);
            offset += field.ty.size();
        }
        // Flush trailing bitfield unit
        offset += bit_unit_size;
        let struct_align = self.align();
        (offset + struct_align - 1) & !(struct_align - 1)
    }

    fn union_size(&self, def: &StructDef) -> usize {
        let max_size = def.fields.iter().map(|f| f.ty.size()).max().unwrap_or(0);
        let align = self.align();
        (max_size + align - 1) & !(align - 1)
    }

    /// Get the field offset within a struct
    /// Returns (byte_offset, bit_offset_within_storage_unit, field_type).
    /// bit_offset is 0 for non-bitfield fields.
    pub fn field_offset(&self, name: &str) -> Option<(usize, u32, CType)> {
        let def = match self {
            CType::Struct(def) => def,
            CType::Union(_) => return self.union_field(name),
            _ => return None,
        };
        let mut offset = 0usize;
        let mut bit_pos = 0u32;
        let mut bit_unit_size = 0usize;
        for field in &def.fields {
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
                    let field_bit_off = bit_pos;
                    bit_pos += bw;
                    if field.name.as_deref() == Some(name) {
                        return Some((offset, field_bit_off, field.ty.clone()));
                    }
                    continue;
                } else {
                    offset += bit_unit_size;
                    offset = (offset + align - 1) & !(align - 1);
                    bit_unit_size = unit_size;
                    bit_pos = bw;
                    if field.name.as_deref() == Some(name) {
                        return Some((offset, 0, field.ty.clone()));
                    }
                    continue;
                }
                continue;
            }
            // Flush any pending bitfield storage unit
            offset += bit_unit_size;
            bit_pos = 0;
            bit_unit_size = 0;

            let align = field.ty.align();
            offset = (offset + align - 1) & !(align - 1);

            if field.name.as_deref() == Some(name) {
                return Some((offset, 0, field.ty.clone()));
            }

            // Anonymous struct/union - search inside
            if field.name.is_none() {
                if let Some((inner_offset, inner_bit_off, ty)) = field.ty.field_offset(name) {
                    return Some((offset + inner_offset, inner_bit_off, ty));
                }
            }

            offset += field.ty.size();
        }
        None
    }

    fn union_field(&self, name: &str) -> Option<(usize, u32, CType)> {
        let def = match self {
            CType::Union(def) => def,
            _ => return None,
        };
        for field in &def.fields {
            if field.name.as_deref() == Some(name) {
                return Some((0, 0, field.ty.clone()));
            }
            if field.name.is_none() {
                if let Some((inner_offset, inner_bit_off, ty)) = field.ty.field_offset(name) {
                    return Some((inner_offset, inner_bit_off, ty));
                }
            }
        }
        None
    }

    /// Look up a field's bitfield width (None if not a bitfield or field not found).
    pub fn field_bit_width(&self, name: &str) -> Option<u32> {
        let def = match self {
            CType::Struct(def) | CType::Union(def) => def,
            _ => return None,
        };
        for field in &def.fields {
            if field.name.as_deref() == Some(name) {
                return field.bit_width;
            }
            // Anonymous struct/union - search inside
            if field.name.is_none() {
                if let Some(w) = field.ty.field_bit_width(name) {
                    return Some(w);
                }
            }
        }
        None
    }

    /// Apply C integer promotion for a type, considering bitfield width if provided.
    /// Per C99 6.3.1.1: if int can represent all values, promote to int; else unsigned int.
    pub fn promote_integer(ty: CType, bit_width: Option<u32>) -> CType {
        if let Some(w) = bit_width {
            // Bitfield promotion: width determines result type
            if w == 0 { return ty; }
            if w < 32 { return CType::Int(Signedness::Signed); }  // fits in int
            if w == 32 { return CType::Int(Signedness::Unsigned); } // unsigned int
            return ty; // > 32 bits: keep original type
        }
        // Non-bitfield integer promotion: small types promote to int
        match ty {
            CType::Char(_) | CType::Short(_) | CType::Bool => CType::Int(Signedness::Signed),
            _ => ty,
        }
    }

    /// Number of scalar initializer values consumed by flat/brace-elided initialization.
    pub fn flat_init_count(&self) -> usize {
        match self {
            CType::Array(elem, Some(n)) => elem.flat_init_count() * n,
            CType::Struct(def) => def.fields.iter().map(|f| f.ty.flat_init_count()).sum(),
            CType::Union(def) => def.fields.first().map(|f| f.ty.flat_init_count()).unwrap_or(1),
            _ => 1,
        }
    }

    /// Integer promotion (C99 6.3.1.1)
    pub fn promote(&self) -> CType {
        match self {
            CType::Bool | CType::Char(_) | CType::Short(_) => CType::Int(Signedness::Signed),
            CType::Enum(_) => CType::Int(Signedness::Signed),
            other => other.clone(),
        }
    }

    /// Usual arithmetic conversions (C99 6.3.1.8)
    pub fn common(a: &CType, b: &CType) -> CType {
        let a = a.promote();
        let b = b.promote();
        if a == b { return a; }
        if a.is_float() || b.is_float() {
            match (&a, &b) {
                (CType::LongDouble, _) | (_, CType::LongDouble) => return CType::LongDouble,
                (CType::Double, _) | (_, CType::Double) => return CType::Double,
                (CType::Float, _) | (_, CType::Float) => return CType::Float,
                _ => {}
            }
        }
        // Both integers
        if a.size() > b.size() { a } else if b.size() > a.size() { b }
        else if !a.is_signed() { a } else { b }
    }
}

/// Tracks struct/union/enum tag definitions and typedefs
pub struct TypeEnv {
    pub typedefs: HashMap<String, CType>,
    pub tags: HashMap<String, CType>,
    pub enum_constants: HashMap<String, i64>,
}

impl TypeEnv {
    pub fn new() -> Self {
        let mut env = Self {
            typedefs: HashMap::new(),
            tags: HashMap::new(),
            enum_constants: HashMap::new(),
        };
        // Compiler builtins — no header provides these
        env.typedefs.insert("__builtin_va_list".into(), CType::Pointer(Box::new(CType::Void)));
        env.typedefs.insert("__uint128_t".into(), CType::Int128(Signedness::Unsigned));
        env
    }

    pub fn is_typedef(&self, name: &str) -> bool {
        self.typedefs.contains_key(name)
    }
}


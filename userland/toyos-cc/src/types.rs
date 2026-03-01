use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum CType {
    Void,
    Bool,
    Char(bool),       // signed
    Short(bool),      // signed
    Int(bool),        // signed
    Long(bool),       // signed
    LongLong(bool),   // signed
    Int128(bool),     // signed
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
    pub packed: bool,
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
            CType::Struct(def) => {
                if def.packed { 1 }
                else { def.fields.iter().map(|f| f.ty.align()).max().unwrap_or(1) }
            }
            CType::Union(def) => {
                if def.packed { 1 }
                else { def.fields.iter().map(|f| f.ty.align()).max().unwrap_or(1) }
            }
        }
    }

    pub fn is_integer(&self) -> bool {
        matches!(self, CType::Bool | CType::Char(_) | CType::Short(_) | CType::Int(_)
            | CType::Long(_) | CType::LongLong(_) | CType::Int128(_) | CType::Enum(_))
    }

    pub fn is_float(&self) -> bool {
        matches!(self, CType::Float | CType::Double | CType::LongDouble)
    }

    pub fn is_arithmetic(&self) -> bool {
        self.is_integer() || self.is_float()
    }

    pub fn is_pointer(&self) -> bool {
        matches!(self, CType::Pointer(_))
    }

    pub fn is_signed(&self) -> bool {
        match self {
            CType::Char(s) | CType::Short(s) | CType::Int(s)
            | CType::Long(s) | CType::LongLong(s) | CType::Int128(s) => *s,
            CType::Float | CType::Double | CType::LongDouble => true,
            _ => false,
        }
    }

    pub fn pointee(&self) -> &CType {
        match self {
            CType::Pointer(inner) => inner,
            _ => panic!("not a pointer type"),
        }
    }

    fn struct_size(&self, def: &StructDef) -> usize {
        let mut offset = 0usize;
        for field in &def.fields {
            if field.bit_width.is_some() {
                // Simplified bitfield handling - allocate full type size
                let align = if def.packed { 1 } else { field.ty.align() };
                offset = (offset + align - 1) & !(align - 1);
                offset += field.ty.size();
                continue;
            }
            let align = if def.packed { 1 } else { field.ty.align() };
            offset = (offset + align - 1) & !(align - 1);
            offset += field.ty.size();
        }
        let struct_align = self.align();
        (offset + struct_align - 1) & !(struct_align - 1)
    }

    fn union_size(&self, def: &StructDef) -> usize {
        let max_size = def.fields.iter().map(|f| f.ty.size()).max().unwrap_or(0);
        let align = self.align();
        (max_size + align - 1) & !(align - 1)
    }

    /// Get the field offset within a struct
    pub fn field_offset(&self, name: &str) -> Option<(usize, CType)> {
        let def = match self {
            CType::Struct(def) => def,
            CType::Union(_) => return self.union_field(name),
            _ => return None,
        };
        let mut offset = 0usize;
        for field in &def.fields {
            let align = if def.packed { 1 } else { field.ty.align() };
            offset = (offset + align - 1) & !(align - 1);

            if field.name.as_deref() == Some(name) {
                return Some((offset, field.ty.clone()));
            }

            // Anonymous struct/union - search inside
            if field.name.is_none() {
                if let Some((inner_offset, ty)) = field.ty.field_offset(name) {
                    return Some((offset + inner_offset, ty));
                }
            }

            offset += field.ty.size();
        }
        None
    }

    fn union_field(&self, name: &str) -> Option<(usize, CType)> {
        let def = match self {
            CType::Union(def) => def,
            _ => return None,
        };
        for field in &def.fields {
            if field.name.as_deref() == Some(name) {
                return Some((0, field.ty.clone()));
            }
            if field.name.is_none() {
                if let Some((inner_offset, ty)) = field.ty.field_offset(name) {
                    return Some((inner_offset, ty));
                }
            }
        }
        None
    }

    /// Integer promotion (C99 6.3.1.1)
    pub fn promote(&self) -> CType {
        match self {
            CType::Bool | CType::Char(_) | CType::Short(_) => CType::Int(true),
            CType::Enum(_) => CType::Int(true),
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
        // Common typedefs
        env.typedefs.insert("size_t".into(), CType::Long(false));
        env.typedefs.insert("ssize_t".into(), CType::Long(true));
        env.typedefs.insert("ptrdiff_t".into(), CType::Long(true));
        env.typedefs.insert("intptr_t".into(), CType::Long(true));
        env.typedefs.insert("uintptr_t".into(), CType::Long(false));
        env.typedefs.insert("int8_t".into(), CType::Char(true));
        env.typedefs.insert("uint8_t".into(), CType::Char(false));
        env.typedefs.insert("int16_t".into(), CType::Short(true));
        env.typedefs.insert("uint16_t".into(), CType::Short(false));
        env.typedefs.insert("int32_t".into(), CType::Int(true));
        env.typedefs.insert("uint32_t".into(), CType::Int(false));
        env.typedefs.insert("int64_t".into(), CType::Long(true));
        env.typedefs.insert("uint64_t".into(), CType::Long(false));
        env.typedefs.insert("__int128_t".into(), CType::Int128(true));
        env.typedefs.insert("__uint128_t".into(), CType::Int128(false));
        env.typedefs.insert("va_list".into(), CType::Pointer(Box::new(CType::Void)));
        env.typedefs.insert("__builtin_va_list".into(), CType::Pointer(Box::new(CType::Void)));
        env.typedefs.insert("FILE".into(), CType::Struct(StructDef { name: Some("_IO_FILE".into()), fields: Vec::new(), packed: false }));
        env.typedefs.insert("wchar_t".into(), CType::Int(true));
        // POSIX/system types
        env.typedefs.insert("off_t".into(), CType::Long(true));
        env.typedefs.insert("pid_t".into(), CType::Int(true));
        env.typedefs.insert("mode_t".into(), CType::Int(false));
        env.typedefs.insert("dev_t".into(), CType::Long(false));
        env.typedefs.insert("ino_t".into(), CType::Long(false));
        env.typedefs.insert("nlink_t".into(), CType::Long(false));
        env.typedefs.insert("uid_t".into(), CType::Int(false));
        env.typedefs.insert("gid_t".into(), CType::Int(false));
        env.typedefs.insert("clock_t".into(), CType::Long(true));
        env.typedefs.insert("time_t".into(), CType::Long(true));
        env.typedefs.insert("socklen_t".into(), CType::Int(false));
        // jmp_buf: array of 8 longs (simplified, enough for sizeof)
        env.typedefs.insert("jmp_buf".into(), CType::Array(Box::new(CType::Long(true)), Some(8)));
        env.typedefs.insert("sigjmp_buf".into(), CType::Array(Box::new(CType::Long(true)), Some(8)));
        // Sync/threading types
        env.typedefs.insert("sem_t".into(), CType::Struct(StructDef { name: Some("sem_t".into()), fields: Vec::new(), packed: false }));
        env.typedefs.insert("dispatch_semaphore_t".into(), CType::Pointer(Box::new(CType::Void)));
        env.typedefs.insert("pthread_t".into(), CType::Long(false));
        env.typedefs.insert("pthread_mutex_t".into(), CType::Struct(StructDef { name: Some("pthread_mutex_t".into()), fields: Vec::new(), packed: false }));
        env.typedefs.insert("pthread_cond_t".into(), CType::Struct(StructDef { name: Some("pthread_cond_t".into()), fields: Vec::new(), packed: false }));
        env
    }

    pub fn is_typedef(&self, name: &str) -> bool {
        self.typedefs.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_sizes() {
        assert_eq!(CType::Void.size(), 0);
        assert_eq!(CType::Bool.size(), 1);
        assert_eq!(CType::Char(true).size(), 1);
        assert_eq!(CType::Char(false).size(), 1);
        assert_eq!(CType::Short(true).size(), 2);
        assert_eq!(CType::Int(true).size(), 4);
        assert_eq!(CType::Long(true).size(), 8);
        assert_eq!(CType::LongLong(true).size(), 8);
        assert_eq!(CType::Float.size(), 4);
        assert_eq!(CType::Double.size(), 8);
        assert_eq!(CType::LongDouble.size(), 16);
        assert_eq!(CType::Int128(true).size(), 16);
        assert_eq!(CType::Pointer(Box::new(CType::Void)).size(), 8);
    }

    #[test]
    fn primitive_alignments() {
        assert_eq!(CType::Char(true).align(), 1);
        assert_eq!(CType::Short(true).align(), 2);
        assert_eq!(CType::Int(true).align(), 4);
        assert_eq!(CType::Long(true).align(), 8);
        assert_eq!(CType::Double.align(), 8);
        assert_eq!(CType::Pointer(Box::new(CType::Int(true))).align(), 8);
        assert_eq!(CType::Int128(true).align(), 16);
    }

    #[test]
    fn array_size() {
        let arr = CType::Array(Box::new(CType::Int(true)), Some(10));
        assert_eq!(arr.size(), 40);
        assert_eq!(arr.align(), 4);
    }

    #[test]
    fn simple_struct_size() {
        // struct { int x; int y; } => 8 bytes
        let s = CType::Struct(StructDef {
            name: None,
            fields: vec![
                FieldDef { name: Some("x".into()), ty: CType::Int(true), bit_width: None },
                FieldDef { name: Some("y".into()), ty: CType::Int(true), bit_width: None },
            ],
            packed: false,
        });
        assert_eq!(s.size(), 8);
        assert_eq!(s.align(), 4);
    }

    #[test]
    fn struct_with_padding() {
        // struct { char a; int b; } => 8 bytes (3 bytes padding after a)
        let s = CType::Struct(StructDef {
            name: None,
            fields: vec![
                FieldDef { name: Some("a".into()), ty: CType::Char(true), bit_width: None },
                FieldDef { name: Some("b".into()), ty: CType::Int(true), bit_width: None },
            ],
            packed: false,
        });
        assert_eq!(s.size(), 8);
        assert_eq!(s.align(), 4);
    }

    #[test]
    fn packed_struct() {
        // __attribute__((packed)) struct { char a; int b; } => 5 bytes
        let s = CType::Struct(StructDef {
            name: None,
            fields: vec![
                FieldDef { name: Some("a".into()), ty: CType::Char(true), bit_width: None },
                FieldDef { name: Some("b".into()), ty: CType::Int(true), bit_width: None },
            ],
            packed: true,
        });
        assert_eq!(s.size(), 5);
        assert_eq!(s.align(), 1);
    }

    #[test]
    fn struct_trailing_padding() {
        // struct { int a; char b; } => 8 bytes (3 bytes trailing padding)
        let s = CType::Struct(StructDef {
            name: None,
            fields: vec![
                FieldDef { name: Some("a".into()), ty: CType::Int(true), bit_width: None },
                FieldDef { name: Some("b".into()), ty: CType::Char(true), bit_width: None },
            ],
            packed: false,
        });
        assert_eq!(s.size(), 8);
    }

    #[test]
    fn union_size() {
        // union { int a; char b; } => 4 bytes
        let u = CType::Union(StructDef {
            name: None,
            fields: vec![
                FieldDef { name: Some("a".into()), ty: CType::Int(true), bit_width: None },
                FieldDef { name: Some("b".into()), ty: CType::Char(true), bit_width: None },
            ],
            packed: false,
        });
        assert_eq!(u.size(), 4);
        assert_eq!(u.align(), 4);
    }

    #[test]
    fn field_offset_simple() {
        let s = CType::Struct(StructDef {
            name: None,
            fields: vec![
                FieldDef { name: Some("x".into()), ty: CType::Int(true), bit_width: None },
                FieldDef { name: Some("y".into()), ty: CType::Int(true), bit_width: None },
            ],
            packed: false,
        });
        assert_eq!(s.field_offset("x"), Some((0, CType::Int(true))));
        assert_eq!(s.field_offset("y"), Some((4, CType::Int(true))));
        assert_eq!(s.field_offset("z"), None);
    }

    #[test]
    fn field_offset_with_padding() {
        let s = CType::Struct(StructDef {
            name: None,
            fields: vec![
                FieldDef { name: Some("a".into()), ty: CType::Char(true), bit_width: None },
                FieldDef { name: Some("b".into()), ty: CType::Long(true), bit_width: None },
            ],
            packed: false,
        });
        assert_eq!(s.field_offset("a"), Some((0, CType::Char(true))));
        assert_eq!(s.field_offset("b"), Some((8, CType::Long(true))));
    }

    #[test]
    fn union_field_offset() {
        let u = CType::Union(StructDef {
            name: None,
            fields: vec![
                FieldDef { name: Some("a".into()), ty: CType::Int(true), bit_width: None },
                FieldDef { name: Some("b".into()), ty: CType::Char(true), bit_width: None },
            ],
            packed: false,
        });
        assert_eq!(u.field_offset("a"), Some((0, CType::Int(true))));
        assert_eq!(u.field_offset("b"), Some((0, CType::Char(true))));
    }

    #[test]
    fn is_integer() {
        assert!(CType::Int(true).is_integer());
        assert!(CType::Char(false).is_integer());
        assert!(CType::Bool.is_integer());
        assert!(!CType::Float.is_integer());
        assert!(!CType::Pointer(Box::new(CType::Void)).is_integer());
    }

    #[test]
    fn is_signed() {
        assert!(CType::Int(true).is_signed());
        assert!(!CType::Int(false).is_signed());
        assert!(CType::Float.is_signed());
        assert!(!CType::Pointer(Box::new(CType::Void)).is_signed());
    }

    #[test]
    fn integer_promotion() {
        assert_eq!(CType::Char(true).promote(), CType::Int(true));
        assert_eq!(CType::Short(false).promote(), CType::Int(true));
        assert_eq!(CType::Bool.promote(), CType::Int(true));
        assert_eq!(CType::Int(true).promote(), CType::Int(true));
        assert_eq!(CType::Long(true).promote(), CType::Long(true));
    }

    #[test]
    fn common_type() {
        assert_eq!(CType::common(&CType::Int(true), &CType::Int(true)), CType::Int(true));
        assert_eq!(CType::common(&CType::Int(true), &CType::Long(true)), CType::Long(true));
        assert_eq!(CType::common(&CType::Int(true), &CType::Double), CType::Double);
        assert_eq!(CType::common(&CType::Float, &CType::Double), CType::Double);
        assert_eq!(CType::common(&CType::Char(true), &CType::Short(true)), CType::Int(true)); // both promote to int
    }

    #[test]
    fn type_env_builtins() {
        let env = TypeEnv::new();
        assert!(env.is_typedef("size_t"));
        assert!(env.is_typedef("int32_t"));
        assert!(env.is_typedef("va_list"));
        assert!(!env.is_typedef("foo"));
    }

    #[test]
    fn pointee() {
        let ptr = CType::Pointer(Box::new(CType::Int(true)));
        assert_eq!(ptr.pointee(), &CType::Int(true));
    }

    #[test]
    fn enum_size() {
        let e = CType::Enum(EnumDef { name: None, variants: vec![] });
        assert_eq!(e.size(), 4);
        assert_eq!(e.align(), 4);
    }
}

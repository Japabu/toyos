pub type TranslationUnit = Vec<ExternalDecl>;

#[derive(Debug, Clone)]
pub enum ExternalDecl {
    Function(FunctionDef),
    Declaration(Declaration),
}

#[derive(Debug, Clone)]
pub struct FunctionDef {
    pub specifiers: Vec<DeclSpecifier>,
    pub declarator: Declarator,
    pub body: Statement,
}

#[derive(Debug, Clone)]
pub struct Declaration {
    pub specifiers: Vec<DeclSpecifier>,
    pub declarators: Vec<InitDeclarator>,
}

#[derive(Debug, Clone)]
pub struct InitDeclarator {
    pub declarator: Declarator,
    pub initializer: Option<Initializer>,
}

#[derive(Debug, Clone)]
pub enum Initializer {
    Expr(Expr),
    List(Vec<InitializerItem>),
}

#[derive(Debug, Clone)]
pub struct InitializerItem {
    pub designators: Vec<Designator>,
    pub initializer: Initializer,
}

#[derive(Debug, Clone)]
pub enum Designator {
    Field(String),
    Index(Box<Expr>),
}

// Declaration specifiers
#[derive(Debug, Clone)]
pub enum DeclSpecifier {
    StorageClass(StorageClass),
    TypeSpec(TypeSpec),
    TypeQual(TypeQual),
    FuncSpec(FuncSpec),
    Attribute(Vec<Attribute>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StorageClass {
    Auto,
    Register,
    Static,
    Extern,
    Typedef,
}

#[derive(Debug, Clone)]
pub enum TypeSpec {
    Void,
    Char,
    Short,
    Int,
    Long,
    Float,
    Double,
    Signed,
    Unsigned,
    Bool,
    Struct(StructType),
    Union(StructType),
    Enum(EnumType),
    TypedefName(String),
    Typeof(Box<Expr>),
    TypeofType(Box<TypeName>),
    Builtin(String),
    Int128,
}

#[derive(Debug, Clone)]
pub struct StructType {
    pub name: Option<String>,
    pub fields: Option<Vec<StructField>>,
    pub attributes: Vec<Attribute>,
}

#[derive(Debug, Clone)]
pub struct StructField {
    pub specifiers: Vec<DeclSpecifier>,
    pub declarators: Vec<StructFieldDeclarator>,
}

#[derive(Debug, Clone)]
pub struct StructFieldDeclarator {
    pub declarator: Option<Declarator>,
    pub bit_width: Option<Box<Expr>>,
}

#[derive(Debug, Clone)]
pub struct EnumType {
    pub name: Option<String>,
    pub variants: Option<Vec<Enumerator>>,
    pub attributes: Vec<Attribute>,
}

#[derive(Debug, Clone)]
pub struct Enumerator {
    pub name: String,
    pub value: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TypeQual {
    Const,
    Volatile,
    Restrict,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FuncSpec {
    Inline,
}

#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    pub args: Vec<Expr>,
}

// Declarator
#[derive(Debug, Clone)]
pub struct Declarator {
    pub pointer: Vec<Pointer>,
    pub direct: DirectDeclarator,
}

#[derive(Debug, Clone)]
pub struct Pointer {
    pub qualifiers: Vec<TypeQual>,
}

#[derive(Debug, Clone)]
pub enum DirectDeclarator {
    Ident(String),
    Paren(Box<Declarator>),
    Array(Box<DirectDeclarator>, Option<Box<Expr>>),
    Function(Box<DirectDeclarator>, ParamList),
}

#[derive(Debug, Clone)]
pub struct ParamList {
    pub params: Vec<ParamDecl>,
    pub variadic: bool,
}

#[derive(Debug, Clone)]
pub struct ParamDecl {
    pub specifiers: Vec<DeclSpecifier>,
    pub declarator: Option<Declarator>,
}

// Type name (for casts, sizeof)
#[derive(Debug, Clone)]
pub struct TypeName {
    pub specifiers: Vec<DeclSpecifier>,
    pub declarator: Option<AbstractDeclarator>,
}

#[derive(Debug, Clone)]
pub struct AbstractDeclarator {
    pub pointer: Vec<Pointer>,
    pub direct: Option<DirectAbstractDeclarator>,
}

#[derive(Debug, Clone)]
pub enum DirectAbstractDeclarator {
    Paren(Box<AbstractDeclarator>),
    Array(Option<Box<DirectAbstractDeclarator>>, Option<Box<Expr>>),
    Function(Option<Box<DirectAbstractDeclarator>>, ParamList),
}

// Statements
#[derive(Debug, Clone)]
pub enum Statement {
    Compound(Vec<BlockItem>),
    Expr(Option<Expr>),
    If(Box<Expr>, Box<Statement>, Option<Box<Statement>>),
    While(Box<Expr>, Box<Statement>),
    DoWhile(Box<Statement>, Box<Expr>),
    For(Option<Box<ForInit>>, Option<Box<Expr>>, Option<Box<Expr>>, Box<Statement>),
    Switch(Box<Expr>, Box<Statement>),
    Case(Box<Expr>, Box<Statement>),
    Default(Box<Statement>),
    Break,
    Continue,
    Return(Option<Expr>),
    Goto(String),
    Label(String, Box<Statement>),
    Asm(AsmStmt),
}

#[derive(Debug, Clone)]
pub enum ForInit {
    Decl(Declaration),
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub enum BlockItem {
    Decl(Declaration),
    Stmt(Statement),
}

#[derive(Debug, Clone)]
pub struct AsmStmt {
    pub volatile: bool,
    pub template: String,
    pub outputs: Vec<AsmOperand>,
    pub inputs: Vec<AsmOperand>,
    pub clobbers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AsmOperand {
    pub constraint: String,
    pub expr: Expr,
}

// Expressions
#[derive(Debug, Clone)]
pub enum Expr {
    IntLit(i128),
    UIntLit(u128),
    FloatLit(f64),
    CharLit(i8),
    StringLit(Vec<u8>),
    Ident(String),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Unary(UnaryOp, Box<Expr>),
    PostUnary(PostOp, Box<Expr>),
    Cast(Box<TypeName>, Box<Expr>),
    Sizeof(Box<SizeofArg>),
    Alignof(Box<TypeName>),
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    Member(Box<Expr>, String),
    Arrow(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Assign(AssignOp, Box<Expr>, Box<Expr>),
    Comma(Box<Expr>, Box<Expr>),
    CompoundLiteral(Box<TypeName>, Vec<InitializerItem>),
    StmtExpr(Vec<BlockItem>),
    VaArg(Box<Expr>, Box<TypeName>),
    Offsetof(Box<TypeName>, Vec<OffsetofField>),
    Builtin(String, Vec<Expr>),
}

#[derive(Debug, Clone)]
pub enum OffsetofField {
    Field(String),
    Index(Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum SizeofArg {
    Expr(Expr),
    Type(TypeName),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add, Sub, Mul, Div, Mod,
    BitAnd, BitOr, BitXor,
    Shl, Shr,
    LogAnd, LogOr,
    Eq, Ne, Lt, Gt, Le, Ge,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnaryOp {
    Neg, BitNot, LogNot,
    Deref, AddrOf,
    PreInc, PreDec,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PostOp {
    PostInc, PostDec,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AssignOp {
    Assign,
    MulAssign, DivAssign, ModAssign,
    AddAssign, SubAssign,
    ShlAssign, ShrAssign,
    AndAssign, XorAssign, OrAssign,
}

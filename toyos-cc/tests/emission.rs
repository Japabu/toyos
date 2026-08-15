//! What the compiler *emitted*, which is the half no other test here reads.
//!
//! The rest of the suite asks whether a translation unit compiled, and the
//! tinycc corpus asks whether the program then ran. Neither can see a
//! miscompilation that the corpus does not happen to exercise: a candidate fix
//! that turned every struct assignment into an eight-byte store of the source's
//! *address* left all 156 corpus files, all eleven host tests and the
//! determinism suite green, because determinism compares a run against another
//! run and encodes no expected bytes at all.

mod common;

use object::read::{Object, ObjectSection, ObjectSymbol};
use object::{RelocationTarget, SymbolKind};

fn compile(source: &str) -> Vec<u8> {
    toyos_cc::compile(source, "emission.c", &common::options())
}

/// The `.text` bytes of one function, by symbol.
fn body(obj: &object::File, name: &str) -> Vec<u8> {
    let sym = obj
        .symbols()
        .find(|s| s.kind() == SymbolKind::Text && s.name() == Ok(name))
        .unwrap_or_else(|| panic!("no function symbol {name:?} in the object"));
    let section = obj.section_by_index(sym.section_index().unwrap()).unwrap();
    let data = section.data().unwrap();
    let start = (sym.address() - section.address()) as usize;
    data[start..start + sym.size() as usize].to_vec()
}

/// The names of the symbols `name`'s body is relocated against.
fn calls(obj: &object::File, name: &str) -> Vec<String> {
    relocations(obj, name).into_iter().map(|(_, target)| target).collect()
}

/// Every relocation in `name`'s body, as an offset from the function's start
/// and the symbol it names.
fn relocations(obj: &object::File, name: &str) -> Vec<(u64, String)> {
    let sym = obj
        .symbols()
        .find(|s| s.kind() == SymbolKind::Text && s.name() == Ok(name))
        .unwrap_or_else(|| panic!("no function symbol {name:?} in the object"));
    let section = obj.section_by_index(sym.section_index().unwrap()).unwrap();
    let (lo, hi) = (sym.address(), sym.address() + sym.size());
    let mut out = Vec::new();
    for (offset, reloc) in section.relocations() {
        if offset < lo || offset >= hi {
            continue;
        }
        if let RelocationTarget::Symbol(index) = reloc.target() {
            if let Ok(target) = obj.symbol_by_index(index) {
                if let Ok(target) = target.name() {
                    out.push((offset - lo, target.to_string()));
                }
            }
        }
    }
    out
}

/// Whether the body loads `imm` into a 32-bit register — `mov r32, imm32`,
/// opcodes `B8`..`BF`, which is how a size operand reaches `memcpy`.
fn loads_immediate(body: &[u8], imm: u32) -> bool {
    let wanted = imm.to_le_bytes();
    body.windows(5).any(|w| (0xB8..=0xBF).contains(&w[0]) && w[1..] == wanted)
}

/// A struct assignment is a `memcpy` of the struct's size, and the eight-byte
/// store that is the tempting shortcut is a different program.
#[test]
fn a_struct_assignment_copies_the_whole_struct() {
    let obj = compile(
        "struct S { long a, b, c, d; };
         void sink(struct S *);
         void f(void) { struct S x, y; sink(&x); y = x; sink(&y); }",
    );
    let obj = object::File::parse(&*obj).unwrap();
    let called = calls(&obj, "f");
    assert!(
        called.iter().any(|s| s == "memcpy"),
        "a struct assignment did not reach memcpy; f is relocated against {called:?}",
    );
    assert!(
        loads_immediate(&body(&obj, "f"), 32),
        "memcpy was not given sizeof(struct S) == 32",
    );
}

/// The control: the same shape on a scalar must *not* call memcpy, or the
/// assertion above passes on a compiler that memcpys everything.
#[test]
fn a_scalar_assignment_does_not() {
    let obj = compile("void sink(long *); void g(void) { long a, b; sink(&a); b = a; sink(&b); }");
    let obj = object::File::parse(&*obj).unwrap();
    let called = calls(&obj, "g");
    assert!(!called.iter().any(|s| s == "memcpy"), "g reached memcpy: {called:?}");
}

/// A call through a function pointer dereferences the pointer once, wherever
/// the pointer is kept. A static local used to get a second load on top of the
/// first, which dereferenced the callee's own address and called whatever its
/// first eight bytes spelled — so the two functions below emitted different
/// code for the same program.
///
/// The file-scope one is the reference because it is an independent path: it
/// is `compile_expr`'s global-data branch and never consults `LocalStorage` at
/// all, so no change to how a local is stored can move both sides together.
#[test]
fn a_call_through_a_static_local_pointer_loads_once() {
    let obj = compile(
        "int g(void);
         static int (*at_file_scope)(void);
         int via_file(void) { at_file_scope = g; return at_file_scope(); }
         int via_block(void) { static int (*in_a_block)(void); in_a_block = g; return in_a_block(); }",
    );
    let obj = object::File::parse(&*obj).unwrap();
    assert_eq!(
        body(&obj, "via_block"),
        body(&obj, "via_file"),
        "a static local function pointer is called differently from a file-scope one",
    );
    // The two differ only in which symbol the pointer is: same offsets, and
    // `g` named once at the same place in each.
    let (block, file) = (relocations(&obj, "via_block"), relocations(&obj, "via_file"));
    assert_eq!(
        block.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
        file.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
    );
    assert_eq!(
        block.iter().filter(|(_, s)| s == "g").count(),
        file.iter().filter(|(_, s)| s == "g").count(),
    );
}

/// An aggregate parameter is copied into the callee's own storage, and reading
/// it back is a read of that copy rather than of whatever the caller passed.
#[test]
fn an_aggregate_parameter_is_copied_into_the_callee() {
    let obj = compile("struct S { long a, b, c, d; }; long f(struct S s) { return s.c; }");
    let obj = object::File::parse(&*obj).unwrap();
    let called = calls(&obj, "f");
    assert!(called.iter().any(|s| s == "memcpy"), "f is relocated against {called:?}");
    assert!(loads_immediate(&body(&obj, "f"), 32), "the copy was not sizeof(struct S) == 32");
}

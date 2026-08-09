//! The allocator shims rustc normally synthesizes at final link time.
//!
//! A link that has rlibs and no leaf crate has to grow them itself. The names
//! carry a rustc crate disambiguator — a build hash that changes every time
//! `rust/` is rebuilt — and the table used to be nine string literals holding
//! one particular hash, which had already gone stale: it occurred zero times
//! across the thirty rlibs of the live `x86_64-unknown-toyos` sysroot, so the
//! synthesis produced nothing at all.
//!
//! **Every disambiguator here is invented**, which is the point: a test that
//! hardcodes today's hash reproduces the bug it is testing for.

mod common;

use common::{Case, ObjBuilder, RET};
use object::read::{Object, ObjectSection, ObjectSymbol};
use object::{SymbolKind, SymbolScope};

/// `_RNvCs<disambiguator>_7___rustc<len>_<name>` — v0 mangling for an item of
/// the compiler-internal `___rustc` crate, written out rather than built by
/// the code under test.
fn rustc_item(disambiguator: &str, name: &str) -> String {
    format!("_RNvCs{disambiguator}_7___rustc{}_{name}", name.len())
}

/// One object that references `__rust_*` and defines `__rdl_*`, which is the
/// shape of a link over rlibs with no leaf crate.
fn link_referencing(disambiguator: &str, referenced: &[&str], defined: &[&str]) -> Case {
    let mut b = ObjBuilder::new();
    for name in defined {
        b.text(&rustc_item(disambiguator, name), &[RET], SymbolScope::Dynamic);
    }
    let targets: Vec<_> = referenced
        .iter()
        .map(|name| b.undefined(&rustc_item(disambiguator, name), SymbolKind::Text))
        .collect();
    b.caller("_start", &targets, SymbolScope::Dynamic);
    Case::new(&format!("shims-{disambiguator}")).input("a.o", b.finish()).arg("-static")
}

fn defines(bytes: &[u8], name: &str) -> bool {
    let obj = object::File::parse(bytes).unwrap();
    obj.symbols().any(|s| s.name() == Ok(name) && s.section_index().is_some())
}

/// The five bytes at `name`, which for a synthesized trampoline is
/// `jmp rel32` with the displacement filled in.
fn body(bytes: &[u8], name: &str) -> Vec<u8> {
    let obj = object::File::parse(bytes).unwrap();
    let sym = obj
        .symbols()
        .find(|s| s.name() == Ok(name))
        .unwrap_or_else(|| panic!("no symbol {name:?} in the output"));
    let section = obj
        .sections()
        .find(|s| {
            let (lo, len) = (s.address(), s.size());
            sym.address() >= lo && sym.address() < lo + len
        })
        .unwrap();
    let start = (sym.address() - section.address()) as usize;
    section.data().unwrap()[start..start + 5].to_vec()
}

#[test]
fn every_shim_is_synthesized_under_a_disambiguator_nobody_has_seen() {
    for (shim, target) in [
        ("__rust_alloc", "__rdl_alloc"),
        ("__rust_dealloc", "__rdl_dealloc"),
        ("__rust_realloc", "__rdl_realloc"),
        ("__rust_alloc_zeroed", "__rdl_alloc_zeroed"),
        ("__rust_alloc_error_handler", "__rdl_alloc_error_handler"),
    ] {
        let disambiguator = "qQ7zZ0aA9bB1c";
        let out = link_referencing(disambiguator, &[shim], &[target]).link();
        let shim_symbol = rustc_item(disambiguator, shim);
        assert!(defines(&out, &shim_symbol), "{shim} got no trampoline");
        let jump = body(&out, &shim_symbol);
        assert_eq!(jump[0], 0xE9, "{shim}'s trampoline is not a jmp: {jump:02x?}");
        assert_ne!(
            i32::from_le_bytes(jump[1..5].try_into().unwrap()),
            0,
            "{shim}'s jmp displacement was never relocated to {target}",
        );
    }
}

/// The one shim that is not a trampoline: rustc emits a bare `ret`.
#[test]
fn the_unstable_marker_is_a_ret() {
    let disambiguator = "Zz9Yy8Xx7Ww6V";
    let name = "__rust_no_alloc_shim_is_unstable_v2";
    let out = link_referencing(disambiguator, &[name], &[]).link();
    assert_eq!(body(&out, &rustc_item(disambiguator, name))[0], RET);
}

/// The bound on the wildcard. The same sysroot carries six `___rustc` symbols
/// that are not allocator shims, and a match that loosened the *name* as well
/// as the hash would hand each of them a jump to a `__rdl_` symbol nothing
/// defines. Each must stay undefined and fail the link by its own name.
#[test]
fn a_rustc_symbol_that_is_not_a_shim_gets_no_trampoline() {
    for name in [
        "__rust_start_panic",
        "__rust_drop_panic",
        "__rust_foreign_exception",
        "__rust_panic_cleanup",
        "__rust_abort",
        "__rust_probestack",
    ] {
        let disambiguator = "aB3cD4eF5gH6i";
        let said = link_referencing(disambiguator, &[name], &[]).link_expecting_failure();
        assert!(
            said.contains(&rustc_item(disambiguator, name)),
            "the link failed without naming {name}: {said}",
        );
        assert!(
            !said.contains("__rdl_"),
            "{name} was given a trampoline to a __rdl_ symbol: {said}",
        );
    }
}

/// A symbol shaped like one of the five but belonging to some other crate is
/// not the compiler's, and the `___rustc` path is what says so.
#[test]
fn a_lookalike_outside_the_rustc_crate_gets_no_trampoline() {
    let mut b = ObjBuilder::new();
    let name = "_RNvCsqQ7zZ0aA9bB1c_8_notrustc12___rust_alloc";
    let target = b.undefined(name, SymbolKind::Text);
    b.caller("_start", &[target], SymbolScope::Dynamic);
    let said = Case::new("shims-lookalike")
        .input("a.o", b.finish())
        .arg("-static")
        .link_expecting_failure();
    assert!(said.contains(name), "the link failed without naming {name}: {said}");
}

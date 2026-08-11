//! The same linker binary, given byte-identical inputs, must emit a
//! byte-identical output.
//!
//! Every case drives the `toyos-ld` binary rather than the library: a build is
//! one process per output, and `RandomState` seeds each map from a per-process
//! key, so two links inside one process are not the two links a build performs.

mod common;

use common::{archive, Case, ObjBuilder, RET};
use object::{SymbolKind, SymbolScope};
use std::process::Command;

/// A synthetic translation unit with `n` of each kind of symbol, dense enough
/// that a hash order and a sorted order almost never coincide.
fn wide_object(n: usize, tag: &str) -> Vec<u8> {
    let mut b = ObjBuilder::new();
    for i in 0..n {
        b.text(&format!("{tag}_local_fn_{i}"), &[RET], SymbolScope::Compilation);
        b.data(&format!("{tag}_local_obj_{i}"), &[i as u8; 8], SymbolScope::Compilation);
    }
    let mut globals = Vec::new();
    for i in 0..n {
        globals.push(b.data(&format!("{tag}_global_obj_{i}"), &[i as u8; 8], SymbolScope::Linkage));
        b.text(&format!("{tag}_global_fn_{i}"), &[RET], SymbolScope::Linkage);
    }
    b.got_loader(&format!("{tag}_loads"), &globals, SymbolScope::Linkage);
    b.finish()
}

fn start_object(calls: &[&str]) -> Vec<u8> {
    let mut b = ObjBuilder::new();
    let targets: Vec<_> = calls.iter().map(|n| b.undefined(n, SymbolKind::Text)).collect();
    b.caller("_start", &targets, SymbolScope::Linkage);
    b.finish()
}

// ── Cases ────────────────────────────────────────────────────────────────

/// `.symtab`/`.strtab`: `LinkState::locals` and `::globals` are walked to build
/// the symbol entries, and the string table follows the entry order.
#[test]
fn symtab_order() {
    Case::new("symtab")
        .input("wide.o", wide_object(200, "w"))
        .input("start.o", start_object(&["w_loads"]))
        .assert_identical("symtab/strtab");
}

/// `.rela.dyn` RELATIVE entries: `ElfLayout::got` is walked to fill the GOT.
#[test]
fn rela_dyn_relatives() {
    let mut b = ObjBuilder::new();
    let targets: Vec<_> =
        (0..200).map(|i| b.data(&format!("g{i}"), &[i as u8; 8], SymbolScope::Linkage)).collect();
    b.got_loader("_start", &targets, SymbolScope::Linkage);
    Case::new("relatives").input("got.o", b.finish()).assert_identical(".rela.dyn RELATIVE");
}

/// `.rela.dyn` GLOB_DAT and the `.dynsym`/`.dynstr` that name them:
/// `ElfLayout::dyn_got` is walked, and the import strings follow it.
#[test]
fn rela_dyn_glob_dat() {
    let dir = std::env::temp_dir().join(format!("toyos-ld-det-{}-solib", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut provider = ObjBuilder::new();
    for i in 0..150 {
        provider.data(&format!("imp{i}"), &[i as u8; 8], SymbolScope::Dynamic);
    }
    let provider_path = dir.join("provider.o");
    std::fs::write(&provider_path, provider.finish()).unwrap();
    let so = dir.join("libprov.so");
    let out = Command::new(env!("CARGO_BIN_EXE_toyos-ld"))
        .arg("--shared")
        .arg("-o")
        .arg(&so)
        .arg(&provider_path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let mut b = ObjBuilder::new();
    let targets: Vec<_> =
        (0..150).map(|i| b.undefined(&format!("imp{i}"), SymbolKind::Data)).collect();
    b.got_loader("_start", &targets, SymbolScope::Linkage);

    Case::new("globdat")
        .input("main.o", b.finish())
        .input("libprov.so", std::fs::read(&so).unwrap())
        .assert_identical(".rela.dyn GLOB_DAT");
}

/// A shared-library output: `.dynsym` exports, `.gnu.hash` buckets and the
/// symbol table all come off the same two maps.
#[test]
fn shared_output() {
    Case::new("shared")
        .arg("--shared")
        .input("wide.o", wide_object(150, "s"))
        .assert_identical("shared library");
}

/// The PE writer emits the UEFI bootloader, and holds a `got` of the same
/// shape as the ELF one.
#[test]
fn pe_output() {
    let mut b = ObjBuilder::new();
    let targets: Vec<_> =
        (0..150).map(|i| b.data(&format!("p{i}"), &[i as u8; 8], SymbolScope::Linkage)).collect();
    b.got_loader("efi_main", &targets, SymbolScope::Linkage);
    for i in 0..150 {
        b.text(&format!("p_local_{i}"), &[RET], SymbolScope::Compilation);
    }
    Case::new("pe")
        .arg("--pe")
        .arg("-e")
        .arg("efi_main")
        .input("uefi.o", b.finish())
        .assert_identical("PE");
}

/// A static output: the kernel's shape. The GOT is filled in place rather than
/// through `.rela.dyn`, so this is the writer's own ordering.
#[test]
fn static_output() {
    Case::new("static")
        .arg("--static")
        .input("wide.o", wide_object(150, "t"))
        .input("start.o", start_object(&["t_loads"]))
        .assert_identical("static executable");
}

/// Archive member selection: the pull-in worklist is seeded from the undefined
/// set, so its order decides which members a symbol with two definitions pulls
/// in — and therefore which sections exist at all.
#[test]
fn archive_member_selection() {
    let mut members: Vec<(&'static str, Vec<u8>)> = Vec::new();
    let names: Vec<String> = (0..40).map(|i| format!("m{i}.o")).collect();
    for (i, name) in names.iter().enumerate() {
        let mut b = ObjBuilder::new();
        // Every member defines the same helper as well as its own symbol, so
        // which member the worklist reaches first is observable.
        b.text("shared_helper", &[RET], SymbolScope::Linkage);
        b.text(&format!("member_fn_{i}"), &[RET], SymbolScope::Linkage);
        let dep = b.undefined(&format!("member_fn_{}", (i + 1) % 40), SymbolKind::Text);
        b.caller(&format!("member_entry_{i}"), &[dep], SymbolScope::Linkage);
        members.push((Box::leak(name.clone().into_boxed_str()), b.finish()));
    }
    let wanted: Vec<String> = (0..40).map(|i| format!("member_entry_{i}")).collect();
    let refs: Vec<&str> = wanted.iter().map(|s| s.as_str()).collect();

    Case::new("archive")
        .input("start.o", start_object(&refs))
        .input("libm.a", archive(&members))
        .assert_identical("archive member selection");
}

/// Everything at once, which is the shape of a real link.
#[test]
fn broad() {
    let mut members: Vec<(&'static str, Vec<u8>)> = Vec::new();
    let names: Vec<String> = (0..20).map(|i| format!("a{i}.o")).collect();
    for (i, name) in names.iter().enumerate() {
        let mut b = ObjBuilder::new();
        for j in 0..10 {
            b.text(&format!("arch_local_{i}_{j}"), &[RET], SymbolScope::Compilation);
        }
        b.text(&format!("arch_fn_{i}"), &[RET], SymbolScope::Linkage);
        members.push((Box::leak(name.clone().into_boxed_str()), b.finish()));
    }
    let wanted: Vec<String> = (0..20).map(|i| format!("arch_fn_{i}")).collect();
    let mut refs: Vec<&str> = wanted.iter().map(|s| s.as_str()).collect();
    refs.push("b_loads");

    Case::new("broad")
        .input("wide.o", wide_object(300, "b"))
        .input("start.o", start_object(&refs))
        .input("liba.a", archive(&members))
        .assert_identical("broad");
}

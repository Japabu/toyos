//! The same linker binary, given byte-identical inputs, must emit a
//! byte-identical output.
//!
//! Every case drives the `toyos-ld` binary rather than the library: a build is
//! one process per output, and `RandomState` seeds each map from a per-process
//! key, so two links inside one process are not the two links a build performs.

use object::write::{Object, Relocation, StandardSection, Symbol, SymbolSection};
use object::{
    elf, Architecture, BinaryFormat, Endianness, RelocationFlags, SymbolFlags, SymbolKind,
    SymbolScope,
};
use std::path::{Path, PathBuf};
use std::process::Command;

/// How many samples each case takes. Two suffices for a case whose hazard is
/// wide — every case below is, deliberately, so that a hash order and a sorted
/// order essentially never coincide. Eight is the margin for the narrow case
/// somebody adds later: `toyos-cc`'s gate has one hazard of a single stack slot,
/// and two runs caught it 39 times in 40 where eight caught it 40.
const RUNS: usize = 8;

// ── Input synthesis ──────────────────────────────────────────────────────

/// `mov rax, [rip + disp32]` — the byte sequence Cranelift emits for a GOT load.
const MOV_RAX_RIP: [u8; 3] = [0x48, 0x8B, 0x05];
/// `call rel32`
const CALL_REL32: u8 = 0xE8;
const RET: u8 = 0xC3;

struct ObjBuilder {
    obj: Object<'static>,
}

impl ObjBuilder {
    fn new() -> Self {
        ObjBuilder {
            obj: Object::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little),
        }
    }

    fn text(&mut self, name: &str, code: &[u8], scope: SymbolScope) -> object::write::SymbolId {
        let section = self.obj.section_id(StandardSection::Text);
        let offset = self.obj.append_section_data(section, code, 16);
        self.obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: offset,
            size: code.len() as u64,
            kind: SymbolKind::Text,
            scope,
            weak: false,
            section: SymbolSection::Section(section),
            flags: SymbolFlags::None,
        })
    }

    fn data(&mut self, name: &str, bytes: &[u8], scope: SymbolScope) -> object::write::SymbolId {
        let section = self.obj.section_id(StandardSection::Data);
        let offset = self.obj.append_section_data(section, bytes, 8);
        self.obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: offset,
            size: bytes.len() as u64,
            kind: SymbolKind::Data,
            scope,
            weak: false,
            section: SymbolSection::Section(section),
            flags: SymbolFlags::None,
        })
    }

    fn undefined(&mut self, name: &str, kind: SymbolKind) -> object::write::SymbolId {
        self.obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: 0,
            size: 0,
            kind,
            scope: SymbolScope::Dynamic,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        })
    }

    /// Append a function that GOT-loads each of `targets`, then returns. The
    /// GOT slot order is what `.rela.dyn` is built from.
    fn got_loader(
        &mut self,
        name: &str,
        targets: &[object::write::SymbolId],
        scope: SymbolScope,
    ) -> object::write::SymbolId {
        let mut code = Vec::new();
        for _ in targets {
            code.extend_from_slice(&MOV_RAX_RIP);
            code.extend_from_slice(&0i32.to_le_bytes());
        }
        code.push(RET);
        let sym = self.text(name, &code, scope);
        let section = self.obj.section_id(StandardSection::Text);
        let base = self.symbol_offset(sym);
        for (i, &target) in targets.iter().enumerate() {
            self.obj.add_relocation(
                section,
                Relocation {
                    offset: base + (i * 7) as u64 + MOV_RAX_RIP.len() as u64,
                    symbol: target,
                    addend: -4,
                    flags: RelocationFlags::Elf { r_type: elf::R_X86_64_REX_GOTPCRELX },
                },
            )
            .unwrap();
        }
        sym
    }

    /// Append a function that calls each of `targets` through PLT32.
    fn caller(
        &mut self,
        name: &str,
        targets: &[object::write::SymbolId],
        scope: SymbolScope,
    ) -> object::write::SymbolId {
        let mut code = Vec::new();
        for _ in targets {
            code.push(CALL_REL32);
            code.extend_from_slice(&0i32.to_le_bytes());
        }
        code.push(RET);
        let sym = self.text(name, &code, scope);
        let section = self.obj.section_id(StandardSection::Text);
        let base = self.symbol_offset(sym);
        for (i, &target) in targets.iter().enumerate() {
            self.obj.add_relocation(
                section,
                Relocation {
                    offset: base + (i * 5) as u64 + 1,
                    symbol: target,
                    addend: -4,
                    flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 },
                },
            )
            .unwrap();
        }
        sym
    }

    fn symbol_offset(&self, id: object::write::SymbolId) -> u64 {
        self.obj.symbol(id).value
    }

    fn finish(self) -> Vec<u8> {
        self.obj.write().unwrap()
    }
}

/// `ar` archive with the given members, in the format `collect` parses.
fn archive(members: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, data) in members {
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            format!("{name}/"),
            0,
            0,
            0,
            "100644",
            data.len()
        );
        assert_eq!(header.len(), 60);
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

// ── Harness ──────────────────────────────────────────────────────────────

struct Case {
    dir: PathBuf,
    inputs: Vec<PathBuf>,
    args: Vec<String>,
}

impl Case {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("toyos-ld-det-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Case { dir, inputs: Vec::new(), args: Vec::new() }
    }

    fn input(mut self, name: &str, bytes: Vec<u8>) -> Self {
        let path = self.dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        self.inputs.push(path);
        self
    }

    fn arg(mut self, a: &str) -> Self {
        self.args.push(a.to_string());
        self
    }

    fn link_once(&self, out: &Path) {
        let status = Command::new(env!("CARGO_BIN_EXE_toyos-ld"))
            .args(&self.args)
            .arg("-o")
            .arg(out)
            .args(&self.inputs)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "link failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    /// Link `RUNS` times and return the index of the first run whose output
    /// differs from run 0, with the number of differing bytes.
    fn diff(&self) -> Option<(usize, usize)> {
        let mut first: Option<Vec<u8>> = None;
        for run in 0..RUNS {
            let out = self.dir.join(format!("out.{run}"));
            self.link_once(&out);
            let bytes = std::fs::read(&out).unwrap();
            match &first {
                None => first = Some(bytes),
                Some(f) => {
                    if *f != bytes {
                        let differing = if f.len() == bytes.len() {
                            f.iter().zip(&bytes).filter(|(a, b)| a != b).count()
                        } else {
                            usize::MAX
                        };
                        return Some((run, differing));
                    }
                }
            }
        }
        None
    }

    fn assert_identical(&self, what: &str) {
        if let Some((run, bytes)) = self.diff() {
            panic!("{what}: run {run} of {RUNS} differs from run 0 in {bytes} bytes");
        }
    }
}

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

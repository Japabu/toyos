use object::{elf, pe};
use object::read::elf::{ElfFile64, FileHeader as _};
use object::read::{self, Object, ObjectSection, ObjectSymbol};
use object::write::{Object as WriteObject, StandardSection, Symbol, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, RelocationFlags, SymbolFlags, SymbolKind, SymbolScope,
};
use std::path::PathBuf;

// ── Helpers: toolchain ───────────────────────────────────────────────────

fn sysroot_libdir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../rust/build/aarch64-apple-darwin/stage1/lib/rustlib/x86_64-unknown-toyos/lib")
}

fn sysroot() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../rust/build/aarch64-apple-darwin/stage1")
}

fn rustc() -> PathBuf {
    sysroot().join("bin/rustc")
}

fn has_rustc() -> bool {
    rustc().exists() && sysroot_libdir().exists()
}

fn compile_to_obj(source: &str) -> Vec<u8> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("toyos-ld-test-{id}"));
    std::fs::create_dir_all(&dir).unwrap();
    let src_path = dir.join("test.rs");
    let obj_path = dir.join("test.o");
    std::fs::write(&src_path, source).unwrap();

    let output = std::process::Command::new(rustc())
        .args([
            "--target", "x86_64-unknown-toyos",
            "--edition", "2021",
            "-C", "panic=abort",
            "--emit=obj",
            "-o",
        ])
        .arg(&obj_path)
        .arg(&src_path)
        .arg("--sysroot")
        .arg(sysroot())
        .output()
        .expect("failed to run rustc");

    assert!(
        output.status.success(),
        "rustc failed:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );

    std::fs::read(&obj_path).unwrap()
}

fn std_rlibs() -> Vec<String> {
    [
        "std", "core", "alloc", "compiler_builtins", "panic_abort",
        "cfg_if", "hashbrown", "rustc_std_workspace_core", "rustc_std_workspace_alloc",
        "libc", "memchr", "adler2", "miniz_oxide", "object", "gimli", "addr2line",
        "rustc_demangle", "unwind", "std_detect",
    ].iter().map(|s| s.to_string()).collect()
}

// ── Helpers: object builder ──────────────────────────────────────────────

/// Fluent builder for constructing test ELF/COFF object files.
struct ObjBuilder {
    obj: WriteObject<'static>,
    text: object::write::SectionId,
}

impl ObjBuilder {
    fn elf() -> Self {
        let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let text = obj.section_id(StandardSection::Text);
        Self { obj, text }
    }

    fn coff() -> Self {
        let mut obj = WriteObject::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
        let text = obj.section_id(StandardSection::Text);
        Self { obj, text }
    }

    /// Add a global function symbol with the given code.
    fn func(mut self, name: &str, code: &[u8]) -> Self {
        let off = self.obj.append_section_data(self.text, code, 16);
        self.obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: off, size: code.len() as u64,
            kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
            weak: false, section: SymbolSection::Section(self.text), flags: SymbolFlags::None,
        });
        self
    }

    /// Add a function that calls an undefined symbol via PLT32/REL32 relocation.
    fn func_calling(mut self, name: &str, callee: &str) -> Self {
        let code = &[0xE8, 0, 0, 0, 0, 0xC3]; // call rel32; ret
        let off = self.obj.append_section_data(self.text, code, 16);
        let callee_sym = self.obj.add_symbol(Symbol {
            name: callee.as_bytes().to_vec(),
            value: 0, size: 0,
            kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
            weak: false, section: SymbolSection::Undefined, flags: SymbolFlags::None,
        });
        self.obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: off, size: code.len() as u64,
            kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
            weak: false, section: SymbolSection::Section(self.text), flags: SymbolFlags::None,
        });
        let r_type = if self.obj.format() == BinaryFormat::Coff {
            RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_REL32 }
        } else {
            RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 }
        };
        self.obj.add_relocation(self.text, object::write::Relocation {
            offset: off + 1, symbol: callee_sym, addend: -4, flags: r_type,
        }).unwrap();
        self
    }

    /// Add a data section with a pointer relocation (R_X86_64_64) to the named symbol.
    fn data_ptr_to(mut self, target_sym_name: &str) -> Self {
        let data_sec = self.obj.section_id(StandardSection::Data);
        let ptr_off = self.obj.append_section_data(data_sec, &[0u8; 8], 8);
        let sym = self.obj.symbol_id(target_sym_name.as_bytes())
            .unwrap_or_else(|| panic!("symbol `{target_sym_name}` not found in object"));
        let r_type = if self.obj.format() == BinaryFormat::Coff {
            RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_ADDR64 }
        } else {
            RelocationFlags::Elf { r_type: elf::R_X86_64_64 }
        };
        self.obj.add_relocation(data_sec, object::write::Relocation {
            offset: ptr_off, symbol: sym, addend: 0, flags: r_type,
        }).unwrap();
        self
    }

    /// Add a weak function symbol (COFF weak external pattern).
    fn weak_func(mut self, name: &str, code: &[u8]) -> Self {
        let off = self.obj.append_section_data(self.text, code, 16);
        self.obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: off, size: code.len() as u64,
            kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
            weak: true, section: SymbolSection::Section(self.text), flags: SymbolFlags::None,
        });
        self
    }

    /// Add an undefined symbol reference (no relocation).
    fn undefined(mut self, name: &str) -> Self {
        self.obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: 0, size: 0,
            kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
            weak: false, section: SymbolSection::Undefined, flags: SymbolFlags::None,
        });
        self
    }

    /// Get mutable access to the underlying WriteObject for advanced construction.
    fn inner_mut(&mut self) -> &mut WriteObject<'static> {
        &mut self.obj
    }

    fn build(self) -> Vec<u8> {
        self.obj.write().unwrap()
    }

    fn named(self, name: &str) -> (String, Vec<u8>) {
        (name.into(), self.build())
    }
}

fn build_minimal_obj(name: &str, code: &[u8]) -> Vec<u8> {
    ObjBuilder::elf().func(name, code).build()
}

fn build_minimal_coff(name: &str, code: &[u8]) -> Vec<u8> {
    ObjBuilder::coff().func(name, code).build()
}

// ── Helpers: ELF parsing ─────────────────────────────────────────────────

fn parse_elf(data: &[u8]) -> ElfFile64<'_> {
    ElfFile64::parse(data).expect("output should be valid ELF")
}

fn has_phdr(elf: &ElfFile64<'_>, p_type: u32) -> bool {
    let endian = elf.endian();
    elf.elf_header()
        .program_headers(endian, elf.data())
        .unwrap()
        .iter()
        .any(|ph| ph.p_type.get(endian) == p_type)
}

fn find_section<'a>(elf: &'a ElfFile64<'a>, name: &str) -> Option<object::read::elf::ElfSection64<'a, 'a>> {
    elf.sections().find(|s| s.name().unwrap_or("") == name)
}

fn dynsym_names(elf: &ElfFile64<'_>) -> Vec<String> {
    elf.dynamic_symbols()
        .filter_map(|s| s.name().ok().map(|n| n.to_string()))
        .filter(|n| !n.is_empty())
        .collect()
}

/// Parse .dynamic section entries, returning (tag, value) pairs.
fn parse_dynamic(elf: &ElfFile64<'_>) -> Vec<(i64, u64)> {
    let endian = elf.endian();
    let phdrs = elf.elf_header().program_headers(endian, elf.data()).unwrap();
    let dyn_phdr = phdrs.iter().find(|ph| ph.p_type.get(endian) == elf::PT_DYNAMIC)
        .expect("should have PT_DYNAMIC");
    let off = dyn_phdr.p_offset.get(endian) as usize;
    let size = dyn_phdr.p_filesz.get(endian) as usize;
    let data = &elf.data()[off..off + size];
    let mut entries = Vec::new();
    for chunk in data.chunks_exact(16) {
        let tag = i64::from_le_bytes(chunk[0..8].try_into().unwrap());
        let val = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
        entries.push((tag, val));
        if tag == elf::DT_NULL as i64 { break; }
    }
    entries
}

// ── Helpers: PE parsing ──────────────────────────────────────────────────

fn pe_u16(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(data[off..off + 2].try_into().unwrap())
}

fn pe_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
}

fn pe_u64(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
}

// ── Tests: hand-crafted objects ──────────────────────────────────────────

#[test]
fn basic_single_function() {
    // Minimal x86-64 exit(0) stub
    let code = vec![0x31, 0xFF, 0xB8, 0x3C, 0x00, 0x00, 0x00, 0x0F, 0x05];
    let obj_data = build_minimal_obj("_start", &code);

    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj_data)], "_start")
        .expect("linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();

    assert_eq!(elf.elf_header().e_type.get(endian), 3, "should be ET_DYN");
    assert_eq!(elf.elf_header().e_machine.get(endian), 62, "should be x86_64");
    let entry = elf.elf_header().e_entry.get(endian);
    assert!(entry > 0, "entry should be nonzero");
    assert!(has_phdr(&elf, elf::PT_LOAD), "should have PT_LOAD");
}

#[test]
fn cross_object_symbol_resolution() {
    let objects = vec![
        ObjBuilder::elf().func("helper", &[0xC3]).named("a.o"),
        ObjBuilder::elf().func_calling("_start", "helper").named("b.o"),
    ];

    let elf_bytes =
        toyos_ld::link(&objects, "_start").expect("cross-object linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let entry = elf.elf_header().e_entry.get(elf.endian());
    assert!(entry > 0, "entry should be nonzero");
}

#[test]
fn undefined_symbol_error() {
    let obj = ObjBuilder::elf().func_calling("_start", "nonexistent").build();
    let result = toyos_ld::link(&[("test.o".into(), obj)], "_start");
    let syms = result.expect_err("should fail with undefined symbol");
    assert!(syms.contains(&"nonexistent".to_string()));
}

#[test]
fn absolute_relocation_produces_relative() {
    // R_X86_64_64 relocation should produce R_X86_64_RELATIVE in output
    let obj = ObjBuilder::elf().func("_start", &[0xC3]).data_ptr_to("_start").build();
    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj)], "_start")
        .expect("R_X86_64_64 linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let rela_sec = find_section(&elf, ".rela.dyn");
    assert!(rela_sec.is_some(), "should have .rela.dyn section");
    let rela_data = rela_sec.unwrap().data().unwrap();
    assert!(!rela_data.is_empty(), ".rela.dyn should have entries");
}

// ── Tests: compiled no_std programs ──────────────────────────────────────

#[test]
fn compiled_nostd_minimal() {
    if !has_rustc() {
        return;
    }

    let source = r#"
#![no_main]
#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! { loop {} }

#[no_mangle]
pub extern "C" fn _start() -> ! {
    loop {}
}
"#;
    let obj = compile_to_obj(source);
    let elf_bytes =
        toyos_ld::link(&[("test.o".into(), obj)], "_start").expect("no_std linking should succeed");

    let elf = parse_elf(&elf_bytes);
    assert_eq!(elf.elf_header().e_type.get(elf.endian()), 3);
}

#[test]
fn tls_relocation_handling() {
    if !has_rustc() {
        return;
    }

    let source = r#"
#![no_main]
#![no_std]
#![feature(thread_local)]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! { loop {} }

#[thread_local]
static mut TLS_VAR: u64 = 42;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    unsafe { TLS_VAR = TLS_VAR.wrapping_add(1); }
    loop {}
}
"#;
    let obj = compile_to_obj(source);
    let libdir = sysroot_libdir();
    let libs: Vec<String> = ["core", "compiler_builtins"]
        .iter().map(|s| s.to_string()).collect();
    let lib_objects = toyos_ld::resolve_libs(&[], &[libdir], &libs);
    let mut all_objects = vec![("test.o".into(), obj)];
    all_objects.extend(lib_objects);
    let elf_bytes = toyos_ld::link(&all_objects, "_start")
        .expect("TLS linking should succeed");

    let elf = parse_elf(&elf_bytes);
    assert!(has_phdr(&elf, elf::PT_TLS), "should have PT_TLS");
}

// ── Tests: programs that need std rlibs ──────────────────────────────────

#[test]
fn allocator_shim_synthesis() {
    if !has_rustc() {
        return;
    }

    // Use a program with std (which provides allocator) + Box to trigger shim synthesis
    let source = r#"
fn main() {
    let x = Box::new(42u64);
    println!("{}", x);
}
"#;
    let obj = compile_to_obj(source);
    let libdir = sysroot_libdir();
    let libs = std_rlibs();
    let lib_objects = toyos_ld::resolve_libs(&[], &[libdir], &libs);

    let mut all_objects = vec![("test.o".into(), obj)];
    all_objects.extend(lib_objects);

    let elf_bytes =
        toyos_ld::link(&all_objects, "main").expect("alloc shim linking should succeed");

    let elf = parse_elf(&elf_bytes);
    assert_eq!(elf.elf_header().e_type.get(elf.endian()), 3);
}

#[test]
fn full_std_hello_world() {
    if !has_rustc() {
        return;
    }

    let source = r#"
fn main() {
    println!("Hello from toyos-ld!");
}
"#;
    let obj = compile_to_obj(source);
    let libdir = sysroot_libdir();
    let libs = std_rlibs();
    let lib_objects = toyos_ld::resolve_libs(&[], &[libdir], &libs);

    let mut all_objects = vec![("test.o".into(), obj)];
    all_objects.extend(lib_objects);

    let elf_bytes =
        toyos_ld::link(&all_objects, "main").expect("full std hello world should link");

    let elf = parse_elf(&elf_bytes);
    assert_eq!(elf.elf_header().e_type.get(elf.endian()), 3);
    assert!(has_phdr(&elf, elf::PT_LOAD));
}

// ── Tests: shared library output (--shared) ──────────────────────────────

#[test]
fn shared_basic_output() {
    let code = vec![0xC3]; // ret
    let obj_data = build_minimal_obj("my_func", &code);

    let elf_bytes = toyos_ld::link_shared(&[("test.o".into(), obj_data)])
        .expect("shared linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();

    assert_eq!(elf.elf_header().e_type.get(endian), 3, "should be ET_DYN");
    assert_eq!(elf.elf_header().e_machine.get(endian), 62, "should be x86_64");
    assert!(has_phdr(&elf, elf::PT_LOAD), "should have PT_LOAD");
    assert!(has_phdr(&elf, elf::PT_DYNAMIC), "should have PT_DYNAMIC");
}

#[test]
fn shared_has_dynsym_section() {
    let obj_data = build_minimal_obj("exported_func", &[0xC3]);

    let elf_bytes = toyos_ld::link_shared(&[("test.o".into(), obj_data)])
        .expect("shared linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let dynsym = find_section(&elf, ".dynsym");
    assert!(dynsym.is_some(), "should have .dynsym section");

    let dynstr = find_section(&elf, ".dynstr");
    assert!(dynstr.is_some(), "should have .dynstr section");

    let dynamic = find_section(&elf, ".dynamic");
    assert!(dynamic.is_some(), "should have .dynamic section");
}

#[test]
fn shared_exports_global_symbols() {
    let obj = ObjBuilder::elf().func("func_alpha", &[0xC3]).func("func_beta", &[0xC3]).build();
    let elf_bytes = toyos_ld::link_shared(&[("test.o".into(), obj)])
        .expect("shared linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let names = dynsym_names(&elf);
    assert!(names.contains(&"func_alpha".to_string()), "dynsym should contain func_alpha");
    assert!(names.contains(&"func_beta".to_string()), "dynsym should contain func_beta");
}

#[test]
fn shared_cross_object_resolution() {
    let objects = vec![
        ObjBuilder::elf().func("helper", &[0xC3]).named("a.o"),
        ObjBuilder::elf().func_calling("entry", "helper").named("b.o"),
    ];

    let elf_bytes = toyos_ld::link_shared(&objects)
        .expect("cross-object shared linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let names = dynsym_names(&elf);
    assert!(names.contains(&"helper".to_string()));
    assert!(names.contains(&"entry".to_string()));
}

#[test]
fn shared_dynamic_has_symtab_strtab() {
    let obj_data = build_minimal_obj("my_sym", &[0xC3]);
    let elf_bytes = toyos_ld::link_shared(&[("test.o".into(), obj_data)])
        .expect("shared linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let dyn_entries = parse_dynamic(&elf);

    let has = |tag: u32| dyn_entries.iter().any(|&(t, _)| t == tag as i64);
    assert!(has(elf::DT_SYMTAB), ".dynamic should have DT_SYMTAB");
    assert!(has(elf::DT_STRTAB), ".dynamic should have DT_STRTAB");
    assert!(has(elf::DT_NULL), ".dynamic should terminate with DT_NULL");
}

#[test]
fn shared_relative_relocations() {
    // R_X86_64_64 in shared lib should produce R_X86_64_RELATIVE
    let obj = ObjBuilder::elf().func("my_func", &[0xC3]).data_ptr_to("my_func").build();
    let elf_bytes = toyos_ld::link_shared(&[("test.o".into(), obj)])
        .expect("shared lib with R_X86_64_64 should link");

    let elf = parse_elf(&elf_bytes);
    let rela = find_section(&elf, ".rela.dyn");
    assert!(rela.is_some(), "shared lib should have .rela.dyn");
    assert!(!rela.unwrap().data().unwrap().is_empty(), ".rela.dyn should have entries");
}

#[test]
fn shared_allows_undefined_symbols() {
    let obj = ObjBuilder::elf().func_calling("my_func", "main").build();
    let result = toyos_ld::link_shared(&[("test.o".into(), obj)]);
    assert!(result.is_ok(), "shared lib should allow undefined symbols");
}

// ── Tests: linking against .so (dynamic library inputs) ──────────────────

#[test]
fn link_against_so_resolves_symbols() {
    let so_bytes = toyos_ld::link_shared(&[("helper.o".into(),
        build_minimal_obj("helper", &[0xB8, 0x2A, 0x00, 0x00, 0x00, 0xC3]))])
        .expect("shared lib should link");

    let obj = ObjBuilder::elf().func_calling("_start", "helper").build();
    let result = toyos_ld::link(
        &[("main.o".into(), obj), ("libhelper.so".into(), so_bytes)], "_start");
    assert!(result.is_ok(), "linking against .so should resolve `helper`: {:?}", result.err());
}

#[test]
fn link_against_so_does_not_include_so_content() {
    let so_bytes = toyos_ld::link_shared(&[("helper.o".into(),
        build_minimal_obj("helper", &[0xB8, 0x2A, 0x00, 0x00, 0x00, 0xC3]))])
        .expect("shared lib should link");

    let start_obj = build_minimal_obj("_start", &[0xC3]);
    let without_so = toyos_ld::link(&[("main.o".into(), start_obj.clone())], "_start")
        .expect("should link without .so");
    let with_so = toyos_ld::link(
        &[("main.o".into(), start_obj), ("libhelper.so".into(), so_bytes)], "_start",
    ).expect("should link with .so");

    assert!(with_so.len() > without_so.len(),
        "dynamic executable should be larger due to dynamic sections");
    let helper_code = [0xB8, 0x2A, 0x00, 0x00, 0x00, 0xC3];
    assert!(!with_so.windows(helper_code.len()).any(|w| w == helper_code),
        "output should not contain the .so's code");
}

#[test]
fn link_against_so_still_reports_truly_undefined() {
    let so_bytes = toyos_ld::link_shared(&[("helper.o".into(), build_minimal_obj("helper", &[0xC3]))])
        .expect("shared lib should link");

    let obj = ObjBuilder::elf().func_calling("_start", "missing_func").build();
    let result = toyos_ld::link(
        &[("main.o".into(), obj), ("libhelper.so".into(), so_bytes)], "_start");
    let err = result.expect_err("should fail for truly undefined symbol");
    assert!(err.contains(&"missing_func".to_string()));
}

#[test]
fn shared_preserves_rustc_metadata_section() {
    let mut b = ObjBuilder::elf().func("my_func", &[0xC3]);
    let rustc_section = b.inner_mut().add_section(vec![], b".rustc".to_vec(), object::SectionKind::ReadOnlyData);
    let metadata = b"RUSTC_METADATA_TEST_1234567890";
    b.inner_mut().append_section_data(rustc_section, metadata, 1);

    let elf_bytes = toyos_ld::link_shared(&[("test.o".into(), b.build())])
        .expect("shared lib with .rustc should link");

    let elf = parse_elf(&elf_bytes);
    let rustc_sec = find_section(&elf, ".rustc");
    assert!(rustc_sec.is_some(), "output should have .rustc section");
    let data = rustc_sec.unwrap().data().unwrap();
    assert_eq!(data, metadata, ".rustc section data should be preserved");
}

#[test]
fn dynamic_executable_has_dt_needed_and_glob_dat() {
    let so_obj = ObjBuilder::elf()
        .func("helper", &[0xB8, 0x2A, 0x00, 0x00, 0x00, 0xC3])
        .func("_start", &[0xC3]);
    let so_bytes = toyos_ld::link_shared(&[so_obj.named("helper.o")])
        .expect("shared lib should link");

    let obj = ObjBuilder::elf().func_calling("main", "helper").build();
    let result = toyos_ld::link(
        &[("main.o".into(), obj), ("libmylib.so".into(), so_bytes)], "_start");
    assert!(result.is_ok(), "should link dynamic executable: {:?}", result.err());
    let elf_bytes = result.unwrap();
    let elf = parse_elf(&elf_bytes);

    // Verify PT_DYNAMIC exists
    assert!(has_phdr(&elf, elf::PT_DYNAMIC), "should have PT_DYNAMIC");

    // Verify .dynamic section has DT_NEEDED
    let dyn_entries = parse_dynamic(&elf);
    let needed_offsets: Vec<u64> = dyn_entries.iter()
        .filter(|&&(tag, _)| tag == elf::DT_NEEDED as i64)
        .map(|&(_, val)| val)
        .collect();
    assert!(!needed_offsets.is_empty(), "should have at least one DT_NEEDED");

    // Read the library name from .dynstr
    let dynstr_sec = find_section(&elf, ".dynstr").expect(".dynstr section");
    let dynstr_data = dynstr_sec.data().unwrap();
    let name_offset = needed_offsets[0] as usize;
    let name_end = dynstr_data[name_offset..].iter().position(|&b| b == 0).unwrap();
    let lib_name = std::str::from_utf8(&dynstr_data[name_offset..name_offset + name_end]).unwrap();
    assert_eq!(lib_name, "libmylib.so", "DT_NEEDED should reference the .so filename");

    // Verify .dynsym has undefined import symbols
    let dynsym_sec = find_section(&elf, ".dynsym").expect(".dynsym section");
    let dynsym_data = dynsym_sec.data().unwrap();
    assert!(dynsym_data.len() > 24, "should have import symbols beyond null entry");

    // Verify .rela.dyn has GLOB_DAT entries
    let rela_sec = find_section(&elf, ".rela.dyn").expect(".rela.dyn section");
    let rela_data = rela_sec.data().unwrap();
    let mut has_glob_dat = false;
    for chunk in rela_data.chunks_exact(24) {
        let r_info = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
        let r_type = (r_info & 0xFFFFFFFF) as u32;
        if r_type == 6 { // R_X86_64_GLOB_DAT
            has_glob_dat = true;
        }
    }
    assert!(has_glob_dat, "should have R_X86_64_GLOB_DAT relocations");

    // Entry point should be valid (PLT stub address for _start)
    let endian = elf.endian();
    let entry = elf.elf_header().e_entry.get(endian);
    assert!(entry > 0, "entry should be non-zero (PLT stub for _start)");
}

// ── Tests: static ELF output (link_static) ────────────────────────────────

#[test]
fn static_produces_et_exec() {
    let code = vec![0x31, 0xFF, 0xB8, 0x3C, 0x00, 0x00, 0x00, 0x0F, 0x05]; // exit(0) stub
    let obj_data = build_minimal_obj("_start", &code);

    let elf_bytes = toyos_ld::link_static(
        &[("test.o".into(), obj_data)],
        "_start",
        0x200000,
    ).expect("static linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();

    assert_eq!(elf.elf_header().e_type.get(endian), 2, "should be ET_EXEC (2), not ET_DYN (3)");
    assert_eq!(elf.elf_header().e_machine.get(endian), 62, "should be x86_64");
    let entry = elf.elf_header().e_entry.get(endian);
    assert!(entry >= 0x200000, "entry should be in virtual address space");
    assert!(has_phdr(&elf, elf::PT_LOAD), "should have PT_LOAD");
}

#[test]
fn static_no_dynamic_section() {
    let obj_data = build_minimal_obj("_start", &[0xC3]);

    let elf_bytes = toyos_ld::link_static(
        &[("test.o".into(), obj_data)],
        "_start",
        0x200000,
    ).expect("static linking should succeed");

    let elf = parse_elf(&elf_bytes);

    assert!(!has_phdr(&elf, elf::PT_DYNAMIC), "static ELF must not have PT_DYNAMIC");
    assert!(find_section(&elf, ".dynsym").is_none(), "static ELF must not have .dynsym");
    assert!(find_section(&elf, ".dynstr").is_none(), "static ELF must not have .dynstr");
    assert!(find_section(&elf, ".dynamic").is_none(), "static ELF must not have .dynamic");
}

#[test]
fn static_no_relative_relocations() {
    // R_X86_64_64 in a static executable should be directly patched,
    // producing NO R_X86_64_RELATIVE entries in the output
    let obj = ObjBuilder::elf().func("_start", &[0xC3]).data_ptr_to("_start").build();
    let elf_bytes = toyos_ld::link_static(
        &[("test.o".into(), obj)], "_start", 0x200000,
    ).expect("static linking with R_X86_64_64 should succeed");

    let elf = parse_elf(&elf_bytes);

    // There should be NO .rela.dyn section at all — addresses are absolute
    assert!(find_section(&elf, ".rela.dyn").is_none(),
        "static ELF must not have .rela.dyn (all relocations resolved at link time)");

    // Verify the data section contains the absolute address of _start
    let entry = elf.elf_header().e_entry.get(elf.endian());
    // The 8-byte pointer in .data should contain the absolute entry address
    let data_section = elf.sections()
        .find(|s| s.name().unwrap_or("") == ".data")
        .expect("should have .data section");
    let data = data_section.data().unwrap();
    let stored_addr = u64::from_le_bytes(data[..8].try_into().unwrap());
    assert_eq!(stored_addr, entry,
        "R_X86_64_64 should be resolved to absolute address in static ELF");
}

#[test]
fn static_high_base_address() {
    let code = vec![0xC3]; // ret
    let obj_data = build_minimal_obj("_start", &code);

    let high_base: u64 = 0xFFFF_8000_0000_0000;
    let elf_bytes = toyos_ld::link_static(
        &[("test.o".into(), obj_data)],
        "_start",
        high_base,
    ).expect("static linking with high base should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();

    assert_eq!(elf.elf_header().e_type.get(endian), 2, "should be ET_EXEC");
    let entry = elf.elf_header().e_entry.get(endian);
    assert!(entry >= high_base, "entry {entry:#x} should be above {high_base:#x}");

    // Verify program headers reference high addresses
    let phdrs = elf.elf_header().program_headers(endian, elf.data()).unwrap();
    for ph in phdrs.iter() {
        if ph.p_type.get(endian) == elf::PT_LOAD {
            let vaddr = ph.p_vaddr.get(endian);
            assert!(vaddr >= high_base,
                "PT_LOAD vaddr {vaddr:#x} should be above {high_base:#x}");
        }
    }
}

#[test]
fn static_cross_object_resolution() {
    let objects = vec![
        ObjBuilder::elf().func("helper", &[0xC3]).named("a.o"),
        ObjBuilder::elf().func_calling("_start", "helper").named("b.o"),
    ];

    let elf_bytes = toyos_ld::link_static(&objects, "_start", 0x200000)
        .expect("cross-object static linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();
    assert_eq!(elf.elf_header().e_type.get(endian), 2, "should be ET_EXEC");
    let entry = elf.elf_header().e_entry.get(endian);
    assert!(entry >= 0x200000);
}

#[test]
fn static_undefined_symbol_error() {
    let obj = ObjBuilder::elf().func_calling("_start", "nonexistent").build();
    let result = toyos_ld::link_static(&[("test.o".into(), obj)], "_start", 0x200000);
    let syms = result.expect_err("should fail with undefined symbol");
    assert!(syms.contains(&"nonexistent".to_string()));
}

#[test]
fn static_compiled_nostd_kernel_base() {
    if !has_rustc() { return; }

    let source = r#"
#![no_main]
#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! { loop {} }

#[no_mangle]
pub extern "C" fn _start() -> ! {
    loop {}
}
"#;
    let obj = compile_to_obj(source);
    let high_base: u64 = 0xFFFF_8000_0000_0000;
    let elf_bytes = toyos_ld::link_static(&[("test.o".into(), obj)], "_start", high_base)
        .expect("no_std static linking at kernel base should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();
    assert_eq!(elf.elf_header().e_type.get(endian), 2, "should be ET_EXEC");
    let entry = elf.elf_header().e_entry.get(endian);
    assert!(entry >= high_base, "entry should be in high kernel address space");
}

#[test]
fn static_compiled_nostd() {
    if !has_rustc() { return; }

    let source = r#"
#![no_main]
#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! { loop {} }

#[no_mangle]
pub extern "C" fn _start() -> ! {
    loop {}
}
"#;
    let obj = compile_to_obj(source);
    let elf_bytes = toyos_ld::link_static(&[("test.o".into(), obj)], "_start", 0x200000)
        .expect("no_std static linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();
    assert_eq!(elf.elf_header().e_type.get(endian), 2, "should be ET_EXEC");
    assert!(!has_phdr(&elf, elf::PT_DYNAMIC), "must not have PT_DYNAMIC");
}

// ── Tests: PE/COFF output (link_pe) ─────────────────────────────────────

fn pe_section_name(data: &[u8], sh_off: usize) -> String {
    let raw = &data[sh_off..sh_off + 8];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(8);
    String::from_utf8_lossy(&raw[..end]).to_string()
}

fn pe_section_characteristics(data: &[u8], sh_off: usize) -> u32 {
    pe_u32(data, sh_off + 36)
}

#[test]
fn pe_valid_headers() {
    let obj_data = build_minimal_obj("efi_main", &[0xC3]);

    let pe = toyos_ld::link_pe(
        &[("test.o".into(), obj_data)],
        "efi_main",
        10, // EFI_APPLICATION
    ).expect("PE linking should succeed");

    // DOS header
    assert_eq!(pe_u16(&pe, 0), 0x5A4D, "should start with MZ magic");
    let pe_offset = pe_u32(&pe, 0x3C) as usize;
    assert_eq!(pe_offset, 0x40, "e_lfanew should point to PE signature");

    // PE signature
    assert_eq!(pe_u32(&pe, pe_offset), 0x00004550, "should have PE\\0\\0 signature");

    // COFF header
    let coff = pe_offset + 4;
    assert_eq!(pe_u16(&pe, coff), 0x8664, "Machine should be AMD64");

    // Optional header
    let oh = coff + 20;
    assert_eq!(pe_u16(&pe, oh), 0x020B, "should be PE32+ (0x020B)");
}

#[test]
fn pe_sections() {
    let obj_data = build_minimal_obj("efi_main", &[0xC3]);

    let pe = toyos_ld::link_pe(
        &[("test.o".into(), obj_data)],
        "efi_main",
        10,
    ).expect("PE linking should succeed");

    let coff = 0x44;
    let num_sections = pe_u16(&pe, coff + 2) as usize;
    assert!(num_sections >= 2, "should have at least .text and .reloc sections");

    let sh_base = 0x58 + 240; // after optional header
    let mut found_text = false;
    let mut found_reloc = false;
    for i in 0..num_sections {
        let sh = sh_base + i * 40;
        let name = pe_section_name(&pe, sh);
        let chars = pe_section_characteristics(&pe, sh);
        match name.as_str() {
            ".text" => {
                found_text = true;
                assert_eq!(chars & 0x60000020, 0x60000020,
                    ".text should have CODE|EXECUTE|READ");
            }
            ".data" => {
                assert_eq!(chars & 0xC0000040, 0xC0000040,
                    ".data should have INIT_DATA|READ|WRITE");
            }
            ".reloc" => {
                found_reloc = true;
                assert_eq!(chars & 0x42000040, 0x42000040,
                    ".reloc should have INIT_DATA|DISCARDABLE|READ");
            }
            _ => {}
        }
    }
    assert!(found_text, "should have .text section");
    assert!(found_reloc, "should have .reloc section");
}

#[test]
fn pe_entry_point() {
    // Two functions to verify entry points to the right one
    let obj = ObjBuilder::elf()
        .func("dummy", &[0x90, 0xC3])    // nop; ret
        .func("efi_main", &[0x31, 0xC0, 0xC3]) // xor eax,eax; ret
        .build();
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj)], "efi_main", 10)
        .expect("PE linking should succeed");

    let oh = 0x58;
    let entry_rva = pe_u32(&pe, oh + 0x10);
    assert!(entry_rva > 0, "AddressOfEntryPoint should be non-zero");

    // Entry should NOT be at the very start of .text (dummy is there)
    let text_rva = pe_u32(&pe, oh + 0x14);
    assert!(entry_rva > text_rva, "entry should not be at start of .text (dummy func is there)");
}

#[test]
fn pe_subsystem() {
    let obj_data = build_minimal_obj("efi_main", &[0xC3]);

    // Test EFI_APPLICATION (10)
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj_data.clone())], "efi_main", 10).unwrap();
    let oh = 0x58;
    assert_eq!(pe_u16(&pe, oh + 0x44), 10, "subsystem should be EFI_APPLICATION (10)");

    // Test EFI_BOOT_SERVICE_DRIVER (11)
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj_data.clone())], "efi_main", 11).unwrap();
    assert_eq!(pe_u16(&pe, oh + 0x44), 11, "subsystem should be EFI_BOOT_SERVICE_DRIVER (11)");

    // Test EFI_RUNTIME_DRIVER (12)
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj_data)], "efi_main", 12).unwrap();
    assert_eq!(pe_u16(&pe, oh + 0x44), 12, "subsystem should be EFI_RUNTIME_DRIVER (12)");
}

#[test]
fn pe_base_relocations() {
    // R_X86_64_64 (absolute 64-bit) needs base relocation in PE
    let obj = ObjBuilder::elf().func("efi_main", &[0xC3]).data_ptr_to("efi_main").build();
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj)], "efi_main", 10)
        .expect("PE with R_X86_64_64 should succeed");

    // Find .reloc section
    let coff = 0x44;
    let num_sections = pe_u16(&pe, coff + 2) as usize;
    let sh_base = 0x58 + 240;
    let mut reloc_file_off = 0u32;
    let mut reloc_raw_size = 0u32;
    for i in 0..num_sections {
        let sh = sh_base + i * 40;
        if pe_section_name(&pe, sh) == ".reloc" {
            reloc_file_off = pe_u32(&pe, sh + 20);
            reloc_raw_size = pe_u32(&pe, sh + 16);
        }
    }
    assert!(reloc_file_off > 0, "should have .reloc section");
    assert!(reloc_raw_size > 0, ".reloc should have data");

    // Parse base relocation blocks
    let reloc_data = &pe[reloc_file_off as usize..(reloc_file_off + reloc_raw_size) as usize];
    assert!(reloc_data.len() >= 12, ".reloc should have at least one block with one entry");

    // First block header
    let page_rva = pe_u32(reloc_data, 0);
    let block_size = pe_u32(reloc_data, 4);
    assert!(block_size >= 12, "block should have header + at least one entry");
    assert!(page_rva > 0, "page_rva should be non-zero");

    // Check entries contain DIR64 type (type=10 in upper 4 bits)
    let num_entries = (block_size - 8) / 2;
    let mut has_dir64 = false;
    for e in 0..num_entries {
        let entry = pe_u16(reloc_data, 8 + e as usize * 2);
        let typ = entry >> 12;
        if typ == 10 { has_dir64 = true; }
    }
    assert!(has_dir64, ".reloc should have IMAGE_REL_BASED_DIR64 entries");
}

#[test]
fn pe_no_relocs_for_pc_relative() {
    // R_X86_64_PLT32 (PC-relative) should NOT produce base relocations
    let obj = ObjBuilder::elf().func("helper", &[0xC3]).func_calling("efi_main", "helper").build();
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj)], "efi_main", 10)
        .expect("PE with only PC-relative relocs should succeed");

    // .reloc should exist but have no DIR64 entries (only padding or empty)
    let oh = 0x58;
    // Data directory 5 (base relocation) should have size 0
    let dd5 = oh + 0x70 + 5 * 8;
    let reloc_dir_size = pe_u32(&pe, dd5 + 4);
    assert_eq!(reloc_dir_size, 0, "no base relocations needed for PC-relative only code");
}

#[test]
fn pe_undefined_symbol_error() {
    let obj = ObjBuilder::elf().func_calling("efi_main", "missing").build();
    let result = toyos_ld::link_pe(&[("test.o".into(), obj)], "efi_main", 10);
    let syms = result.expect_err("should fail with undefined symbol");
    assert!(syms.contains(&"missing".to_string()));
}

#[test]
fn pe_dll_characteristics_dynamic_base() {
    // UEFI PE must have DYNAMIC_BASE set so firmware applies base relocations
    let obj_data = build_minimal_obj("efi_main", &[0xC3]);
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj_data)], "efi_main", 10)
        .expect("PE linking should succeed");

    let pe_off = pe_u32(&pe, 0x3C) as usize;
    let oh = pe_off + 4 + 20;
    let dll_chars = pe_u16(&pe, oh + 70);

    let dynamic_base = 0x0040u16;
    let nx_compat = 0x0100u16;
    assert!(dll_chars & dynamic_base != 0,
        "DllCharacteristics should have DYNAMIC_BASE ({dll_chars:#06x})");
    assert!(dll_chars & nx_compat != 0,
        "DllCharacteristics should have NX_COMPAT ({dll_chars:#06x})");
}

#[test]
fn pe_stack_heap_sizes() {
    // UEFI PE should have nonzero stack/heap reserve values
    let obj_data = build_minimal_obj("efi_main", &[0xC3]);
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj_data)], "efi_main", 10)
        .expect("PE linking should succeed");

    let pe_off = pe_u32(&pe, 0x3C) as usize;
    let oh = pe_off + 4 + 20;
    let stack_reserve = pe_u64(&pe, oh + 72);
    let stack_commit = pe_u64(&pe, oh + 80);

    assert!(stack_reserve > 0, "StackReserve should be nonzero");
    assert!(stack_commit > 0, "StackCommit should be nonzero");
}

// ── COFF input tests ─────────────────────────────────────────────────────

#[test]
fn coff_input_link_pie() {
    // COFF object should work as input for PIE ELF output
    let code = vec![0x31, 0xFF, 0xB8, 0x3C, 0x00, 0x00, 0x00, 0x0F, 0x05];
    let obj_data = build_minimal_coff("_start", &code);
    let result = toyos_ld::link(
        &[("test.o".into(), obj_data)],
        "_start",
    );
    let elf_bytes = result.expect("linking COFF input should succeed");
    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();
    assert_eq!(elf.elf_header().e_type.get(endian), 3, "should be ET_DYN");
    assert_eq!(elf.elf_header().e_machine.get(endian), 62, "should be x86_64");
    let entry = elf.elf_header().e_entry.get(endian);
    assert!(entry > 0, "entry should be nonzero");
}

#[test]
fn coff_input_link_pe() {
    // COFF object should work as input for PE output
    let obj_data = build_minimal_coff("efi_main", &[0xC3]);
    let result = toyos_ld::link_pe(
        &[("test.o".into(), obj_data)],
        "efi_main",
        10,
    );
    let pe = result.expect("linking COFF→PE should succeed");
    // Verify PE magic
    assert_eq!(&pe[0..2], b"MZ");
    let pe_off = pe_u32(&pe, 0x3C) as usize;
    assert_eq!(&pe[pe_off..pe_off + 4], b"PE\0\0");
}

#[test]
fn coff_input_pc_relative_reloc() {
    let obj = ObjBuilder::coff().func("callee", &[0xC3]).func_calling("_start", "callee").build();
    toyos_ld::link(&[("test.o".into(), obj)], "_start")
        .expect("COFF with REL32 should link successfully");
}

#[test]
fn coff_input_absolute_reloc_pe() {
    // COFF IMAGE_REL_AMD64_ADDR64 should produce base relocations in PE
    let mut obj = WriteObject::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let data = obj.section_id(StandardSection::Data);

    // data: 8 bytes
    let data_off = obj.append_section_data(data, &[0x42; 8], 8);
    let data_sym = obj.add_symbol(Symbol {
        name: b"my_data".to_vec(),
        value: data_off, size: 8,
        kind: SymbolKind::Data, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(data), flags: SymbolFlags::None,
    });

    // text: movabs rax, [my_data] — 8 bytes absolute address, then ret
    let code = [0x48, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0xC3];
    let code_off = obj.append_section_data(text, &code, 16);
    obj.add_relocation(text, object::write::Relocation {
        offset: code_off + 2,
        symbol: data_sym,
        addend: 0,
        flags: RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_ADDR64 },
    }).unwrap();
    obj.add_symbol(Symbol {
        name: b"efi_main".to_vec(),
        value: code_off, size: code.len() as u64,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    let result = toyos_ld::link_pe(
        &[("test.o".into(), obj.write().unwrap())],
        "efi_main",
        10,
    );
    let pe = result.expect("COFF with ADDR64 should link to PE");

    // Verify base relocation directory is non-empty
    let pe_off = pe_u32(&pe, 0x3C) as usize;
    let oh = pe_off + 4 + 20; // optional header start
    let dd5 = oh + 0x70 + 5 * 8; // base relocation data directory
    let reloc_dir_size = pe_u32(&pe, dd5 + 4);
    assert!(reloc_dir_size > 0, "ADDR64 should produce base relocations");
}

#[test]
fn coff_input_cross_object() {
    let objects = vec![
        ObjBuilder::coff().func("callee", &[0xC3]).named("obj1.o"),
        ObjBuilder::coff().func_calling("_start", "callee").named("obj2.o"),
    ];
    toyos_ld::link(&objects, "_start").expect("cross-object COFF linking should succeed");
}

#[test]
fn coff_input_mixed_with_elf() {
    let objects = vec![
        ObjBuilder::elf().func("callee", &[0xC3]).named("elf.o"),
        ObjBuilder::coff().func_calling("_start", "callee").named("coff.o"),
    ];
    toyos_ld::link(&objects, "_start").expect("mixing ELF and COFF objects should link successfully");
}

fn build_coff_with_weak_external(weak_name: &str, code: &[u8]) -> Vec<u8> {
    ObjBuilder::coff().weak_func(weak_name, code).build()
}

#[test]
fn coff_weak_external_resolves() {
    let objects = vec![
        ("builtins.o".into(), build_coff_with_weak_external("memcpy", &[0xC3])),
        ObjBuilder::coff().func_calling("_start", "memcpy").named("caller.o"),
    ];
    toyos_ld::link(&objects, "_start").expect("COFF weak external memcpy should be resolved");
}

#[test]
fn coff_weak_external_pe() {
    // Same test but for PE output — weak externals must also work for PE linking
    let weak_obj = build_coff_with_weak_external("efi_main", &[0xC3]);
    let result = toyos_ld::link_pe(
        &[("builtins.o".into(), weak_obj)],
        "efi_main",
        10,
    );
    result.expect("COFF weak external should resolve for PE output");
}

#[test]
fn coff_weak_external_multiple_builtins() {
    let caller = ObjBuilder::coff().func("_start", &[0xC3])
        .undefined("memcpy").undefined("memset").undefined("__adddf3");
    let objects = vec![
        ("memcpy.o".into(), build_coff_with_weak_external("memcpy", &[0xC3])),
        ("memset.o".into(), build_coff_with_weak_external("memset", &[0xC3])),
        ("adddf3.o".into(), build_coff_with_weak_external("__adddf3", &[0xC3])),
        caller.named("caller.o"),
    ];
    toyos_ld::link(&objects, "_start").expect("multiple COFF weak externals should all resolve");
}

// ── Tests: PIE ELF base address ──────────────────────────────────────────

#[test]
fn pie_base_vaddr_is_zero() {
    // PIE ELF should have LOAD segments starting near vaddr 0, not 0x200000
    let code = vec![0xC3]; // ret
    let obj_data = build_minimal_obj("_start", &code);

    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj_data)], "_start")
        .expect("linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();
    let phdrs = elf.elf_header().program_headers(endian, elf.data()).unwrap();

    let first_load = phdrs.iter()
        .find(|ph| ph.p_type.get(endian) == elf::PT_LOAD)
        .expect("should have PT_LOAD");

    let vaddr = first_load.p_vaddr.get(endian);
    assert!(vaddr < 0x10000,
        "first PT_LOAD vaddr should be near 0 for PIE, got {vaddr:#x}");
}

#[test]
fn pie_entry_is_zero_based() {
    // Entry point should be a small offset (0-based), not 0x200000+offset
    let code = vec![0xC3]; // ret
    let obj_data = build_minimal_obj("_start", &code);

    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj_data)], "_start")
        .expect("linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let entry = elf.elf_header().e_entry.get(elf.endian());
    assert!(entry < 0x10000,
        "entry should be zero-based for PIE, got {entry:#x}");
}

#[test]
fn pie_file_offset_equals_vaddr() {
    // For PIE with base 0, file offset should equal vaddr (p_offset == p_vaddr)
    let code = vec![0xC3];
    let obj_data = build_minimal_obj("_start", &code);

    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj_data)], "_start")
        .expect("linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();
    let phdrs = elf.elf_header().program_headers(endian, elf.data()).unwrap();

    for ph in phdrs.iter().filter(|ph| ph.p_type.get(endian) == elf::PT_LOAD) {
        let vaddr = ph.p_vaddr.get(endian);
        let offset = ph.p_offset.get(endian);
        assert_eq!(offset, vaddr,
            "PT_LOAD p_offset ({offset:#x}) should equal p_vaddr ({vaddr:#x}) for PIE");
    }
}

#[test]
fn pie_relative_relocs_are_zero_based() {
    let obj = ObjBuilder::elf().func("_start", &[0xC3]).data_ptr_to("_start").build();
    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj)], "_start")
        .expect("linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let rela_sec = find_section(&elf, ".rela.dyn").expect("should have .rela.dyn");
    let rela_data = rela_sec.data().unwrap();

    // Parse first RELA entry: offset(8) + info(8) + addend(8)
    assert!(rela_data.len() >= 24, "should have at least one RELA entry");
    let addend = i64::from_le_bytes(rela_data[16..24].try_into().unwrap());
    // Addend should point to _start which should be near 0, not 0x200000+
    assert!(addend < 0x10000,
        "RELATIVE addend should be zero-based, got {addend:#x}");
}

#[test]
fn pie_bootloader_loadable() {
    // Simulate bootloader loading: allocate based on max(vaddr+memsz),
    // copy segments at vaddr offsets, verify entry code is at the right place
    let code: Vec<u8> = vec![
        0x48, 0x31, 0xFF, // xor rdi, rdi
        0xB8, 0x3C, 0x00, 0x00, 0x00, // mov eax, 60
        0x0F, 0x05, // syscall
    ];
    let obj_data = build_minimal_obj("_start", &code);

    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj_data)], "_start")
        .expect("linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();
    let phdrs = elf.elf_header().program_headers(endian, elf.data()).unwrap();

    // Calculate total memory size (like bootloader does)
    let mut mem_size: usize = 0;
    for ph in phdrs.iter().filter(|ph| ph.p_type.get(endian) == elf::PT_LOAD) {
        let end = ph.p_vaddr.get(endian) + ph.p_memsz.get(endian);
        mem_size = mem_size.max(end as usize);
    }

    // For PIE, mem_size should be reasonable (not 2MB+ wasted)
    assert!(mem_size < 0x100000,
        "total memory for simple PIE should be < 1MB, got {mem_size:#x}");

    // Load segments into simulated memory
    let mut process_mem = vec![0u8; mem_size];
    for ph in phdrs.iter().filter(|ph| ph.p_type.get(endian) == elf::PT_LOAD) {
        let fstart = ph.p_offset.get(endian) as usize;
        let fend = fstart + ph.p_filesz.get(endian) as usize;
        let vstart = ph.p_vaddr.get(endian) as usize;
        let vend = vstart + ph.p_filesz.get(endian) as usize;
        process_mem[vstart..vend].copy_from_slice(&elf_bytes[fstart..fend]);
    }

    // Verify entry point code is at the right place in process memory
    let entry = elf.elf_header().e_entry.get(endian) as usize;
    assert!(entry + code.len() <= process_mem.len(),
        "entry ({entry:#x}) + code should fit in process memory");
    assert_eq!(&process_mem[entry..entry + code.len()], &code,
        "code at entry point should match original");
}

// ── Tests: COFF implicit addend and section classification ───────────────

/// Helper: parse PE section info. Returns vec of (name, va, virt_size, raw_ptr, raw_size).
fn pe_section_list(pe: &[u8]) -> Vec<(String, u32, u32, u32, u32)> {
    let pe_off = pe_u32(pe, 0x3C) as usize;
    let coff = pe_off + 4;
    let num_secs = pe_u16(pe, coff + 2) as usize;
    let opt_sz = pe_u16(pe, coff + 16) as usize;
    let sh_start = coff + 20 + opt_sz;
    (0..num_secs).map(|i| {
        let sh = sh_start + i * 40;
        let name = pe_section_name(pe, sh);
        let vs = pe_u32(pe, sh + 8);
        let va = pe_u32(pe, sh + 12);
        let rs = pe_u32(pe, sh + 16);
        let rp = pe_u32(pe, sh + 20);
        (name, va, vs, rp, rs)
    }).collect()
}

/// Read bytes from a PE at a given RVA, using section headers to map RVA→file offset.
fn pe_read_at_rva(pe: &[u8], rva: u32, len: usize) -> &[u8] {
    for (_, va, vs, rp, _) in pe_section_list(pe) {
        if rva >= va && rva + len as u32 <= va + vs {
            let off = (rp + (rva - va)) as usize;
            return &pe[off..off + len];
        }
    }
    panic!("RVA {rva:#x} not in any PE section");
}

fn pe_read_i32_at_rva(pe: &[u8], rva: u32) -> i32 {
    let b = pe_read_at_rva(pe, rva, 4);
    i32::from_le_bytes(b.try_into().unwrap())
}

fn pe_entry_rva(pe: &[u8]) -> u32 {
    let pe_off = pe_u32(pe, 0x3C) as usize;
    pe_u32(pe, pe_off + 4 + 20 + 16) // OptionalHeader.AddressOfEntryPoint
}

#[test]
fn coff_cross_object_call_displacement() {
    let objects = vec![
        ("callee.o".into(), build_minimal_coff("callee", &[0xC3])),
        ObjBuilder::coff().func_calling("efi_main", "callee").named("caller.o"),
    ];
    let pe = toyos_ld::link_pe(&objects, "efi_main", 10).expect("linking should succeed");

    let entry_rva = pe_entry_rva(&pe);
    let disp = pe_read_i32_at_rva(&pe, entry_rva + 1);
    let target_rva = (entry_rva as i64 + 1 + 4 + disp as i64) as u32;

    // callee is in the first .text (placed before caller's .text)
    let secs = pe_section_list(&pe);
    let text_rva = secs[0].1;
    assert_eq!(pe_read_at_rva(&pe, text_rva, 1), &[0xC3], "callee should be at text_rva");
    assert_eq!(target_rva, text_rva,
        "call target should be callee RVA: disp={disp}, got {target_rva:#x}, want {text_rva:#x}");
}

#[test]
fn coff_jump_table_implicit_addend() {
    // Simulates a switch/jump table in .rdata with REL32 relocations.
    // Each entry has a different implicit addend (compensating for its position
    // within the table). Without reading the implicit addend from section data,
    // all entries would compute the same wrong offset.
    let mut obj = WriteObject::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let rdata = obj.add_section(Vec::new(), b".rdata".to_vec(), object::SectionKind::ReadOnlyData);

    // Target basic block in .text — just `ret`
    let bb_off = obj.append_section_data(text, &[0xC3], 16);
    let bb_sym = obj.add_symbol(Symbol {
        name: b"bb0".to_vec(), value: bb_off, size: 1,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    // Jump table: 3 entries, each 4 bytes. All point to the same target (bb0),
    // but with different addends to account for position within the table.
    // REL32 formula: val = S + A - P, where P = entry address.
    // We want val = bb0 - jt_base for every entry.
    // Entry[i] at P = jt_base + i*4: A must be i*4 so the i*4 cancels P's offset.
    let jt_off = obj.append_section_data(rdata, &[0u8; 12], 4);
    for i in 0..3u64 {
        obj.add_relocation(rdata, object::write::Relocation {
            offset: jt_off + i * 4,
            symbol: bb_sym,
            addend: i as i64 * 4,
            flags: RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_REL32 },
        }).unwrap();
    }

    // Entry point: just a ret (we need an entry symbol)
    let entry_off = obj.append_section_data(text, &[0xC3], 16);
    obj.add_symbol(Symbol {
        name: b"efi_main".to_vec(), value: entry_off, size: 1,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    let pe = toyos_ld::link_pe(
        &[("test.o".into(), obj.write().unwrap())],
        "efi_main", 10,
    ).expect("linking should succeed");

    // Find the jump table and bb0 in the PE
    // bb0 is at the start of .text PE section
    let secs = pe_section_list(&pe);
    let text_rva = secs[0].1;
    assert_eq!(pe_read_at_rva(&pe, text_rva, 1), &[0xC3], "bb0 should be at start of .text");
    let bb0_rva = text_rva;

    // The .rdata section goes to .data PE section (or .text if is_rx_section matches .rdata)
    // Find the jump table: look for 3 consecutive i32 values
    // The jump table RVA is somewhere after the .text data
    // All 3 entries should contain the same value: bb0_rva - jt_base_rva
    let mut jt_rva = 0u32;
    for (_, va, vs, rp, _) in &secs {
        // Search this section for our jump table
        let sec_data = &pe[*rp as usize..(*rp + *vs) as usize];
        // Jump table entries should all be the same (target - base)
        for off in (0..=sec_data.len().saturating_sub(12)).step_by(4) {
            let e0 = i32::from_le_bytes(sec_data[off..off+4].try_into().unwrap());
            let e1 = i32::from_le_bytes(sec_data[off+4..off+8].try_into().unwrap());
            let e2 = i32::from_le_bytes(sec_data[off+8..off+12].try_into().unwrap());
            if e0 != 0 && e0 == e1 && e1 == e2 {
                jt_rva = va + off as u32;
                break;
            }
        }
        if jt_rva != 0 { break; }
    }

    assert!(jt_rva != 0, "should find 3 identical jump table entries in the PE");

    let e0 = pe_read_i32_at_rva(&pe, jt_rva);
    let expected = bb0_rva as i32 - jt_rva as i32;
    assert_eq!(e0, expected,
        "jump table entry should be target - base: got {e0}, expected {expected} \
         (bb0_rva={bb0_rva:#x}, jt_rva={jt_rva:#x})");
}

#[test]
fn coff_rdata_in_text_pe() {
    // .rdata sections (COFF naming for read-only data) should be placed in the
    // .text PE section (alongside code), not in .data (writable).
    let mut obj = WriteObject::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let rdata = obj.add_section(Vec::new(), b".rdata".to_vec(), object::SectionKind::ReadOnlyData);

    obj.append_section_data(rdata, &[0xAA; 16], 8);
    let code_off = obj.append_section_data(text, &[0xC3], 16);
    obj.add_symbol(Symbol {
        name: b"efi_main".to_vec(), value: code_off, size: 1,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    let pe = toyos_ld::link_pe(
        &[("test.o".into(), obj.write().unwrap())],
        "efi_main", 10,
    ).expect("linking should succeed");

    let secs = pe_section_list(&pe);
    let (_, text_va, text_vs, text_rp, _) = &secs[0];
    let text_data = &pe[*text_rp as usize..(*text_rp + *text_vs) as usize];
    let found = text_data.windows(16).any(|w| w.iter().all(|&b| b == 0xAA));
    assert!(found,
        ".rdata data should be in .text PE section, not .data. \
         .text VA={text_va:#x} VS={text_vs:#x}");
}

#[test]
fn coff_multiple_same_named_sections() {
    // COFF COMDAT sections often share names (e.g. many `.rdata` sections in
    // one object). Relocations targeting section symbols must resolve to the
    // correct specific section, not whichever one was registered last.
    //
    // We create two `.rdata` sections with distinct data and a `.text` section
    // with a LEA-style PC-relative relocation into each. The linker must
    // produce correct displacements for both — not resolve both to the same
    // section.
    let coff_bytes = {
        let mut obj = WriteObject::new(
            BinaryFormat::Coff, Architecture::X86_64, Endianness::Little,
        );
        let text = obj.section_id(StandardSection::Text);
        let rdata_a = obj.add_section(
            Vec::new(), b".rdata".to_vec(), object::SectionKind::ReadOnlyData,
        );
        let rdata_b = obj.add_section(
            Vec::new(), b".rdata".to_vec(), object::SectionKind::ReadOnlyData,
        );

        // Put distinct marker data in each .rdata section
        obj.append_section_data(rdata_a, &[0xAA; 8], 8);
        obj.append_section_data(rdata_b, &[0xBB; 8], 8);

        // Create section symbols for each .rdata section
        let sym_a = obj.add_symbol(Symbol {
            name: Vec::new(), value: 0, size: 0,
            kind: SymbolKind::Section, scope: SymbolScope::Compilation,
            weak: false, section: SymbolSection::Section(rdata_a),
            flags: SymbolFlags::None,
        });
        let sym_b = obj.add_symbol(Symbol {
            name: Vec::new(), value: 0, size: 0,
            kind: SymbolKind::Section, scope: SymbolScope::Compilation,
            weak: false, section: SymbolSection::Section(rdata_b),
            flags: SymbolFlags::None,
        });

        // .text: two LEA-like instructions, each with a 4-byte displacement
        // placeholder targeting a different .rdata section symbol.
        // LEA rax, [rip+disp] = 48 8d 05 XX XX XX XX
        let mut code = Vec::new();
        // LEA #1 → rdata_a
        code.extend_from_slice(&[0x48, 0x8d, 0x05]);
        let reloc_off_a = code.len() as u64;
        code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // placeholder
        // LEA #2 → rdata_b
        code.extend_from_slice(&[0x48, 0x8d, 0x05]);
        let reloc_off_b = code.len() as u64;
        code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // placeholder
        // ret
        code.push(0xC3);

        obj.append_section_data(text, &code, 16);

        // Entry point symbol at the start of .text
        obj.add_symbol(Symbol {
            name: b"efi_main".to_vec(), value: 0, size: 0,
            kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
            weak: false, section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });

        // REL32 relocations: displacement field → section symbol
        obj.add_relocation(text, object::write::Relocation {
            offset: reloc_off_a,
            symbol: sym_a,
            addend: -4, // standard REL32: S + A - P, A = -4 compensates for instr size
            flags: RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_REL32 },
        }).unwrap();
        obj.add_relocation(text, object::write::Relocation {
            offset: reloc_off_b,
            symbol: sym_b,
            addend: -4,
            flags: RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_REL32 },
        }).unwrap();

        obj.write().unwrap()
    };

    // Verify the object actually has two .rdata sections with section symbols
    let obj = read::File::parse(coff_bytes.as_slice()).unwrap();
    let rdata_count = obj.sections()
        .filter(|s| s.name().unwrap_or("") == ".rdata")
        .count();
    assert!(rdata_count >= 2, "test object must have multiple .rdata sections, got {rdata_count}");

    let pe = toyos_ld::link_pe(
        &[("test.o".into(), coff_bytes)],
        "efi_main", 10,
    ).expect("linking should succeed");

    // Find marker bytes in the PE output
    let pe_text = pe_section_list(&pe);
    let (_, text_va, text_vs, text_rp, _) = &pe_text[0];
    let text_data = &pe[*text_rp as usize..(*text_rp + *text_vs) as usize];

    // Find the two LEA instructions in .text
    let lea_pos_a = text_data.windows(3)
        .position(|w| w == [0x48, 0x8d, 0x05])
        .expect("first LEA not found");
    let disp_a = i32::from_le_bytes(
        text_data[lea_pos_a + 3..lea_pos_a + 7].try_into().unwrap()
    );
    let lea_pos_b = lea_pos_a + 7 + text_data[lea_pos_a + 7..].windows(3)
        .position(|w| w == [0x48, 0x8d, 0x05])
        .expect("second LEA not found");
    let disp_b = i32::from_le_bytes(
        text_data[lea_pos_b + 3..lea_pos_b + 7].try_into().unwrap()
    );

    // Each LEA should point to a different location (different .rdata sections)
    let target_rva_a = (*text_va + lea_pos_a as u32 + 7) as i32 + disp_a;
    let target_rva_b = (*text_va + lea_pos_b as u32 + 7) as i32 + disp_b;
    assert_ne!(target_rva_a, target_rva_b,
        "LEAs must target different RVAs (different .rdata sections), \
         but both point to {target_rva_a:#x}");

    // Verify each target points to the correct marker data
    let marker_a = pe_read_at_rva(&pe, target_rva_a as u32, 8);
    let marker_b = pe_read_at_rva(&pe, target_rva_b as u32, 8);
    assert_eq!(marker_a, &[0xAA; 8],
        "first LEA should target 0xAA data, got {marker_a:02x?}");
    assert_eq!(marker_b, &[0xBB; 8],
        "second LEA should target 0xBB data, got {marker_b:02x?}");
}

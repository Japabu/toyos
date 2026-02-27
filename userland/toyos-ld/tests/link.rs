use object::elf;
use object::read::elf::{ElfFile64, FileHeader as _};
use object::read::{Object, ObjectSection, ObjectSymbol};
use object::write::{Object as WriteObject, StandardSection, Symbol, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, RelocationFlags, SymbolFlags, SymbolKind, SymbolScope,
};
use std::path::PathBuf;

// ── Helpers ──────────────────────────────────────────────────────────────

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

fn build_minimal_obj(symbol_name: &str, code: &[u8]) -> Vec<u8> {
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let offset = obj.append_section_data(text, code, 16);
    obj.add_symbol(Symbol {
        name: symbol_name.as_bytes().to_vec(),
        value: offset,
        size: code.len() as u64,
        kind: SymbolKind::Text,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Section(text),
        flags: SymbolFlags::None,
    });
    obj.write().unwrap()
}

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

fn has_rustc() -> bool {
    rustc().exists() && sysroot_libdir().exists()
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
    assert!(entry >= 0x200000, "entry should be in virtual address space");
    assert!(has_phdr(&elf, elf::PT_LOAD), "should have PT_LOAD");
}

#[test]
fn cross_object_symbol_resolution() {
    // Object A: defines `helper`
    let mut obj_a = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text_a = obj_a.section_id(StandardSection::Text);
    let off_a = obj_a.append_section_data(text_a, &[0xC3], 16);
    obj_a.add_symbol(Symbol {
        name: b"helper".to_vec(),
        value: off_a,
        size: 1,
        kind: SymbolKind::Text,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Section(text_a),
        flags: SymbolFlags::None,
    });

    // Object B: defines `_start`, calls `helper` via PLT32
    let mut obj_b = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text_b = obj_b.section_id(StandardSection::Text);
    let off_b = obj_b.append_section_data(text_b, &[0xE8, 0, 0, 0, 0, 0xC3], 16);
    let helper_sym = obj_b.add_symbol(Symbol {
        name: b"helper".to_vec(),
        value: 0,
        size: 0,
        kind: SymbolKind::Text,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    });
    obj_b.add_symbol(Symbol {
        name: b"_start".to_vec(),
        value: off_b,
        size: 6,
        kind: SymbolKind::Text,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Section(text_b),
        flags: SymbolFlags::None,
    });
    obj_b
        .add_relocation(
            text_b,
            object::write::Relocation {
                offset: off_b + 1,
                symbol: helper_sym,
                addend: -4,
                flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 },
            },
        )
        .unwrap();

    let objects = vec![
        ("a.o".into(), obj_a.write().unwrap()),
        ("b.o".into(), obj_b.write().unwrap()),
    ];

    let elf_bytes =
        toyos_ld::link(&objects, "_start").expect("cross-object linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let entry = elf.elf_header().e_entry.get(elf.endian());
    assert!(entry >= 0x200000);
}

#[test]
fn undefined_symbol_error() {
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let off = obj.append_section_data(text, &[0xE8, 0, 0, 0, 0], 16);
    let undef_sym = obj.add_symbol(Symbol {
        name: b"nonexistent".to_vec(),
        value: 0,
        size: 0,
        kind: SymbolKind::Text,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    });
    obj.add_symbol(Symbol {
        name: b"_start".to_vec(),
        value: off,
        size: 5,
        kind: SymbolKind::Text,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Section(text),
        flags: SymbolFlags::None,
    });
    obj.add_relocation(
        text,
        object::write::Relocation {
            offset: off + 1,
            symbol: undef_sym,
            addend: -4,
            flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 },
        },
    )
    .unwrap();

    let result = toyos_ld::link(&[("test.o".into(), obj.write().unwrap())], "_start");
    let syms = result.expect_err("should fail with undefined symbol");
    assert!(syms.contains(&"nonexistent".to_string()));
}

#[test]
fn absolute_relocation_produces_relative() {
    // R_X86_64_64 relocation should produce R_X86_64_RELATIVE in output
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let start_off = obj.append_section_data(text, &[0xC3], 16);
    let start_sym = obj.add_symbol(Symbol {
        name: b"_start".to_vec(),
        value: start_off,
        size: 1,
        kind: SymbolKind::Text,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Section(text),
        flags: SymbolFlags::None,
    });

    let data_sec = obj.section_id(StandardSection::Data);
    let ptr_off = obj.append_section_data(data_sec, &[0u8; 8], 8);
    obj.add_relocation(
        data_sec,
        object::write::Relocation {
            offset: ptr_off,
            symbol: start_sym,
            addend: 0,
            flags: RelocationFlags::Elf { r_type: elf::R_X86_64_64 },
        },
    )
    .unwrap();

    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj.write().unwrap())], "_start")
        .expect("R_X86_64_64 linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let rela_sec = elf
        .sections()
        .find(|s| s.name().unwrap_or("") == ".rela.dyn");
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
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);

    // Two functions
    let off_a = obj.append_section_data(text, &[0xC3], 16);
    obj.add_symbol(Symbol {
        name: b"func_alpha".to_vec(),
        value: off_a, size: 1,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });
    let off_b = obj.append_section_data(text, &[0xC3], 16);
    obj.add_symbol(Symbol {
        name: b"func_beta".to_vec(),
        value: off_b, size: 1,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    let elf_bytes = toyos_ld::link_shared(&[("test.o".into(), obj.write().unwrap())])
        .expect("shared linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let names = dynsym_names(&elf);
    assert!(names.contains(&"func_alpha".to_string()), "dynsym should contain func_alpha");
    assert!(names.contains(&"func_beta".to_string()), "dynsym should contain func_beta");
}

#[test]
fn shared_cross_object_resolution() {
    // Object A: defines `helper`
    let obj_a_data = build_minimal_obj("helper", &[0xC3]);

    // Object B: defines `entry`, calls `helper` via PLT32
    let mut obj_b = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text_b = obj_b.section_id(StandardSection::Text);
    let off_b = obj_b.append_section_data(text_b, &[0xE8, 0, 0, 0, 0, 0xC3], 16);
    let helper_sym = obj_b.add_symbol(Symbol {
        name: b"helper".to_vec(),
        value: 0, size: 0,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Undefined, flags: SymbolFlags::None,
    });
    obj_b.add_symbol(Symbol {
        name: b"entry".to_vec(),
        value: off_b, size: 6,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text_b), flags: SymbolFlags::None,
    });
    obj_b.add_relocation(text_b, object::write::Relocation {
        offset: off_b + 1, symbol: helper_sym, addend: -4,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 },
    }).unwrap();

    let objects = vec![
        ("a.o".into(), obj_a_data),
        ("b.o".into(), obj_b.write().unwrap()),
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
    let endian = elf.endian();

    // Parse PT_DYNAMIC to find DT_SYMTAB and DT_STRTAB entries
    let phdrs = elf.elf_header().program_headers(endian, elf.data()).unwrap();
    let dyn_phdr = phdrs.iter().find(|ph| ph.p_type.get(endian) == elf::PT_DYNAMIC);
    assert!(dyn_phdr.is_some(), "should have PT_DYNAMIC");

    let dyn_phdr = dyn_phdr.unwrap();
    let dyn_off = dyn_phdr.p_offset.get(endian) as usize;
    let dyn_size = dyn_phdr.p_filesz.get(endian) as usize;
    let dyn_data = &elf.data()[dyn_off..dyn_off + dyn_size];

    let mut has_symtab = false;
    let mut has_strtab = false;
    let mut has_null = false;
    let mut offset = 0;
    while offset + 16 <= dyn_data.len() {
        let d_tag = i64::from_le_bytes(dyn_data[offset..offset + 8].try_into().unwrap());
        match d_tag {
            tag if tag == elf::DT_SYMTAB as i64 => has_symtab = true,
            tag if tag == elf::DT_STRTAB as i64 => has_strtab = true,
            tag if tag == elf::DT_NULL as i64 => { has_null = true; break; }
            _ => {}
        }
        offset += 16;
    }

    assert!(has_symtab, ".dynamic should have DT_SYMTAB");
    assert!(has_strtab, ".dynamic should have DT_STRTAB");
    assert!(has_null, ".dynamic should terminate with DT_NULL");
}

#[test]
fn shared_relative_relocations() {
    // R_X86_64_64 in shared lib should produce R_X86_64_RELATIVE
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let func_off = obj.append_section_data(text, &[0xC3], 16);
    let func_sym = obj.add_symbol(Symbol {
        name: b"my_func".to_vec(),
        value: func_off, size: 1,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    let data_sec = obj.section_id(StandardSection::Data);
    let ptr_off = obj.append_section_data(data_sec, &[0u8; 8], 8);
    obj.add_relocation(data_sec, object::write::Relocation {
        offset: ptr_off, symbol: func_sym, addend: 0,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_64 },
    }).unwrap();

    let elf_bytes = toyos_ld::link_shared(&[("test.o".into(), obj.write().unwrap())])
        .expect("shared lib with R_X86_64_64 should link");

    let elf = parse_elf(&elf_bytes);
    let rela = find_section(&elf, ".rela.dyn");
    assert!(rela.is_some(), "shared lib should have .rela.dyn");
    assert!(!rela.unwrap().data().unwrap().is_empty(), ".rela.dyn should have entries");
}

#[test]
fn shared_allows_undefined_symbols() {
    // Shared libraries allow undefined symbols (resolved at load time)
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let off = obj.append_section_data(text, &[0xE8, 0, 0, 0, 0], 16);
    let undef_sym = obj.add_symbol(Symbol {
        name: b"main".to_vec(),
        value: 0, size: 0,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Undefined, flags: SymbolFlags::None,
    });
    obj.add_symbol(Symbol {
        name: b"my_func".to_vec(),
        value: off, size: 5,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });
    obj.add_relocation(text, object::write::Relocation {
        offset: off + 1, symbol: undef_sym, addend: -4,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 },
    }).unwrap();

    let result = toyos_ld::link_shared(&[("test.o".into(), obj.write().unwrap())]);
    assert!(result.is_ok(), "shared lib should allow undefined symbols");
}

// ── Tests: linking against .so (dynamic library inputs) ──────────────────

#[test]
fn link_against_so_resolves_symbols() {
    // Step 1: Build a .so that exports `helper`
    let helper_obj = build_minimal_obj("helper", &[0xB8, 0x2A, 0x00, 0x00, 0x00, 0xC3]); // mov eax, 42; ret
    let so_bytes = toyos_ld::link_shared(&[("helper.o".into(), helper_obj)])
        .expect("shared lib should link");

    // Step 2: Build an object that references `helper` (undefined)
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let off = obj.append_section_data(text, &[0xE8, 0, 0, 0, 0, 0xC3], 16);
    let helper_sym = obj.add_symbol(Symbol {
        name: b"helper".to_vec(),
        value: 0, size: 0,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Undefined, flags: SymbolFlags::None,
    });
    obj.add_symbol(Symbol {
        name: b"_start".to_vec(),
        value: off, size: 6,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });
    obj.add_relocation(text, object::write::Relocation {
        offset: off + 1, symbol: helper_sym, addend: -4,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 },
    }).unwrap();

    // Step 3: Link the executable against the .so
    let objects = vec![
        ("main.o".into(), obj.write().unwrap()),
        ("libhelper.so".into(), so_bytes),
    ];
    let result = toyos_ld::link(&objects, "_start");
    assert!(result.is_ok(), "linking against .so should resolve `helper`: {:?}", result.err());
}

#[test]
fn link_against_so_does_not_include_so_content() {
    // A .so providing `helper` should not be included in the output binary
    let helper_obj = build_minimal_obj("helper", &[0xB8, 0x2A, 0x00, 0x00, 0x00, 0xC3]);
    let so_bytes = toyos_ld::link_shared(&[("helper.o".into(), helper_obj)])
        .expect("shared lib should link");

    // Build an executable that references `helper`
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let off = obj.append_section_data(text, &[0xC3], 16); // just ret
    obj.add_symbol(Symbol {
        name: b"_start".to_vec(),
        value: off, size: 1,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    // Link without .so — should succeed (no references to helper in relocs)
    let without_so = toyos_ld::link(&[("main.o".into(), obj.write().unwrap())], "_start")
        .expect("should link without .so");

    // Link with .so — output should be same size (so content not included)
    let with_so = toyos_ld::link(
        &[("main.o".into(), obj.write().unwrap()), ("libhelper.so".into(), so_bytes)],
        "_start",
    ).expect("should link with .so");

    // With .so, output is larger due to dynamic sections (DT_NEEDED, .dynsym, .dynstr, .dynamic)
    // but should NOT contain the .so's actual code/data content
    assert!(with_so.len() > without_so.len(),
        "dynamic executable should be larger due to dynamic sections");
    // The .so's code (0xB8 0x2A ...) should not appear in the output
    let helper_code = [0xB8, 0x2A, 0x00, 0x00, 0x00, 0xC3];
    let found = with_so.windows(helper_code.len()).any(|w| w == helper_code);
    assert!(!found, "output should not contain the .so's code");
}

#[test]
fn link_against_so_still_reports_truly_undefined() {
    // A .so provides `helper` but not `missing_func`
    let helper_obj = build_minimal_obj("helper", &[0xC3]);
    let so_bytes = toyos_ld::link_shared(&[("helper.o".into(), helper_obj)])
        .expect("shared lib should link");

    // Build object referencing `missing_func` (not in the .so)
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let off = obj.append_section_data(text, &[0xE8, 0, 0, 0, 0, 0xC3], 16);
    let undef_sym = obj.add_symbol(Symbol {
        name: b"missing_func".to_vec(),
        value: 0, size: 0,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Undefined, flags: SymbolFlags::None,
    });
    obj.add_symbol(Symbol {
        name: b"_start".to_vec(),
        value: off, size: 6,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });
    obj.add_relocation(text, object::write::Relocation {
        offset: off + 1, symbol: undef_sym, addend: -4,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 },
    }).unwrap();

    let result = toyos_ld::link(
        &[("main.o".into(), obj.write().unwrap()), ("libhelper.so".into(), so_bytes)],
        "_start",
    );
    let err = result.expect_err("should fail for truly undefined symbol");
    assert!(err.contains(&"missing_func".to_string()));
}

#[test]
fn shared_preserves_rustc_metadata_section() {
    // Build an object with a .rustc section
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let off = obj.append_section_data(text, &[0xC3], 16);
    obj.add_symbol(Symbol {
        name: b"my_func".to_vec(),
        value: off, size: 1,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    // Add a .rustc section with test metadata
    let rustc_section = obj.add_section(vec![], b".rustc".to_vec(), object::SectionKind::ReadOnlyData);
    let metadata = b"RUSTC_METADATA_TEST_1234567890";
    obj.append_section_data(rustc_section, metadata, 1);

    let elf_bytes = toyos_ld::link_shared(&[("test.o".into(), obj.write().unwrap())])
        .expect("shared lib with .rustc should link");

    let elf = parse_elf(&elf_bytes);
    let rustc_sec = find_section(&elf, ".rustc");
    assert!(rustc_sec.is_some(), "output should have .rustc section");
    let data = rustc_sec.unwrap().data().unwrap();
    assert_eq!(data, metadata, ".rustc section data should be preserved");
}

#[test]
fn dynamic_executable_has_dt_needed_and_glob_dat() {
    // Build a .so exporting `helper` and `_start`
    let mut so_obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = so_obj.section_id(StandardSection::Text);
    let off = so_obj.append_section_data(text, &[0xB8, 0x2A, 0x00, 0x00, 0x00, 0xC3], 16);
    so_obj.add_symbol(Symbol {
        name: b"helper".to_vec(), value: off, size: 6,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });
    let off2 = so_obj.append_section_data(text, &[0xC3], 16);
    so_obj.add_symbol(Symbol {
        name: b"_start".to_vec(), value: off2, size: 1,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });
    let so_bytes = toyos_ld::link_shared(&[("helper.o".into(), so_obj.write().unwrap())])
        .expect("shared lib should link");

    // Build an object that calls `helper` (via PLT32 reloc) but has no _start
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let off = obj.append_section_data(text, &[0xE8, 0x00, 0x00, 0x00, 0x00, 0xC3], 16);
    obj.add_symbol(Symbol {
        name: b"main".to_vec(), value: off, size: 6,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });
    let helper_sym = obj.add_symbol(Symbol {
        name: b"helper".to_vec(), value: 0, size: 0,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Undefined, flags: SymbolFlags::None,
    });
    obj.add_relocation(text, object::write::Relocation {
        offset: off + 1, symbol: helper_sym, addend: -4,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 },
    }).unwrap();

    // Link executable against the .so — _start comes from .so via PLT
    let result = toyos_ld::link(
        &[("main.o".into(), obj.write().unwrap()), ("libmylib.so".into(), so_bytes)],
        "_start",
    );
    assert!(result.is_ok(), "should link dynamic executable: {:?}", result.err());
    let elf_bytes = result.unwrap();
    let elf = parse_elf(&elf_bytes);

    // Verify PT_DYNAMIC exists
    assert!(has_phdr(&elf, elf::PT_DYNAMIC), "should have PT_DYNAMIC");

    // Verify .dynamic section exists with DT_NEEDED
    let dynamic_sec = find_section(&elf, ".dynamic");
    assert!(dynamic_sec.is_some(), "should have .dynamic section");
    let dyn_data = dynamic_sec.unwrap().data().unwrap();
    // Parse DT_NEEDED entries (tag=1, each entry is 16 bytes)
    let mut needed_offsets = Vec::new();
    for chunk in dyn_data.chunks_exact(16) {
        let tag = i64::from_le_bytes(chunk[0..8].try_into().unwrap());
        let val = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
        if tag == elf::DT_NEEDED as i64 {
            needed_offsets.push(val);
        }
    }
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
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let start_off = obj.append_section_data(text, &[0xC3], 16);
    let start_sym = obj.add_symbol(Symbol {
        name: b"_start".to_vec(),
        value: start_off, size: 1,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    let data_sec = obj.section_id(StandardSection::Data);
    let ptr_off = obj.append_section_data(data_sec, &[0u8; 8], 8);
    obj.add_relocation(data_sec, object::write::Relocation {
        offset: ptr_off, symbol: start_sym, addend: 0,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_64 },
    }).unwrap();

    let elf_bytes = toyos_ld::link_static(
        &[("test.o".into(), obj.write().unwrap())],
        "_start",
        0x200000,
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
    // Object A: defines `helper`
    let obj_a_data = build_minimal_obj("helper", &[0xC3]);

    // Object B: defines `_start`, calls `helper` via PLT32
    let mut obj_b = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text_b = obj_b.section_id(StandardSection::Text);
    let off_b = obj_b.append_section_data(text_b, &[0xE8, 0, 0, 0, 0, 0xC3], 16);
    let helper_sym = obj_b.add_symbol(Symbol {
        name: b"helper".to_vec(),
        value: 0, size: 0,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Undefined, flags: SymbolFlags::None,
    });
    obj_b.add_symbol(Symbol {
        name: b"_start".to_vec(),
        value: off_b, size: 6,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text_b), flags: SymbolFlags::None,
    });
    obj_b.add_relocation(text_b, object::write::Relocation {
        offset: off_b + 1, symbol: helper_sym, addend: -4,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 },
    }).unwrap();

    let objects = vec![
        ("a.o".into(), obj_a_data),
        ("b.o".into(), obj_b.write().unwrap()),
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
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let off = obj.append_section_data(text, &[0xE8, 0, 0, 0, 0], 16);
    let undef_sym = obj.add_symbol(Symbol {
        name: b"nonexistent".to_vec(),
        value: 0, size: 0,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Undefined, flags: SymbolFlags::None,
    });
    obj.add_symbol(Symbol {
        name: b"_start".to_vec(),
        value: off, size: 5,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });
    obj.add_relocation(text, object::write::Relocation {
        offset: off + 1, symbol: undef_sym, addend: -4,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 },
    }).unwrap();

    let result = toyos_ld::link_static(
        &[("test.o".into(), obj.write().unwrap())],
        "_start",
        0x200000,
    );
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

fn parse_pe_u16(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(data[off..off + 2].try_into().unwrap())
}

fn parse_pe_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
}

fn pe_section_name(data: &[u8], sh_off: usize) -> String {
    let raw = &data[sh_off..sh_off + 8];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(8);
    String::from_utf8_lossy(&raw[..end]).to_string()
}

fn pe_section_characteristics(data: &[u8], sh_off: usize) -> u32 {
    parse_pe_u32(data, sh_off + 36)
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
    assert_eq!(parse_pe_u16(&pe, 0), 0x5A4D, "should start with MZ magic");
    let pe_offset = parse_pe_u32(&pe, 0x3C) as usize;
    assert_eq!(pe_offset, 0x40, "e_lfanew should point to PE signature");

    // PE signature
    assert_eq!(parse_pe_u32(&pe, pe_offset), 0x00004550, "should have PE\\0\\0 signature");

    // COFF header
    let coff = pe_offset + 4;
    assert_eq!(parse_pe_u16(&pe, coff), 0x8664, "Machine should be AMD64");

    // Optional header
    let oh = coff + 20;
    assert_eq!(parse_pe_u16(&pe, oh), 0x020B, "should be PE32+ (0x020B)");
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
    let num_sections = parse_pe_u16(&pe, coff + 2) as usize;
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
    // Create two functions to verify entry points to the right one
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let off_dummy = obj.append_section_data(text, &[0x90, 0xC3], 16); // nop; ret
    obj.add_symbol(Symbol {
        name: b"dummy".to_vec(),
        value: off_dummy, size: 2,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });
    let off_entry = obj.append_section_data(text, &[0x31, 0xC0, 0xC3], 16); // xor eax,eax; ret
    obj.add_symbol(Symbol {
        name: b"efi_main".to_vec(),
        value: off_entry, size: 3,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    let pe = toyos_ld::link_pe(
        &[("test.o".into(), obj.write().unwrap())],
        "efi_main",
        10,
    ).expect("PE linking should succeed");

    let oh = 0x58;
    let entry_rva = parse_pe_u32(&pe, oh + 0x10);
    assert!(entry_rva > 0, "AddressOfEntryPoint should be non-zero");

    // Entry should NOT be at the very start of .text (dummy is there)
    let text_rva = parse_pe_u32(&pe, oh + 0x14);
    assert!(entry_rva > text_rva, "entry should not be at start of .text (dummy func is there)");
}

#[test]
fn pe_subsystem() {
    let obj_data = build_minimal_obj("efi_main", &[0xC3]);

    // Test EFI_APPLICATION (10)
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj_data.clone())], "efi_main", 10).unwrap();
    let oh = 0x58;
    assert_eq!(parse_pe_u16(&pe, oh + 0x44), 10, "subsystem should be EFI_APPLICATION (10)");

    // Test EFI_BOOT_SERVICE_DRIVER (11)
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj_data.clone())], "efi_main", 11).unwrap();
    assert_eq!(parse_pe_u16(&pe, oh + 0x44), 11, "subsystem should be EFI_BOOT_SERVICE_DRIVER (11)");

    // Test EFI_RUNTIME_DRIVER (12)
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj_data)], "efi_main", 12).unwrap();
    assert_eq!(parse_pe_u16(&pe, oh + 0x44), 12, "subsystem should be EFI_RUNTIME_DRIVER (12)");
}

#[test]
fn pe_base_relocations() {
    // Create an object with R_X86_64_64 (absolute 64-bit) — needs base relocation in PE
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let func_off = obj.append_section_data(text, &[0xC3], 16);
    let func_sym = obj.add_symbol(Symbol {
        name: b"efi_main".to_vec(),
        value: func_off, size: 1,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    let data_sec = obj.section_id(StandardSection::Data);
    let ptr_off = obj.append_section_data(data_sec, &[0u8; 8], 8);
    obj.add_relocation(data_sec, object::write::Relocation {
        offset: ptr_off, symbol: func_sym, addend: 0,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_64 },
    }).unwrap();

    let pe = toyos_ld::link_pe(
        &[("test.o".into(), obj.write().unwrap())],
        "efi_main",
        10,
    ).expect("PE with R_X86_64_64 should succeed");

    // Find .reloc section
    let coff = 0x44;
    let num_sections = parse_pe_u16(&pe, coff + 2) as usize;
    let sh_base = 0x58 + 240;
    let mut reloc_file_off = 0u32;
    let mut reloc_raw_size = 0u32;
    for i in 0..num_sections {
        let sh = sh_base + i * 40;
        if pe_section_name(&pe, sh) == ".reloc" {
            reloc_file_off = parse_pe_u32(&pe, sh + 20);
            reloc_raw_size = parse_pe_u32(&pe, sh + 16);
        }
    }
    assert!(reloc_file_off > 0, "should have .reloc section");
    assert!(reloc_raw_size > 0, ".reloc should have data");

    // Parse base relocation blocks
    let reloc_data = &pe[reloc_file_off as usize..(reloc_file_off + reloc_raw_size) as usize];
    assert!(reloc_data.len() >= 12, ".reloc should have at least one block with one entry");

    // First block header
    let page_rva = parse_pe_u32(reloc_data, 0);
    let block_size = parse_pe_u32(reloc_data, 4);
    assert!(block_size >= 12, "block should have header + at least one entry");
    assert!(page_rva > 0, "page_rva should be non-zero");

    // Check entries contain DIR64 type (type=10 in upper 4 bits)
    let num_entries = (block_size - 8) / 2;
    let mut has_dir64 = false;
    for e in 0..num_entries {
        let entry = parse_pe_u16(reloc_data, 8 + e as usize * 2);
        let typ = entry >> 12;
        if typ == 10 { has_dir64 = true; }
    }
    assert!(has_dir64, ".reloc should have IMAGE_REL_BASED_DIR64 entries");
}

#[test]
fn pe_no_relocs_for_pc_relative() {
    // R_X86_64_PLT32 (PC-relative) should NOT produce base relocations
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);

    // helper function
    let off_a = obj.append_section_data(text, &[0xC3], 16);
    let helper_sym = obj.add_symbol(Symbol {
        name: b"helper".to_vec(),
        value: off_a, size: 1,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    // efi_main calls helper via PLT32 (PC-relative)
    let off_b = obj.append_section_data(text, &[0xE8, 0, 0, 0, 0, 0xC3], 16);
    obj.add_symbol(Symbol {
        name: b"efi_main".to_vec(),
        value: off_b, size: 6,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });
    obj.add_relocation(text, object::write::Relocation {
        offset: off_b + 1, symbol: helper_sym, addend: -4,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 },
    }).unwrap();

    let pe = toyos_ld::link_pe(
        &[("test.o".into(), obj.write().unwrap())],
        "efi_main",
        10,
    ).expect("PE with only PC-relative relocs should succeed");

    // .reloc should exist but have no DIR64 entries (only padding or empty)
    let oh = 0x58;
    // Data directory 5 (base relocation) should have size 0
    let dd5 = oh + 0x70 + 5 * 8;
    let reloc_dir_size = parse_pe_u32(&pe, dd5 + 4);
    assert_eq!(reloc_dir_size, 0, "no base relocations needed for PC-relative only code");
}

#[test]
fn pe_undefined_symbol_error() {
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let off = obj.append_section_data(text, &[0xE8, 0, 0, 0, 0], 16);
    let undef_sym = obj.add_symbol(Symbol {
        name: b"missing".to_vec(),
        value: 0, size: 0,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Undefined, flags: SymbolFlags::None,
    });
    obj.add_symbol(Symbol {
        name: b"efi_main".to_vec(),
        value: off, size: 5,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });
    obj.add_relocation(text, object::write::Relocation {
        offset: off + 1, symbol: undef_sym, addend: -4,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 },
    }).unwrap();

    let result = toyos_ld::link_pe(
        &[("test.o".into(), obj.write().unwrap())],
        "efi_main",
        10,
    );
    let syms = result.expect_err("should fail with undefined symbol");
    assert!(syms.contains(&"missing".to_string()));
}

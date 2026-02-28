mod common;
use common::*;
use object::elf;
use object::read::ObjectSection;

#[test]
fn shared_basic_output() {
    let obj_data = build_minimal_obj("my_func", &[0xC3]);

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
    assert!(find_section(&elf, ".dynsym").is_some(), "should have .dynsym section");
    assert!(find_section(&elf, ".dynstr").is_some(), "should have .dynstr section");
    assert!(find_section(&elf, ".dynamic").is_some(), "should have .dynamic section");
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

// ── Dynamic linking (linking against .so) ────────────────────────────────

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
    match &err {
        toyos_ld::LinkError::UndefinedSymbols(syms) => assert!(syms.contains(&"missing_func".to_string())),
        other => panic!("expected UndefinedSymbols, got: {other:?}"),
    }
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

    assert!(has_phdr(&elf, elf::PT_DYNAMIC), "should have PT_DYNAMIC");

    let dyn_entries = parse_dynamic(&elf);
    let needed_offsets: Vec<u64> = dyn_entries.iter()
        .filter(|&&(tag, _)| tag == elf::DT_NEEDED as i64)
        .map(|&(_, val)| val)
        .collect();
    assert!(!needed_offsets.is_empty(), "should have at least one DT_NEEDED");

    let dynstr_sec = find_section(&elf, ".dynstr").expect(".dynstr section");
    let dynstr_data = dynstr_sec.data().unwrap();
    let name_offset = needed_offsets[0] as usize;
    let name_end = dynstr_data[name_offset..].iter().position(|&b| b == 0).unwrap();
    let lib_name = std::str::from_utf8(&dynstr_data[name_offset..name_offset + name_end]).unwrap();
    assert_eq!(lib_name, "libmylib.so", "DT_NEEDED should reference the .so filename");

    let dynsym_sec = find_section(&elf, ".dynsym").expect(".dynsym section");
    let dynsym_data = dynsym_sec.data().unwrap();
    assert!(dynsym_data.len() > 24, "should have import symbols beyond null entry");

    let rela_sec = find_section(&elf, ".rela.dyn").expect(".rela.dyn section");
    let rela_data = rela_sec.data().unwrap();
    let mut has_glob_dat = false;
    for chunk in rela_data.chunks_exact(24) {
        let r_info = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
        let r_type = (r_info & 0xFFFFFFFF) as u32;
        if r_type == 6 { has_glob_dat = true; }
    }
    assert!(has_glob_dat, "should have R_X86_64_GLOB_DAT relocations");

    let endian = elf.endian();
    let entry = elf.elf_header().e_entry.get(endian);
    assert!(entry > 0, "entry should be non-zero (PLT stub for _start)");
}

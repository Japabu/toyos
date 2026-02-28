mod common;
use common::*;
use object::elf;
use object::read::elf::FileHeader as _;
use object::read::{Object, ObjectSection};

#[test]
fn static_produces_et_exec() {
    let code = vec![0x31, 0xFF, 0xB8, 0x3C, 0x00, 0x00, 0x00, 0x0F, 0x05];
    let obj_data = build_minimal_obj("_start", &code);

    let elf_bytes = toyos_ld::link_static(
        &[("test.o".into(), obj_data)], "_start", 0x200000,
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
        &[("test.o".into(), obj_data)], "_start", 0x200000,
    ).expect("static linking should succeed");

    let elf = parse_elf(&elf_bytes);

    assert!(!has_phdr(&elf, elf::PT_DYNAMIC), "static ELF must not have PT_DYNAMIC");
    assert!(find_section(&elf, ".dynsym").is_none(), "static ELF must not have .dynsym");
    assert!(find_section(&elf, ".dynstr").is_none(), "static ELF must not have .dynstr");
    assert!(find_section(&elf, ".dynamic").is_none(), "static ELF must not have .dynamic");
}

#[test]
fn static_no_relative_relocations() {
    let obj = ObjBuilder::elf().func("_start", &[0xC3]).data_ptr_to("_start").build();
    let elf_bytes = toyos_ld::link_static(
        &[("test.o".into(), obj)], "_start", 0x200000,
    ).expect("static linking with R_X86_64_64 should succeed");

    let elf = parse_elf(&elf_bytes);

    assert!(find_section(&elf, ".rela.dyn").is_none(),
        "static ELF must not have .rela.dyn (all relocations resolved at link time)");

    let entry = elf.elf_header().e_entry.get(elf.endian());
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
    let obj_data = build_minimal_obj("_start", &[0xC3]);

    let high_base: u64 = 0xFFFF_8000_0000_0000;
    let elf_bytes = toyos_ld::link_static(
        &[("test.o".into(), obj_data)], "_start", high_base,
    ).expect("static linking with high base should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();

    assert_eq!(elf.elf_header().e_type.get(endian), 2, "should be ET_EXEC");
    let entry = elf.elf_header().e_entry.get(endian);
    assert!(entry >= high_base, "entry {entry:#x} should be above {high_base:#x}");

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
    let err = result.expect_err("should fail with undefined symbol");
    match &err {
        toyos_ld::LinkError::UndefinedSymbols(syms) => assert!(syms.contains(&"nonexistent".to_string())),
        other => panic!("expected UndefinedSymbols, got: {other:?}"),
    }
}

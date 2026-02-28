mod common;
use common::*;
use object::elf;
use object::read::elf::FileHeader as _;
use object::read::ObjectSection;

#[test]
fn basic_single_function() {
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
    let err = result.expect_err("should fail with undefined symbol");
    match &err {
        toyos_ld::LinkError::UndefinedSymbols(syms) => assert!(syms.contains(&"nonexistent".to_string())),
        other => panic!("expected UndefinedSymbols, got: {other:?}"),
    }
}

#[test]
fn absolute_relocation_produces_relative() {
    let obj = ObjBuilder::elf().func("_start", &[0xC3]).data_ptr_to("_start").build();
    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj)], "_start")
        .expect("R_X86_64_64 linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let rela_sec = find_section(&elf, ".rela.dyn");
    assert!(rela_sec.is_some(), "should have .rela.dyn section");
    let rela_data = rela_sec.unwrap().data().unwrap();
    assert!(!rela_data.is_empty(), ".rela.dyn should have entries");
}

#[test]
fn pie_base_vaddr_is_zero() {
    let obj_data = build_minimal_obj("_start", &[0xC3]);

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
    let obj_data = build_minimal_obj("_start", &[0xC3]);

    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj_data)], "_start")
        .expect("linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let entry = elf.elf_header().e_entry.get(elf.endian());
    assert!(entry < 0x10000,
        "entry should be zero-based for PIE, got {entry:#x}");
}

#[test]
fn pie_file_offset_equals_vaddr() {
    let obj_data = build_minimal_obj("_start", &[0xC3]);

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

    assert!(rela_data.len() >= 24, "should have at least one RELA entry");
    let addend = i64::from_le_bytes(rela_data[16..24].try_into().unwrap());
    assert!(addend < 0x10000,
        "RELATIVE addend should be zero-based, got {addend:#x}");
}

#[test]
fn pie_bootloader_loadable() {
    let code: Vec<u8> = vec![
        0x48, 0x31, 0xFF, 0xB8, 0x3C, 0x00, 0x00, 0x00, 0x0F, 0x05,
    ];
    let obj_data = build_minimal_obj("_start", &code);

    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj_data)], "_start")
        .expect("linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();
    let phdrs = elf.elf_header().program_headers(endian, elf.data()).unwrap();

    let mut mem_size: usize = 0;
    for ph in phdrs.iter().filter(|ph| ph.p_type.get(endian) == elf::PT_LOAD) {
        let end = ph.p_vaddr.get(endian) + ph.p_memsz.get(endian);
        mem_size = mem_size.max(end as usize);
    }

    assert!(mem_size < 0x100000,
        "total memory for simple PIE should be < 1MB, got {mem_size:#x}");

    let mut process_mem = vec![0u8; mem_size];
    for ph in phdrs.iter().filter(|ph| ph.p_type.get(endian) == elf::PT_LOAD) {
        let fstart = ph.p_offset.get(endian) as usize;
        let fend = fstart + ph.p_filesz.get(endian) as usize;
        let vstart = ph.p_vaddr.get(endian) as usize;
        let vend = vstart + ph.p_filesz.get(endian) as usize;
        process_mem[vstart..vend].copy_from_slice(&elf_bytes[fstart..fend]);
    }

    let entry = elf.elf_header().e_entry.get(endian) as usize;
    assert!(entry + code.len() <= process_mem.len(),
        "entry ({entry:#x}) + code should fit in process memory");
    assert_eq!(&process_mem[entry..entry + code.len()], &code,
        "code at entry point should match original");
}

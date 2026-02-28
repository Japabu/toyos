mod common;
use common::*;
use object::elf;
use object::read::elf::FileHeader as _;
use object::read::ObjectSection;

// ── Error reporting ─────────────────────────────────────────────────────

#[test]
fn parse_error_on_invalid_input() {
    let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let result = toyos_ld::link(&[("garbage.o".into(), garbage)], "_start");
    let err = result.unwrap_err();
    match &err {
        toyos_ld::LinkError::Parse { file, .. } => {
            assert_eq!(file, "garbage.o");
        }
        other => panic!("expected Parse error, got: {other:?}"),
    }
}

#[test]
fn missing_entry_symbol_error() {
    let obj_data = build_minimal_obj("some_func", &[0xC3]);
    let result = toyos_ld::link(&[("test.o".into(), obj_data)], "nonexistent_entry");
    let err = result.unwrap_err();
    match &err {
        toyos_ld::LinkError::MissingEntry(name) => {
            assert_eq!(name, "nonexistent_entry");
        }
        other => panic!("expected MissingEntry error, got: {other:?}"),
    }
}

#[test]
fn missing_entry_symbol_error_pe() {
    let obj_data = build_minimal_obj("some_func", &[0xC3]);
    let result = toyos_ld::link_pe(&[("test.o".into(), obj_data)], "nonexistent_entry", 10);
    let err = result.unwrap_err();
    match &err {
        toyos_ld::LinkError::MissingEntry(name) => {
            assert_eq!(name, "nonexistent_entry");
        }
        other => panic!("expected MissingEntry error, got: {other:?}"),
    }
}

#[test]
fn missing_entry_symbol_error_static() {
    let obj_data = build_minimal_obj("some_func", &[0xC3]);
    let result = toyos_ld::link_static(&[("test.o".into(), obj_data)], "nonexistent_entry", 0x200000);
    let err = result.unwrap_err();
    match &err {
        toyos_ld::LinkError::MissingEntry(name) => {
            assert_eq!(name, "nonexistent_entry");
        }
        other => panic!("expected MissingEntry error, got: {other:?}"),
    }
}

// ── BSS / NOBITS ────────────────────────────────────────────────────────

#[test]
fn bss_no_file_space() {
    let obj = ObjBuilder::elf()
        .func("_start", &[0xC3])
        .bss("big_buffer", 0x10000)
        .build();
    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj)], "_start")
        .expect("linking with .bss should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();
    let phdrs = elf.elf_header().program_headers(endian, elf.data()).unwrap();
    let rw_phdr = phdrs.iter()
        .find(|ph| ph.p_type.get(endian) == elf::PT_LOAD && ph.p_flags.get(endian) & elf::PF_W != 0)
        .expect("should have RW PT_LOAD");
    let filesz = rw_phdr.p_filesz.get(endian);
    let memsz = rw_phdr.p_memsz.get(endian);
    assert!(memsz > filesz,
        "RW segment p_memsz ({memsz:#x}) should be larger than p_filesz ({filesz:#x}) due to .bss");

    assert!(elf_bytes.len() < 0x10000,
        "output file ({:#x} bytes) should be smaller than .bss size (0x10000)", elf_bytes.len());
}

#[test]
fn bss_virtual_address_valid() {
    let mut builder = ObjBuilder::elf().func("_start", &[0xC3]).bss("my_bss", 256);
    let bss_sym = builder.inner_mut().symbol_id(b"my_bss").unwrap();
    let data_sec = builder.inner_mut().section_id(object::write::StandardSection::Data);
    let ptr_off = builder.inner_mut().append_section_data(data_sec, &[0u8; 8], 8);
    builder.inner_mut().add_relocation(data_sec, object::write::Relocation {
        offset: ptr_off, symbol: bss_sym, addend: 0,
        flags: object::RelocationFlags::Elf { r_type: elf::R_X86_64_64 },
    }).unwrap();
    let obj = builder.build();

    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj)], "_start")
        .expect("linking should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();
    assert_eq!(elf.elf_header().e_type.get(endian), elf::ET_DYN);
}

#[test]
fn bss_no_file_space_static() {
    let obj = ObjBuilder::elf()
        .func("_start", &[0xC3])
        .bss("big_buffer", 0x10000)
        .build();
    let elf_bytes = toyos_ld::link_static(&[("test.o".into(), obj)], "_start", 0x200000)
        .expect("static linking with .bss should succeed");

    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();
    let phdrs = elf.elf_header().program_headers(endian, elf.data()).unwrap();
    let rw_phdr = phdrs.iter()
        .find(|ph| ph.p_type.get(endian) == elf::PT_LOAD && ph.p_flags.get(endian) & elf::PF_W != 0)
        .expect("should have RW PT_LOAD");
    let filesz = rw_phdr.p_filesz.get(endian);
    let memsz = rw_phdr.p_memsz.get(endian);
    assert!(memsz > filesz,
        "RW segment p_memsz ({memsz:#x}) should be larger than p_filesz ({filesz:#x}) due to .bss");
}

// ── Symtab ──────────────────────────────────────────────────────────────

#[test]
fn pie_has_symtab() {
    let obj = ObjBuilder::elf().func("_start", &[0xC3]).func("helper", &[0xC3]).build();
    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj)], "_start")
        .expect("linking should succeed");
    let elf = parse_elf(&elf_bytes);
    assert!(find_section(&elf, ".symtab").is_some(), "PIE should have .symtab");
    assert!(find_section(&elf, ".strtab").is_some(), "PIE should have .strtab");
    let names = symtab_names(&elf);
    assert!(names.contains(&"_start".to_string()), "symtab should contain _start");
    assert!(names.contains(&"helper".to_string()), "symtab should contain helper");
}

#[test]
fn static_has_symtab() {
    let obj = ObjBuilder::elf().func("_start", &[0xC3]).func("kernel_main", &[0xC3]).build();
    let elf_bytes = toyos_ld::link_static(&[("test.o".into(), obj)], "_start", 0x200000)
        .expect("static linking should succeed");
    let elf = parse_elf(&elf_bytes);
    assert!(find_section(&elf, ".symtab").is_some(), "static ELF should have .symtab");
    assert!(find_section(&elf, ".strtab").is_some(), "static ELF should have .strtab");
    let names = symtab_names(&elf);
    assert!(names.contains(&"_start".to_string()), "symtab should contain _start");
    assert!(names.contains(&"kernel_main".to_string()), "symtab should contain kernel_main");
}

#[test]
fn shared_has_symtab() {
    let obj = ObjBuilder::elf().func("exported", &[0xC3]).build();
    let elf_bytes = toyos_ld::link_shared(&[("test.o".into(), obj)])
        .expect("shared linking should succeed");
    let elf = parse_elf(&elf_bytes);
    assert!(find_section(&elf, ".symtab").is_some(), "shared lib should have .symtab");
    assert!(find_section(&elf, ".strtab").is_some(), "shared lib should have .strtab");
    let names = symtab_names(&elf);
    assert!(names.contains(&"exported".to_string()), "symtab should contain exported");
}

#[test]
fn symtab_entry_has_valid_address() {
    let obj = ObjBuilder::elf().func("_start", &[0xC3]).build();
    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj)], "_start")
        .expect("linking should succeed");
    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();

    // Parse .symtab + .strtab manually to avoid ObjectSymbol ICE in this compiler
    let symtab = find_section(&elf, ".symtab").expect(".symtab");
    let strtab = find_section(&elf, ".strtab").expect(".strtab");
    let symtab_data = symtab.data().unwrap();
    let strtab_data = strtab.data().unwrap();

    let mut found = false;
    for chunk in symtab_data.chunks_exact(24) {
        let name_off = u32::from_le_bytes(chunk[0..4].try_into().unwrap()) as usize;
        let value = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
        if name_off < strtab_data.len() {
            let end = strtab_data[name_off..].iter().position(|&b| b == 0).unwrap_or(0);
            let name = std::str::from_utf8(&strtab_data[name_off..name_off + end]).unwrap_or("");
            if name == "_start" {
                let entry = elf.elf_header().e_entry.get(endian);
                assert_eq!(value, entry, "_start symbol address should match entry point");
                found = true;
                break;
            }
        }
    }
    assert!(found, "_start should be in .symtab");
}

// ── Archive selection ───────────────────────────────────────────────────

#[test]
fn archive_only_needed_members() {
    let main_obj = ObjBuilder::elf().func("_start", &[0xC3]).func_calling("caller", "helper").build();
    let needed_obj = ObjBuilder::elf().func("helper", &[0xC3]).build();
    let unneeded_obj = ObjBuilder::elf().func("unused_func", &[0xC3]).build();

    let ar = build_archive(&[("needed.o", &needed_obj), ("unneeded.o", &unneeded_obj)]);

    let dir = std::env::temp_dir().join("toyos_ld_test_archive_only_needed");
    let _ = std::fs::create_dir_all(&dir);
    let main_path = dir.join("main.o");
    let ar_path = dir.join("libtest.a");
    std::fs::write(&main_path, &main_obj).unwrap();
    std::fs::write(&ar_path, &ar).unwrap();

    let objects = toyos_ld::resolve_libs(
        &[main_path.clone(), ar_path.clone()], &[], &[],
    ).expect("resolve_libs should succeed");

    let names: Vec<&str> = objects.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.iter().any(|n| n.contains("main.o")), "should include main.o");
    assert!(names.iter().any(|n| n.contains("needed.o")), "should include needed.o");
    assert!(!names.iter().any(|n| n.contains("unneeded.o")),
        "should NOT include unneeded.o, got: {names:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn archive_transitive_pull() {
    let main_obj = ObjBuilder::elf().func("_start", &[0xC3]).func_calling("caller", "func_a").build();
    let a_obj = ObjBuilder::elf().func("func_a", &[0xC3]).func_calling("a_calls_b", "func_b").build();
    let b_obj = ObjBuilder::elf().func("func_b", &[0xC3]).build();

    let ar = build_archive(&[("a.o", &a_obj), ("b.o", &b_obj)]);

    let dir = std::env::temp_dir().join("toyos_ld_test_archive_transitive");
    let _ = std::fs::create_dir_all(&dir);
    let main_path = dir.join("main.o");
    let ar_path = dir.join("libtest.a");
    std::fs::write(&main_path, &main_obj).unwrap();
    std::fs::write(&ar_path, &ar).unwrap();

    let objects = toyos_ld::resolve_libs(
        &[main_path.clone(), ar_path.clone()], &[], &[],
    ).expect("resolve_libs should succeed");

    let names: Vec<&str> = objects.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.iter().any(|n| n.contains("a.o")), "should include a.o (defines func_a)");
    assert!(names.iter().any(|n| n.contains("b.o")), "should include b.o (transitively needed)");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn archive_empty_if_nothing_needed() {
    let main_obj = ObjBuilder::elf().func("_start", &[0xC3]).build();
    let lib_obj = ObjBuilder::elf().func("unused", &[0xC3]).build();

    let ar = build_archive(&[("lib.o", &lib_obj)]);

    let dir = std::env::temp_dir().join("toyos_ld_test_archive_empty");
    let _ = std::fs::create_dir_all(&dir);
    let main_path = dir.join("main.o");
    let ar_path = dir.join("libtest.a");
    std::fs::write(&main_path, &main_obj).unwrap();
    std::fs::write(&ar_path, &ar).unwrap();

    let objects = toyos_ld::resolve_libs(
        &[main_path.clone(), ar_path.clone()], &[], &[],
    ).expect("resolve_libs should succeed");

    assert_eq!(objects.len(), 1, "should only have main.o, got {} objects", objects.len());

    let _ = std::fs::remove_dir_all(&dir);
}

// ── Relocation overflow ─────────────────────────────────────────────────

#[test]
fn relocation_overflow_32s() {
    let obj = build_reloc_overflow_obj(elf::R_X86_64_32S);
    let result = toyos_ld::link_static(&[("test.o".into(), obj)], "_start", 0x1_0000_0000);
    match result {
        Err(toyos_ld::LinkError::RelocationOverflow { reloc_type, .. }) => {
            assert_eq!(reloc_type, elf::R_X86_64_32S);
        }
        Err(other) => panic!("expected RelocationOverflow, got: {other:?}"),
        Ok(_) => panic!("expected RelocationOverflow error, but linking succeeded"),
    }
}

#[test]
fn relocation_overflow_32() {
    let obj = build_reloc_overflow_obj(elf::R_X86_64_32);
    let result = toyos_ld::link_static(&[("test.o".into(), obj)], "_start", 0x1_0000_0000);
    match result {
        Err(toyos_ld::LinkError::RelocationOverflow { reloc_type, .. }) => {
            assert_eq!(reloc_type, elf::R_X86_64_32);
        }
        Err(other) => panic!("expected RelocationOverflow, got: {other:?}"),
        Ok(_) => panic!("expected RelocationOverflow error, but linking succeeded"),
    }
}

// ── GC sections ─────────────────────────────────────────────────────────

#[test]
fn gc_removes_dead_code() {
    let obj1 = ObjBuilder::elf().func("_start", &[0xC3]).build();
    let obj2 = ObjBuilder::elf().func("dead_func", &[0xCC; 256]).build();

    let objects: Vec<(String, Vec<u8>)> = vec![
        ("main.o".into(), obj1.clone()), ("dead.o".into(), obj2.clone()),
    ];
    let without_gc = toyos_ld::link(&objects, "_start")
        .expect("link without gc");
    let with_gc = toyos_ld::link_with(&objects, "_start", true)
        .expect("link with gc");

    assert!(with_gc.len() < without_gc.len(),
        "gc output ({}) should be smaller than non-gc ({})", with_gc.len(), without_gc.len());

    let elf = parse_elf(&with_gc);
    let names = symtab_names(&elf);
    assert!(names.contains(&"_start".to_string()), "symtab should still contain _start");
    assert!(!names.contains(&"dead_func".to_string()),
        "symtab should NOT contain dead_func after gc");
}

#[test]
fn gc_preserves_reachable_chain() {
    let obj1 = ObjBuilder::elf().func_calling("_start", "helper").build();
    let obj2 = ObjBuilder::elf().func_calling("helper", "util").build();
    let obj3 = ObjBuilder::elf().func("util", &[0xC3]).build();
    let obj4 = ObjBuilder::elf().func("dead", &[0xCC]).build();

    let objects = vec![
        ("a.o".into(), obj1), ("b.o".into(), obj2),
        ("c.o".into(), obj3), ("d.o".into(), obj4),
    ];
    let elf_bytes = toyos_ld::link_with(&objects, "_start", true)
        .expect("gc linking should succeed");
    let elf = parse_elf(&elf_bytes);
    let names = symtab_names(&elf);
    assert!(names.contains(&"_start".to_string()));
    assert!(names.contains(&"helper".to_string()));
    assert!(names.contains(&"util".to_string()));
    assert!(!names.contains(&"dead".to_string()), "dead should be gc'd");
}

#[test]
fn gc_disabled_by_default() {
    let obj1 = ObjBuilder::elf().func("_start", &[0xC3]).build();
    let obj2 = ObjBuilder::elf().func("dead_func", &[0xCC]).build();
    let objects = vec![("main.o".into(), obj1), ("dead.o".into(), obj2)];
    let elf_bytes = toyos_ld::link(&objects, "_start")
        .expect("link should succeed");
    let elf = parse_elf(&elf_bytes);
    let names = symtab_names(&elf);
    assert!(names.contains(&"dead_func".to_string()),
        "dead_func should remain without gc");
}

// ── .init_array / .fini_array ───────────────────────────────────────────

#[test]
fn init_array_in_dynamic_pie() {
    let obj = build_init_array_obj();
    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj)], "_start")
        .expect("PIE with .init_array should link");

    let elf = parse_elf(&elf_bytes);
    assert!(has_phdr(&elf, elf::PT_DYNAMIC), "PIE with .init_array should have PT_DYNAMIC");

    let dyn_entries = parse_dynamic(&elf);
    let has = |tag: u32| dyn_entries.iter().any(|&(t, _)| t == tag as i64);
    let get = |tag: u32| dyn_entries.iter().find(|&&(t, _)| t == tag as i64).map(|&(_, v)| v);

    assert!(has(elf::DT_INIT_ARRAY), ".dynamic should have DT_INIT_ARRAY");
    assert!(has(elf::DT_INIT_ARRAYSZ), ".dynamic should have DT_INIT_ARRAYSZ");
    let arraysz = get(elf::DT_INIT_ARRAYSZ).unwrap();
    assert_eq!(arraysz, 8, "DT_INIT_ARRAYSZ should be 8 (one pointer)");
    assert!(has(elf::DT_NULL), ".dynamic should terminate with DT_NULL");
}

#[test]
fn init_array_in_dynamic_shared() {
    let obj = build_init_array_obj();
    let elf_bytes = toyos_ld::link_shared(&[("test.o".into(), obj)])
        .expect("shared lib with .init_array should link");

    let elf = parse_elf(&elf_bytes);
    let dyn_entries = parse_dynamic(&elf);
    let has = |tag: u32| dyn_entries.iter().any(|&(t, _)| t == tag as i64);
    let get = |tag: u32| dyn_entries.iter().find(|&&(t, _)| t == tag as i64).map(|&(_, v)| v);

    assert!(has(elf::DT_INIT_ARRAY), ".dynamic should have DT_INIT_ARRAY");
    let arraysz = get(elf::DT_INIT_ARRAYSZ).unwrap();
    assert_eq!(arraysz, 8, "DT_INIT_ARRAYSZ should be 8");
}

#[test]
fn init_array_survives_gc() {
    let obj = build_init_array_obj();
    let elf_bytes = toyos_ld::link_with(&[("test.o".into(), obj)], "_start", true)
        .expect("GC with .init_array should link");

    let elf = parse_elf(&elf_bytes);
    assert!(has_phdr(&elf, elf::PT_DYNAMIC), ".init_array should survive gc");
    let dyn_entries = parse_dynamic(&elf);
    let has = |tag: u32| dyn_entries.iter().any(|&(t, _)| t == tag as i64);
    assert!(has(elf::DT_INIT_ARRAY), ".init_array should survive gc as root");
}

// ── .eh_frame / .eh_frame_hdr ───────────────────────────────────────────

#[test]
fn eh_frame_hdr_present_pie() {
    let obj = build_eh_frame_obj();
    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj)], "_start")
        .expect(".eh_frame should link");

    let elf = parse_elf(&elf_bytes);
    let hdr = find_section(&elf, ".eh_frame_hdr");
    assert!(hdr.is_some(), "PIE output should have .eh_frame_hdr section");
}

#[test]
fn eh_frame_pt_gnu_eh_frame_pie() {
    let obj = build_eh_frame_obj();
    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj)], "_start")
        .expect(".eh_frame should link");

    let elf = parse_elf(&elf_bytes);
    assert!(has_phdr(&elf, 0x6474_e550), "PIE should have PT_GNU_EH_FRAME program header");
}

#[test]
fn eh_frame_hdr_present_shared() {
    let obj = build_eh_frame_obj();
    let elf_bytes = toyos_ld::link_shared(&[("test.o".into(), obj)])
        .expect(".eh_frame shared should link");

    let elf = parse_elf(&elf_bytes);
    let hdr = find_section(&elf, ".eh_frame_hdr");
    assert!(hdr.is_some(), "shared lib should have .eh_frame_hdr section");
    assert!(has_phdr(&elf, 0x6474_e550), "shared lib should have PT_GNU_EH_FRAME");
}

#[test]
fn eh_frame_hdr_sorted_table() {
    let obj = build_eh_frame_obj();
    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj)], "_start")
        .expect(".eh_frame should link");

    let elf = parse_elf(&elf_bytes);
    let hdr = find_section(&elf, ".eh_frame_hdr").unwrap();
    let data = hdr.data().unwrap();

    assert!(data.len() >= 12, ".eh_frame_hdr should be at least 12 bytes");
    assert_eq!(data[0], 1, "version should be 1");
    assert_eq!(data[1], 0x1B, "eh_frame_ptr encoding should be pcrel|sdata4");
    assert_eq!(data[2], 0x03, "fde_count encoding should be udata4");
    assert_eq!(data[3], 0x3B, "table encoding should be datarel|sdata4");

    let fde_count = u32::from_le_bytes(data[8..12].try_into().unwrap());
    assert_eq!(fde_count, 1, "should have exactly 1 FDE entry");

    assert_eq!(data.len(), 12 + 8, "table should have 1 entry of 8 bytes");
}

#[test]
fn no_eh_frame_hdr_without_eh_frame() {
    let obj = ObjBuilder::elf().func("_start", &[0xC3]).build();
    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj)], "_start")
        .expect("should link without .eh_frame");

    let elf = parse_elf(&elf_bytes);
    assert!(find_section(&elf, ".eh_frame_hdr").is_none(), "no .eh_frame_hdr without .eh_frame");
    assert!(!has_phdr(&elf, 0x6474_e550), "no PT_GNU_EH_FRAME without .eh_frame");
}

// ── .gnu.hash ───────────────────────────────────────────────────────────

#[test]
fn gnu_hash_present_in_shared() {
    let obj = ObjBuilder::elf().func("foo", &[0xC3]).func("bar", &[0xC3]).build();
    let elf_bytes = toyos_ld::link_shared(&[("test.o".into(), obj)])
        .expect("shared lib should link");

    let elf = parse_elf(&elf_bytes);
    let sec = find_section(&elf, ".gnu.hash");
    assert!(sec.is_some(), "shared lib should have .gnu.hash section");
}

#[test]
fn gnu_hash_dt_entry() {
    let obj = ObjBuilder::elf().func("foo", &[0xC3]).build();
    let elf_bytes = toyos_ld::link_shared(&[("test.o".into(), obj)])
        .expect("shared lib should link");

    let elf = parse_elf(&elf_bytes);
    let dyn_entries = parse_dynamic(&elf);
    let has_gnu_hash = dyn_entries.iter().any(|&(t, _)| t == elf::DT_GNU_HASH as i64);
    assert!(has_gnu_hash, ".dynamic should have DT_GNU_HASH entry");
}

// ── --build-id ──────────────────────────────────────────────────────────

#[test]
fn build_id_present_pie() {
    let obj = ObjBuilder::elf().func("_start", &[0xC3]).build();
    let elf_bytes = toyos_ld::link_full(&[("test.o".into(), obj)], "_start", false, true)
        .expect("PIE with build-id should link");

    let elf = parse_elf(&elf_bytes);
    let note = find_section(&elf, ".note.gnu.build-id");
    assert!(note.is_some(), "PIE should have .note.gnu.build-id section");
    assert!(has_phdr(&elf, elf::PT_NOTE), "PIE should have PT_NOTE program header");

    let data = note.unwrap().data().unwrap();
    assert_eq!(data.len(), 36, "note should be 36 bytes");
    let namesz = u32::from_le_bytes(data[0..4].try_into().unwrap());
    let descsz = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let note_type = u32::from_le_bytes(data[8..12].try_into().unwrap());
    assert_eq!(namesz, 4);
    assert_eq!(descsz, 20);
    assert_eq!(note_type, 3);
    assert_eq!(&data[12..16], b"GNU\0");
    assert!(data[16..36].iter().any(|&b| b != 0), "build-id hash should be non-zero");
}

#[test]
fn build_id_present_static() {
    let obj = ObjBuilder::elf().func("_start", &[0xC3]).build();
    let elf_bytes = toyos_ld::link_static_full(&[("test.o".into(), obj)], "_start", 0x200000, false, true)
        .expect("static with build-id should link");

    let elf = parse_elf(&elf_bytes);
    assert!(find_section(&elf, ".note.gnu.build-id").is_some(), "static should have .note.gnu.build-id");
    assert!(has_phdr(&elf, elf::PT_NOTE), "static should have PT_NOTE");
}

#[test]
fn build_id_present_shared() {
    let obj = ObjBuilder::elf().func("foo", &[0xC3]).build();
    let elf_bytes = toyos_ld::link_shared_full(&[("test.o".into(), obj)], true)
        .expect("shared with build-id should link");

    let elf = parse_elf(&elf_bytes);
    assert!(find_section(&elf, ".note.gnu.build-id").is_some(), "shared should have .note.gnu.build-id");
    assert!(has_phdr(&elf, elf::PT_NOTE), "shared should have PT_NOTE");
}

#[test]
fn build_id_deterministic() {
    let obj = ObjBuilder::elf().func("_start", &[0xC3]).build();
    let elf1 = toyos_ld::link_full(&[("test.o".into(), obj.clone())], "_start", false, true).unwrap();
    let elf2 = toyos_ld::link_full(&[("test.o".into(), obj)], "_start", false, true).unwrap();
    assert_eq!(elf1, elf2, "same input should produce identical output with same build-id");
}

#[test]
fn no_build_id_by_default() {
    let obj = ObjBuilder::elf().func("_start", &[0xC3]).build();
    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj)], "_start").unwrap();

    let elf = parse_elf(&elf_bytes);
    assert!(find_section(&elf, ".note.gnu.build-id").is_none(), "no build-id by default");
    assert!(!has_phdr(&elf, elf::PT_NOTE), "no PT_NOTE by default");
}

// ── Section merging ─────────────────────────────────────────────────────

#[test]
fn string_merging_deduplicates() {
    let obj1 = build_merge_string_obj("_start", &[b"hello\0", b"world\0"]);
    let obj2 = build_merge_string_obj("other", &[b"hello\0", b"unique\0"]);

    let elf_bytes = toyos_ld::link(
        &[("a.o".into(), obj1), ("b.o".into(), obj2)],
        "_start",
    ).unwrap();

    let elf = parse_elf(&elf_bytes);
    let text = find_section(&elf, ".text").expect(".text");
    let text_data = text.data().unwrap();
    assert!(text_data.windows(6).any(|w| w == b"hello\0"), "\"hello\" in output");
    assert!(text_data.windows(6).any(|w| w == b"world\0"), "\"world\" in output");
    assert!(text_data.windows(7).any(|w| w == b"unique\0"), "\"unique\" in output");

    let hello_count = text_data.windows(6).filter(|w| *w == b"hello\0").count();
    assert_eq!(hello_count, 1, "\"hello\" should be deduplicated to one copy");
}

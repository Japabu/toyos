mod common;
use common::*;
use object::pe;
use object::read::{Object, ObjectSection};
use object::write::{Object as WriteObject, StandardSection, Symbol, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, RelocationFlags, SymbolFlags, SymbolKind, SymbolScope,
};

// ── PE output tests ─────────────────────────────────────────────────────

#[test]
fn pe_valid_headers() {
    let obj_data = build_minimal_obj("efi_main", &[0xC3]);

    let pe = toyos_ld::link_pe(
        &[("test.o".into(), obj_data)], "efi_main", 10,
    ).expect("PE linking should succeed");

    assert_eq!(pe_u16(&pe, 0), 0x5A4D, "should start with MZ magic");
    let pe_offset = pe_u32(&pe, 0x3C) as usize;
    assert_eq!(pe_offset, 0x40, "e_lfanew should point to PE signature");
    assert_eq!(pe_u32(&pe, pe_offset), 0x00004550, "should have PE\\0\\0 signature");

    let coff = pe_offset + 4;
    assert_eq!(pe_u16(&pe, coff), 0x8664, "Machine should be AMD64");

    let oh = coff + 20;
    assert_eq!(pe_u16(&pe, oh), 0x020B, "should be PE32+ (0x020B)");
}

#[test]
fn pe_sections() {
    let obj_data = build_minimal_obj("efi_main", &[0xC3]);

    let pe = toyos_ld::link_pe(
        &[("test.o".into(), obj_data)], "efi_main", 10,
    ).expect("PE linking should succeed");

    let coff = 0x44;
    let num_sections = pe_u16(&pe, coff + 2) as usize;
    assert!(num_sections >= 2, "should have at least .text and .reloc sections");

    let sh_base = 0x58 + 240;
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
    let obj = ObjBuilder::elf()
        .func("dummy", &[0x90, 0xC3])
        .func("efi_main", &[0x31, 0xC0, 0xC3])
        .build();
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj)], "efi_main", 10)
        .expect("PE linking should succeed");

    let oh = 0x58;
    let entry_rva = pe_u32(&pe, oh + 0x10);
    assert!(entry_rva > 0, "AddressOfEntryPoint should be non-zero");

    let text_rva = pe_u32(&pe, oh + 0x14);
    assert!(entry_rva > text_rva, "entry should not be at start of .text (dummy func is there)");
}

#[test]
fn pe_subsystem() {
    let obj_data = build_minimal_obj("efi_main", &[0xC3]);

    let pe = toyos_ld::link_pe(&[("test.o".into(), obj_data.clone())], "efi_main", 10).unwrap();
    let oh = 0x58;
    assert_eq!(pe_u16(&pe, oh + 0x44), 10, "subsystem should be EFI_APPLICATION (10)");

    let pe = toyos_ld::link_pe(&[("test.o".into(), obj_data.clone())], "efi_main", 11).unwrap();
    assert_eq!(pe_u16(&pe, oh + 0x44), 11, "subsystem should be EFI_BOOT_SERVICE_DRIVER (11)");

    let pe = toyos_ld::link_pe(&[("test.o".into(), obj_data)], "efi_main", 12).unwrap();
    assert_eq!(pe_u16(&pe, oh + 0x44), 12, "subsystem should be EFI_RUNTIME_DRIVER (12)");
}

#[test]
fn pe_base_relocations() {
    let obj = ObjBuilder::elf().func("efi_main", &[0xC3]).data_ptr_to("efi_main").build();
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj)], "efi_main", 10)
        .expect("PE with R_X86_64_64 should succeed");

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

    let reloc_data = &pe[reloc_file_off as usize..(reloc_file_off + reloc_raw_size) as usize];
    assert!(reloc_data.len() >= 12, ".reloc should have at least one block with one entry");

    let page_rva = pe_u32(reloc_data, 0);
    let block_size = pe_u32(reloc_data, 4);
    assert!(block_size >= 12, "block should have header + at least one entry");
    assert!(page_rva > 0, "page_rva should be non-zero");

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
    let obj = ObjBuilder::elf().func("helper", &[0xC3]).func_calling("efi_main", "helper").build();
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj)], "efi_main", 10)
        .expect("PE with only PC-relative relocs should succeed");

    let oh = 0x58;
    let dd5 = oh + 0x70 + 5 * 8;
    let reloc_dir_size = pe_u32(&pe, dd5 + 4);
    assert_eq!(reloc_dir_size, 0, "no base relocations needed for PC-relative only code");
}

#[test]
fn pe_undefined_symbol_error() {
    let obj = ObjBuilder::elf().func_calling("efi_main", "missing").build();
    let result = toyos_ld::link_pe(&[("test.o".into(), obj)], "efi_main", 10);
    let err = result.expect_err("should fail with undefined symbol");
    match &err {
        toyos_ld::LinkError::UndefinedSymbols(syms) => assert!(syms.contains(&"missing".to_string())),
        other => panic!("expected UndefinedSymbols, got: {other:?}"),
    }
}

#[test]
fn pe_dll_characteristics_dynamic_base() {
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
    let code = vec![0x31, 0xFF, 0xB8, 0x3C, 0x00, 0x00, 0x00, 0x0F, 0x05];
    let obj_data = build_minimal_coff("_start", &code);
    let elf_bytes = toyos_ld::link(&[("test.o".into(), obj_data)], "_start")
        .expect("linking COFF input should succeed");
    let elf = parse_elf(&elf_bytes);
    let endian = elf.endian();
    assert_eq!(elf.elf_header().e_type.get(endian), 3, "should be ET_DYN");
    assert_eq!(elf.elf_header().e_machine.get(endian), 62, "should be x86_64");
    let entry = elf.elf_header().e_entry.get(endian);
    assert!(entry > 0, "entry should be nonzero");
}

#[test]
fn coff_input_link_pe() {
    let obj_data = build_minimal_coff("efi_main", &[0xC3]);
    let pe = toyos_ld::link_pe(&[("test.o".into(), obj_data)], "efi_main", 10)
        .expect("linking COFF→PE should succeed");
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
    let mut obj = WriteObject::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let data = obj.section_id(StandardSection::Data);

    let data_off = obj.append_section_data(data, &[0x42; 8], 8);
    let data_sym = obj.add_symbol(Symbol {
        name: b"my_data".to_vec(),
        value: data_off, size: 8,
        kind: SymbolKind::Data, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(data), flags: SymbolFlags::None,
    });

    let code = [0x48, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0xC3];
    let code_off = obj.append_section_data(text, &code, 16);
    obj.add_relocation(text, object::write::Relocation {
        offset: code_off + 2, symbol: data_sym, addend: 0,
        flags: RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_ADDR64 },
    }).unwrap();
    obj.add_symbol(Symbol {
        name: b"efi_main".to_vec(),
        value: code_off, size: code.len() as u64,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    let pe = toyos_ld::link_pe(
        &[("test.o".into(), obj.write().unwrap())], "efi_main", 10,
    ).expect("COFF with ADDR64 should link to PE");

    let pe_off = pe_u32(&pe, 0x3C) as usize;
    let oh = pe_off + 4 + 20;
    let dd5 = oh + 0x70 + 5 * 8;
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
    let weak_obj = build_coff_with_weak_external("efi_main", &[0xC3]);
    toyos_ld::link_pe(&[("builtins.o".into(), weak_obj)], "efi_main", 10)
        .expect("COFF weak external should resolve for PE output");
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

    let secs = pe_section_list(&pe);
    let text_rva = secs[0].1;
    assert_eq!(pe_read_at_rva(&pe, text_rva, 1), &[0xC3], "callee should be at text_rva");
    assert_eq!(target_rva, text_rva,
        "call target should be callee RVA: disp={disp}, got {target_rva:#x}, want {text_rva:#x}");
}

#[test]
fn coff_jump_table_implicit_addend() {
    let mut obj = WriteObject::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let rdata = obj.add_section(Vec::new(), b".rdata".to_vec(), object::SectionKind::ReadOnlyData);

    let bb_off = obj.append_section_data(text, &[0xC3], 16);
    let bb_sym = obj.add_symbol(Symbol {
        name: b"bb0".to_vec(), value: bb_off, size: 1,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    let jt_off = obj.append_section_data(rdata, &[0u8; 12], 4);
    for i in 0..3u64 {
        obj.add_relocation(rdata, object::write::Relocation {
            offset: jt_off + i * 4,
            symbol: bb_sym,
            addend: i as i64 * 4,
            flags: RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_REL32 },
        }).unwrap();
    }

    let entry_off = obj.append_section_data(text, &[0xC3], 16);
    obj.add_symbol(Symbol {
        name: b"efi_main".to_vec(), value: entry_off, size: 1,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    let pe = toyos_ld::link_pe(
        &[("test.o".into(), obj.write().unwrap())], "efi_main", 10,
    ).expect("linking should succeed");

    let secs = pe_section_list(&pe);
    let text_rva = secs[0].1;
    assert_eq!(pe_read_at_rva(&pe, text_rva, 1), &[0xC3], "bb0 should be at start of .text");
    let bb0_rva = text_rva;

    let mut jt_rva = 0u32;
    for (_, va, vs, rp, _) in &secs {
        let sec_data = &pe[*rp as usize..(*rp + *vs) as usize];
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
        &[("test.o".into(), obj.write().unwrap())], "efi_main", 10,
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

        obj.append_section_data(rdata_a, &[0xAA; 8], 8);
        obj.append_section_data(rdata_b, &[0xBB; 8], 8);

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

        let mut code = Vec::new();
        code.extend_from_slice(&[0x48, 0x8d, 0x05]);
        let reloc_off_a = code.len() as u64;
        code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        code.extend_from_slice(&[0x48, 0x8d, 0x05]);
        let reloc_off_b = code.len() as u64;
        code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        code.push(0xC3);

        obj.append_section_data(text, &code, 16);

        obj.add_symbol(Symbol {
            name: b"efi_main".to_vec(), value: 0, size: 0,
            kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
            weak: false, section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });

        obj.add_relocation(text, object::write::Relocation {
            offset: reloc_off_a, symbol: sym_a, addend: -4,
            flags: RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_REL32 },
        }).unwrap();
        obj.add_relocation(text, object::write::Relocation {
            offset: reloc_off_b, symbol: sym_b, addend: -4,
            flags: RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_REL32 },
        }).unwrap();

        obj.write().unwrap()
    };

    let obj = object::read::File::parse(coff_bytes.as_slice()).unwrap();
    let rdata_count = obj.sections()
        .filter(|s| s.name().unwrap_or("") == ".rdata")
        .count();
    assert!(rdata_count >= 2, "test object must have multiple .rdata sections, got {rdata_count}");

    let pe = toyos_ld::link_pe(
        &[("test.o".into(), coff_bytes)], "efi_main", 10,
    ).expect("linking should succeed");

    let pe_text = pe_section_list(&pe);
    let (_, text_va, text_vs, text_rp, _) = &pe_text[0];
    let text_data = &pe[*text_rp as usize..(*text_rp + *text_vs) as usize];

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

    let target_rva_a = (*text_va + lea_pos_a as u32 + 7) as i32 + disp_a;
    let target_rva_b = (*text_va + lea_pos_b as u32 + 7) as i32 + disp_b;
    assert_ne!(target_rva_a, target_rva_b,
        "LEAs must target different RVAs (different .rdata sections), \
         but both point to {target_rva_a:#x}");

    let marker_a = pe_read_at_rva(&pe, target_rva_a as u32, 8);
    let marker_b = pe_read_at_rva(&pe, target_rva_b as u32, 8);
    assert_eq!(marker_a, &[0xAA; 8],
        "first LEA should target 0xAA data, got {marker_a:02x?}");
    assert_eq!(marker_b, &[0xBB; 8],
        "second LEA should target 0xBB data, got {marker_b:02x?}");
}

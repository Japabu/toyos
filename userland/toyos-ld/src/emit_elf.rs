use object::write::elf::{FileHeader, ProgramHeader, SectionHeader, Sym, Rel, Writer, SymbolIndex};
use object::write::StringId;
use object::Endianness;
use crate::collect::{collect_unique_symbols, InputSection, LinkState, DYNAMIC_SYMBOL_SENTINEL};
use crate::reloc::{RelocOutput, resolve_symbol, tpoff};
use crate::{align_up, classify_sections, BASE_VADDR, PAGE_SIZE};
use object::elf;
use std::collections::{HashMap, HashSet};

// ── Layout ───────────────────────────────────────────────────────────────

pub(crate) struct ElfLayout {
    pub(crate) base_addr: u64,
    pub(crate) rx_start: u64,
    pub(crate) rx_end: u64,
    pub(crate) rw_start: u64,
    pub(crate) rw_end: u64,
    pub(crate) tls_start: u64,
    pub(crate) tls_filesz: u64,
    pub(crate) tls_memsz: u64,
    pub(crate) got: HashMap<String, u64>,
    pub(crate) plt: HashMap<String, u64>,
    pub(crate) plt_data: Vec<u8>,
    pub(crate) plt_vaddr: u64,
    pub(crate) dyn_got: HashMap<String, u64>,
}

pub(crate) fn layout_elf(state: &mut LinkState, base_addr: u64, entry_name: Option<&str>) -> ElfLayout {
    let headers_size = 0x1000u64;
    let buckets = classify_sections(state);

    let mut cursor = base_addr + headers_size;

    let rx_start = cursor;
    for &idx in &buckets.rx {
        let sec = &mut state.sections[idx];
        cursor = align_up(cursor, sec.align);
        sec.vaddr = cursor;
        cursor += sec.size;
    }

    // PLT stubs for dynamic symbols (PIE mode only)
    let mut dyn_syms = collect_unique_symbols(
        state.relocs.iter(),
        |r| state.dynamic_imports.contains(&r.symbol_name),
    );
    if let Some(entry) = entry_name {
        if state.dynamic_imports.contains(entry) && !dyn_syms.iter().any(|s| s == entry) {
            dyn_syms.push(entry.to_string());
        }
    }

    const PLT_STUB_SIZE: u64 = 6;
    let plt_vaddr = if dyn_syms.is_empty() { cursor } else { align_up(cursor, 16) };
    cursor = plt_vaddr + dyn_syms.len() as u64 * PLT_STUB_SIZE;

    let rx_end = align_up(cursor, PAGE_SIZE);

    cursor = rx_end;
    let rw_start = cursor;
    for &idx in &buckets.rw {
        let sec = &mut state.sections[idx];
        cursor = align_up(cursor, sec.align);
        sec.vaddr = cursor;
        cursor += sec.size;
    }

    let got_symbols = collect_unique_symbols(state.relocs.iter(), |r| {
        matches!(r.r_type,
            elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX
            | elf::R_X86_64_REX_GOTPCRELX | elf::R_X86_64_GOTTPOFF)
    });

    cursor = align_up(cursor, 8);
    let mut got = HashMap::new();
    for sym in &got_symbols {
        got.insert(sym.clone(), cursor);
        cursor += 8;
    }

    let mut dyn_got = HashMap::new();
    for sym in &dyn_syms {
        dyn_got.insert(sym.clone(), cursor);
        cursor += 8;
    }

    let rw_end = align_up(cursor, PAGE_SIZE);

    // Build PLT stub code: `jmp *[rip + offset]` (FF 25 xx xx xx xx)
    let mut plt = HashMap::new();
    let mut plt_data = Vec::new();
    for (i, sym) in dyn_syms.iter().enumerate() {
        let stub_vaddr = plt_vaddr + i as u64 * PLT_STUB_SIZE;
        plt.insert(sym.clone(), stub_vaddr);
        let rip = stub_vaddr + 6;
        let offset = (dyn_got[sym] as i64 - rip as i64) as i32;
        plt_data.extend_from_slice(&[0xFF, 0x25]);
        plt_data.extend_from_slice(&offset.to_le_bytes());
    }

    // TLS layout
    let tls_start = align_up(rw_end, 64);
    let mut tls_cursor = tls_start;
    for &idx in &buckets.tls {
        let sec = &mut state.sections[idx];
        tls_cursor = align_up(tls_cursor, sec.align);
        sec.vaddr = tls_cursor;
        tls_cursor += sec.size;
    }
    let tls_filesz = buckets.tls
        .iter()
        .filter(|&&idx| !state.sections[idx].name.starts_with(".tbss"))
        .map(|&idx| state.sections[idx].size)
        .sum::<u64>();
    let tls_memsz = if buckets.tls.is_empty() { 0 } else { tls_cursor - tls_start };

    ElfLayout {
        base_addr, rx_start, rx_end, rw_start, rw_end,
        tls_start, tls_filesz, tls_memsz,
        got, plt, plt_data, plt_vaddr, dyn_got,
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn resolve_entry(state: &LinkState, entry_name: &str, plt: Option<&HashMap<String, u64>>) -> u64 {
    state
        .globals
        .get(entry_name)
        .map(|def| {
            if def.section_global_idx == DYNAMIC_SYMBOL_SENTINEL {
                plt.and_then(|p| p.get(entry_name).copied())
                    .unwrap_or_else(|| panic!("toyos-ld: entry '{entry_name}' is in .so but has no PLT entry"))
            } else {
                state.sections[def.section_global_idx].vaddr + def.value
            }
        })
        .unwrap_or_else(|| panic!("toyos-ld: entry symbol '{entry_name}' not found"))
}

/// Write section data to the Writer in file-offset order.
fn write_sections_data(w: &mut Writer, sections: &[InputSection], base: u64) {
    let mut indices: Vec<usize> = (0..sections.len())
        .filter(|&i| sections[i].vaddr != 0 && !sections[i].data.is_empty())
        .collect();
    indices.sort_by_key(|&i| sections[i].vaddr);
    for i in indices {
        let file_off = (sections[i].vaddr - base) as usize;
        w.pad_until(file_off);
        w.write(&sections[i].data);
    }
}

// ── PIE ELF output ───────────────────────────────────────────────────────

pub(crate) fn emit_bytes(
    state: &LinkState,
    layout: &ElfLayout,
    relocs: &RelocOutput,
    entry_name: &str,
) -> Vec<u8> {
    let is_dynamic = !state.dynamic_libs.is_empty();
    let entry = resolve_entry(state, entry_name, Some(&layout.plt));
    let after_rw = layout.rw_end.max(layout.tls_start + layout.tls_memsz);

    let mut buf = Vec::new();
    let mut w = Writer::new(Endianness::Little, true, &mut buf);

    // ── Phase 1: Add strings and reserve ──

    // Section names (only for sections without convenience methods)
    let text_name = w.add_section_name(b".text");
    let data_name = w.add_section_name(b".data");
    let rela_name = w.add_section_name(b".rela.dyn");

    // Dynamic strings
    let mut needed_str_ids = Vec::new();
    let mut sym_str_ids: Vec<(String, StringId)> = Vec::new();
    let mut sym_to_writer_idx: HashMap<String, SymbolIndex> = HashMap::new();

    if is_dynamic {
        for lib in &state.dynamic_libs {
            needed_str_ids.push(w.add_dynamic_string(lib.as_bytes()));
        }
        for (_, sym_name) in &relocs.glob_dats {
            if !sym_to_writer_idx.contains_key(sym_name) {
                let str_id = w.add_dynamic_string(sym_name.as_bytes());
                sym_str_ids.push((sym_name.clone(), str_id));
                // placeholder — real index comes from reserve below
                sym_to_writer_idx.insert(sym_name.clone(), SymbolIndex(0));
            }
        }
    }

    // Reserve section indices
    w.reserve_null_section_index();
    w.reserve_section_index(); // .text
    w.reserve_section_index(); // .data

    let dynsym_sec_idx = if is_dynamic {
        Some(w.reserve_dynsym_section_index())
    } else { None };
    if is_dynamic {
        w.reserve_dynstr_section_index();
    }
    w.reserve_section_index(); // .rela.dyn
    if is_dynamic {
        w.reserve_dynamic_section_index();
    }
    w.reserve_shstrtab_section_index();

    // Reserve dynamic symbol indices
    if is_dynamic {
        w.reserve_null_dynamic_symbol_index();
        for (sym_name, _) in &sym_str_ids {
            let idx = w.reserve_dynamic_symbol_index();
            sym_to_writer_idx.insert(sym_name.clone(), idx);
        }
    }

    // Reserve file layout
    w.reserve_file_header();
    let phdr_count = 2 + if layout.tls_memsz > 0 { 1 } else { 0 }
        + if is_dynamic { 2 } else { 0 };
    w.reserve_program_headers(phdr_count as u32);

    // Reserve section data area
    w.reserve_until(after_rw as usize);

    // Dynamic metadata
    let rela_count = relocs.relatives.len() + if is_dynamic { relocs.glob_dats.len() } else { 0 };
    let rela_size = rela_count as u64 * 24;

    let (dynsym_off, dynstr_off, rela_off, dynamic_off, dyn_segment_end);
    if is_dynamic {
        dynsym_off = w.reserve_dynsym() as u64;
        dynstr_off = w.reserve_dynstr() as u64;
        rela_off = w.reserve_relocations(rela_count, true) as u64;
        let dynamic_count = needed_str_ids.len() + 4 + 3 + 1;
        dynamic_off = w.reserve_dynamic(dynamic_count) as u64;
        dyn_segment_end = align_up(w.reserved_len() as u64, PAGE_SIZE);
        w.reserve_until(dyn_segment_end as usize);
    } else {
        dynsym_off = 0;
        dynstr_off = 0;
        rela_off = w.reserve_relocations(rela_count, true) as u64;
        dynamic_off = 0;
        dyn_segment_end = 0;
    }

    w.reserve_shstrtab();
    w.reserve_section_headers();

    // ── Phase 2: Write ──

    // File header
    w.write_file_header(&FileHeader {
        os_abi: 0,
        abi_version: 0,
        e_type: elf::ET_DYN,
        e_machine: elf::EM_X86_64,
        e_entry: entry,
        e_flags: 0,
    }).unwrap();

    // Program headers
    w.write_align_program_headers();
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R | elf::PF_X,
        p_offset: BASE_VADDR,
        p_vaddr: BASE_VADDR,
        p_paddr: BASE_VADDR,
        p_filesz: layout.rx_end - BASE_VADDR,
        p_memsz: layout.rx_end - BASE_VADDR,
        p_align: PAGE_SIZE,
    });
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R | elf::PF_W,
        p_offset: layout.rw_start,
        p_vaddr: layout.rw_start,
        p_paddr: layout.rw_start,
        p_filesz: layout.rw_end - layout.rw_start,
        p_memsz: layout.rw_end - layout.rw_start,
        p_align: PAGE_SIZE,
    });
    if layout.tls_memsz > 0 {
        w.write_program_header(&ProgramHeader {
            p_type: elf::PT_TLS,
            p_flags: elf::PF_R,
            p_offset: layout.tls_start,
            p_vaddr: layout.tls_start,
            p_paddr: layout.tls_start,
            p_filesz: layout.tls_filesz,
            p_memsz: layout.tls_memsz,
            p_align: 64,
        });
    }
    if is_dynamic {
        let dynamic_size = (needed_str_ids.len() as u64 + 4 + 3 + 1) * 16;
        w.write_program_header(&ProgramHeader {
            p_type: elf::PT_LOAD,
            p_flags: elf::PF_R,
            p_offset: dynsym_off,
            p_vaddr: dynsym_off,
            p_paddr: dynsym_off,
            p_filesz: dyn_segment_end - dynsym_off,
            p_memsz: dyn_segment_end - dynsym_off,
            p_align: PAGE_SIZE,
        });
        w.write_program_header(&ProgramHeader {
            p_type: elf::PT_DYNAMIC,
            p_flags: elf::PF_R,
            p_offset: dynamic_off,
            p_vaddr: dynamic_off,
            p_paddr: dynamic_off,
            p_filesz: dynamic_size,
            p_memsz: dynamic_size,
            p_align: 8,
        });
    }

    // Section data
    write_sections_data(&mut w, &state.sections, BASE_VADDR);
    if !layout.plt_data.is_empty() {
        w.pad_until(layout.plt_vaddr as usize);
        w.write(&layout.plt_data);
    }
    w.pad_until(after_rw as usize);

    // Dynamic symbols
    if is_dynamic {
        w.write_null_dynamic_symbol();
        for (_, str_id) in &sym_str_ids {
            w.write_dynamic_symbol(&Sym {
                name: Some(*str_id),
                section: None,
                st_info: (elf::STB_GLOBAL << 4) | elf::STT_NOTYPE,
                st_other: elf::STV_DEFAULT,
                st_shndx: 0,
                st_value: 0,
                st_size: 0,
            });
        }
        w.write_dynstr();
    }

    // Relocations
    w.write_align_relocation();
    for &(offset, addend) in &relocs.relatives {
        w.write_relocation(true, &Rel {
            r_offset: offset,
            r_sym: 0,
            r_type: elf::R_X86_64_RELATIVE,
            r_addend: addend,
        });
    }
    if is_dynamic {
        for (got_vaddr, sym_name) in &relocs.glob_dats {
            let sym_idx = sym_to_writer_idx[sym_name];
            w.write_relocation(true, &Rel {
                r_offset: *got_vaddr,
                r_sym: sym_idx.0,
                r_type: elf::R_X86_64_GLOB_DAT,
                r_addend: 0,
            });
        }
    }

    // Dynamic section
    if is_dynamic {
        w.write_align_dynamic();
        for &str_id in &needed_str_ids {
            w.write_dynamic_string(elf::DT_NEEDED as u32, str_id);
        }
        w.write_dynamic(elf::DT_SYMTAB as u32, dynsym_off);
        w.write_dynamic(elf::DT_STRTAB as u32, dynstr_off);
        let strsz = w.dynstr_len() as u64;
        w.write_dynamic(elf::DT_STRSZ as u32, strsz);
        w.write_dynamic(elf::DT_SYMENT as u32, 24);
        w.write_dynamic(elf::DT_RELA as u32, rela_off);
        w.write_dynamic(elf::DT_RELASZ as u32, rela_size);
        w.write_dynamic(elf::DT_RELAENT as u32, 24);
        w.write_dynamic(elf::DT_NULL as u32, 0);
        w.pad_until(dyn_segment_end as usize);
    }

    // shstrtab
    w.write_shstrtab();

    // Section headers
    w.write_null_section_header();
    w.write_section_header(&SectionHeader {
        name: Some(text_name),
        sh_type: elf::SHT_PROGBITS,
        sh_flags: (elf::SHF_ALLOC | elf::SHF_EXECINSTR) as u64,
        sh_addr: layout.rx_start,
        sh_offset: layout.rx_start - BASE_VADDR,
        sh_size: layout.rx_end - layout.rx_start,
        sh_link: 0, sh_info: 0, sh_addralign: 1, sh_entsize: 0,
    });
    w.write_section_header(&SectionHeader {
        name: Some(data_name),
        sh_type: elf::SHT_PROGBITS,
        sh_flags: (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
        sh_addr: layout.rw_start,
        sh_offset: layout.rw_start - BASE_VADDR,
        sh_size: layout.rw_end - layout.rw_start,
        sh_link: 0, sh_info: 0, sh_addralign: 1, sh_entsize: 0,
    });

    if is_dynamic {
        w.write_dynsym_section_header(dynsym_off, 1);
        w.write_dynstr_section_header(dynstr_off);
    }

    // .rela.dyn
    w.write_section_header(&SectionHeader {
        name: Some(rela_name),
        sh_type: elf::SHT_RELA,
        sh_flags: elf::SHF_ALLOC as u64,
        sh_addr: if is_dynamic { rela_off } else { 0 },
        sh_offset: rela_off,
        sh_size: rela_size,
        sh_link: if is_dynamic { dynsym_sec_idx.unwrap().0 } else { 0 },
        sh_info: 0,
        sh_addralign: 8,
        sh_entsize: 24,
    });

    if is_dynamic {
        w.write_dynamic_section_header(dynamic_off);
    }

    w.write_shstrtab_section_header();

    buf
}

// ── Static ELF output ────────────────────────────────────────────────────

pub(crate) fn emit_static_bytes(
    state: &LinkState,
    layout: &ElfLayout,
    entry_name: &str,
) -> Vec<u8> {
    let entry = resolve_entry(state, entry_name, None);
    let base = layout.base_addr;
    let after_rw = layout.rw_end.max(layout.tls_start + layout.tls_memsz);

    let mut buf = Vec::new();
    let mut w = Writer::new(Endianness::Little, true, &mut buf);

    // ── Phase 1: Reserve ──
    let text_name = w.add_section_name(b".text");
    let data_name = w.add_section_name(b".data");

    w.reserve_null_section_index();
    w.reserve_section_index(); // .text
    w.reserve_section_index(); // .data
    w.reserve_shstrtab_section_index();

    w.reserve_file_header();
    let phdr_count = 2 + if layout.tls_memsz > 0 { 1 } else { 0 };
    w.reserve_program_headers(phdr_count as u32);

    // Section data + GOT entries
    w.reserve_until((after_rw - base) as usize);

    w.reserve_shstrtab();
    w.reserve_section_headers();

    // ── Phase 2: Write ──
    w.write_file_header(&FileHeader {
        os_abi: 0,
        abi_version: 0,
        e_type: elf::ET_EXEC,
        e_machine: elf::EM_X86_64,
        e_entry: entry,
        e_flags: 0,
    }).unwrap();

    // Program headers
    w.write_align_program_headers();
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R | elf::PF_X,
        p_offset: 0,
        p_vaddr: base,
        p_paddr: base,
        p_filesz: layout.rx_end - base,
        p_memsz: layout.rx_end - base,
        p_align: PAGE_SIZE,
    });
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R | elf::PF_W,
        p_offset: layout.rw_start - base,
        p_vaddr: layout.rw_start,
        p_paddr: layout.rw_start,
        p_filesz: layout.rw_end - layout.rw_start,
        p_memsz: layout.rw_end - layout.rw_start,
        p_align: PAGE_SIZE,
    });
    if layout.tls_memsz > 0 {
        w.write_program_header(&ProgramHeader {
            p_type: elf::PT_TLS,
            p_flags: elf::PF_R,
            p_offset: layout.tls_start - base,
            p_vaddr: layout.tls_start,
            p_paddr: layout.tls_start,
            p_filesz: layout.tls_filesz,
            p_memsz: layout.tls_memsz,
            p_align: 64,
        });
    }

    // Section data
    write_sections_data(&mut w, &state.sections, base);

    // GOT entries (filled directly in static mode)
    let gottpoff_syms: HashSet<String> = state.relocs.iter()
        .filter(|r| r.r_type == elf::R_X86_64_GOTTPOFF)
        .map(|r| r.symbol_name.clone()).collect();
    let mut got_entries: Vec<_> = layout.got.iter().collect();
    got_entries.sort_by_key(|(_, &vaddr)| vaddr);
    for (sym_name, &got_vaddr) in got_entries {
        let sym_addr = resolve_symbol(state, sym_name, 0, None)
            .unwrap_or_else(|| panic!("toyos-ld: undefined GOT symbol: {sym_name}"));
        let value = if gottpoff_syms.contains(sym_name) {
            tpoff(sym_addr, layout.tls_start, layout.tls_memsz) as u64
        } else { sym_addr };
        let file_off = (got_vaddr - base) as usize;
        w.pad_until(file_off);
        w.write(&value.to_le_bytes());
    }

    w.pad_until((after_rw - base) as usize);

    // shstrtab + section headers
    w.write_shstrtab();

    w.write_null_section_header();
    w.write_section_header(&SectionHeader {
        name: Some(text_name),
        sh_type: elf::SHT_PROGBITS,
        sh_flags: (elf::SHF_ALLOC | elf::SHF_EXECINSTR) as u64,
        sh_addr: layout.rx_start,
        sh_offset: layout.rx_start - base,
        sh_size: layout.rx_end - layout.rx_start,
        sh_link: 0, sh_info: 0, sh_addralign: 1, sh_entsize: 0,
    });
    w.write_section_header(&SectionHeader {
        name: Some(data_name),
        sh_type: elf::SHT_PROGBITS,
        sh_flags: (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
        sh_addr: layout.rw_start,
        sh_offset: layout.rw_start - base,
        sh_size: layout.rw_end - layout.rw_start,
        sh_link: 0, sh_info: 0, sh_addralign: 1, sh_entsize: 0,
    });
    w.write_shstrtab_section_header();

    buf
}

// ── Shared library output ────────────────────────────────────────────────

pub(crate) fn emit_shared_bytes(
    state: &LinkState,
    layout: &ElfLayout,
    relocs: &RelocOutput,
) -> Vec<u8> {
    let after_rw = layout.rw_end.max(layout.tls_start + layout.tls_memsz);

    let mut buf = Vec::new();
    let mut w = Writer::new(Endianness::Little, true, &mut buf);

    // ── Phase 1: Reserve ──

    let text_name = w.add_section_name(b".text");
    let data_name = w.add_section_name(b".data");
    let rela_name = w.add_section_name(b".rela.dyn");

    // Add dynamic strings for exported symbols
    let mut symbols: Vec<_> = state.globals.iter().collect();
    symbols.sort_by_key(|(name, _)| *name);
    let mut sym_str_ids: Vec<(String, StringId, u64)> = Vec::new();
    for (name, def) in &symbols {
        if def.section_global_idx == DYNAMIC_SYMBOL_SENTINEL {
            continue;
        }
        let str_id = w.add_dynamic_string(name.as_bytes());
        let st_value = state.sections[def.section_global_idx].vaddr + def.value;
        sym_str_ids.push((name.to_string(), str_id, st_value));
    }

    // Metadata section names
    let mut meta_names: Vec<StringId> = Vec::new();
    for (name, _) in &state.metadata {
        meta_names.push(w.add_section_name(name.as_bytes()));
    }

    // Reserve section indices
    w.reserve_null_section_index();
    w.reserve_section_index(); // .text
    w.reserve_section_index(); // .data
    w.reserve_section_index(); // .rela.dyn
    w.reserve_dynsym_section_index();
    w.reserve_dynstr_section_index();
    w.reserve_dynamic_section_index();
    for _ in &state.metadata {
        w.reserve_section_index(); // metadata sections
    }
    w.reserve_shstrtab_section_index();

    // Reserve dynamic symbol indices
    w.reserve_null_dynamic_symbol_index();
    for _ in &sym_str_ids {
        w.reserve_dynamic_symbol_index();
    }

    // Reserve file layout
    w.reserve_file_header();
    let phdr_count = 4 + if layout.tls_memsz > 0 { 1 } else { 0 };
    w.reserve_program_headers(phdr_count as u32);

    w.reserve_until(after_rw as usize);

    let dynsym_off = w.reserve_dynsym() as u64;
    let dynstr_off = w.reserve_dynstr() as u64;
    let dynamic_count = 5; // SYMTAB + STRTAB + STRSZ + SYMENT + NULL
    let dynamic_off = w.reserve_dynamic(dynamic_count) as u64;
    let dyn_segment_end = align_up(w.reserved_len() as u64, PAGE_SIZE);
    w.reserve_until(dyn_segment_end as usize);

    let rela_count = relocs.relatives.len();
    let rela_size = rela_count as u64 * 24;
    let rela_off = w.reserve_relocations(rela_count, true) as u64;

    // Metadata sections
    let mut meta_offsets = Vec::new();
    for (_, data) in &state.metadata {
        let off = w.reserve(data.len(), 8);
        meta_offsets.push(off as u64);
    }

    w.reserve_shstrtab();
    w.reserve_section_headers();

    // ── Phase 2: Write ──

    w.write_file_header(&FileHeader {
        os_abi: 0,
        abi_version: 0,
        e_type: elf::ET_DYN,
        e_machine: elf::EM_X86_64,
        e_entry: 0,
        e_flags: 0,
    }).unwrap();

    // Program headers
    w.write_align_program_headers();
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R | elf::PF_X,
        p_offset: BASE_VADDR,
        p_vaddr: BASE_VADDR,
        p_paddr: BASE_VADDR,
        p_filesz: layout.rx_end - BASE_VADDR,
        p_memsz: layout.rx_end - BASE_VADDR,
        p_align: PAGE_SIZE,
    });
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R | elf::PF_W,
        p_offset: layout.rw_start,
        p_vaddr: layout.rw_start,
        p_paddr: layout.rw_start,
        p_filesz: layout.rw_end - layout.rw_start,
        p_memsz: layout.rw_end - layout.rw_start,
        p_align: PAGE_SIZE,
    });
    let dynamic_size = dynamic_count as u64 * 16;
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R,
        p_offset: dynsym_off,
        p_vaddr: dynsym_off,
        p_paddr: dynsym_off,
        p_filesz: dyn_segment_end - dynsym_off,
        p_memsz: dyn_segment_end - dynsym_off,
        p_align: PAGE_SIZE,
    });
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_DYNAMIC,
        p_flags: elf::PF_R,
        p_offset: dynamic_off,
        p_vaddr: dynamic_off,
        p_paddr: dynamic_off,
        p_filesz: dynamic_size,
        p_memsz: dynamic_size,
        p_align: 8,
    });
    if layout.tls_memsz > 0 {
        w.write_program_header(&ProgramHeader {
            p_type: elf::PT_TLS,
            p_flags: elf::PF_R,
            p_offset: layout.tls_start,
            p_vaddr: layout.tls_start,
            p_paddr: layout.tls_start,
            p_filesz: layout.tls_filesz,
            p_memsz: layout.tls_memsz,
            p_align: 64,
        });
    }

    // Section data
    write_sections_data(&mut w, &state.sections, BASE_VADDR);
    w.pad_until(after_rw as usize);

    // Dynamic symbols
    w.write_null_dynamic_symbol();
    for (_, str_id, st_value) in &sym_str_ids {
        w.write_dynamic_symbol(&Sym {
            name: Some(*str_id),
            section: None,
            st_info: (elf::STB_GLOBAL << 4) | elf::STT_NOTYPE,
            st_other: elf::STV_DEFAULT,
            st_shndx: 1, // defined (non-zero)
            st_value: *st_value,
            st_size: 0,
        });
    }
    w.write_dynstr();

    // Dynamic section
    w.write_align_dynamic();
    w.write_dynamic(elf::DT_SYMTAB as u32, dynsym_off);
    w.write_dynamic(elf::DT_STRTAB as u32, dynstr_off);
    let strsz = w.dynstr_len() as u64;
    w.write_dynamic(elf::DT_STRSZ as u32, strsz);
    w.write_dynamic(elf::DT_SYMENT as u32, 24);
    w.write_dynamic(elf::DT_NULL as u32, 0);
    w.pad_until(dyn_segment_end as usize);

    // Relocations
    w.write_align_relocation();
    for &(offset, addend) in &relocs.relatives {
        w.write_relocation(true, &Rel {
            r_offset: offset,
            r_sym: 0,
            r_type: elf::R_X86_64_RELATIVE,
            r_addend: addend,
        });
    }

    // Metadata sections
    for (i, (_, data)) in state.metadata.iter().enumerate() {
        w.pad_until(meta_offsets[i] as usize);
        w.write(data);
    }

    // shstrtab + section headers
    w.write_shstrtab();

    w.write_null_section_header();
    w.write_section_header(&SectionHeader {
        name: Some(text_name),
        sh_type: elf::SHT_PROGBITS,
        sh_flags: (elf::SHF_ALLOC | elf::SHF_EXECINSTR) as u64,
        sh_addr: layout.rx_start,
        sh_offset: layout.rx_start - BASE_VADDR,
        sh_size: layout.rx_end - layout.rx_start,
        sh_link: 0, sh_info: 0, sh_addralign: 1, sh_entsize: 0,
    });
    w.write_section_header(&SectionHeader {
        name: Some(data_name),
        sh_type: elf::SHT_PROGBITS,
        sh_flags: (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
        sh_addr: layout.rw_start,
        sh_offset: layout.rw_start - BASE_VADDR,
        sh_size: layout.rw_end - layout.rw_start,
        sh_link: 0, sh_info: 0, sh_addralign: 1, sh_entsize: 0,
    });
    w.write_section_header(&SectionHeader {
        name: Some(rela_name),
        sh_type: elf::SHT_RELA,
        sh_flags: elf::SHF_ALLOC as u64,
        sh_addr: 0,
        sh_offset: rela_off,
        sh_size: rela_size,
        sh_link: 0, sh_info: 0, sh_addralign: 8, sh_entsize: 24,
    });
    w.write_dynsym_section_header(dynsym_off, 1);
    w.write_dynstr_section_header(dynstr_off);
    w.write_dynamic_section_header(dynamic_off);
    for (i, (_, data)) in state.metadata.iter().enumerate() {
        w.write_section_header(&SectionHeader {
            name: Some(meta_names[i]),
            sh_type: elf::SHT_PROGBITS,
            sh_flags: 0,
            sh_addr: 0,
            sh_offset: meta_offsets[i],
            sh_size: data.len() as u64,
            sh_link: 0, sh_info: 0, sh_addralign: 1, sh_entsize: 0,
        });
    }
    w.write_shstrtab_section_header();

    buf
}

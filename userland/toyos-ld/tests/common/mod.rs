#![allow(dead_code, unused_imports)]

use object::{elf, pe};
use object::read::elf::{ElfFile64, FileHeader as _};
use object::read::{Object, ObjectSection, ObjectSymbol};
use object::write::{Object as WriteObject, StandardSection, Symbol, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, RelocationFlags, SymbolFlags, SymbolKind, SymbolScope,
};

// ── Object builder ──────────────────────────────────────────────────────

/// Fluent builder for constructing test ELF/COFF object files.
pub struct ObjBuilder {
    obj: WriteObject<'static>,
    text: object::write::SectionId,
}

impl ObjBuilder {
    pub fn elf() -> Self {
        let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let text = obj.section_id(StandardSection::Text);
        Self { obj, text }
    }

    pub fn coff() -> Self {
        let mut obj = WriteObject::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
        let text = obj.section_id(StandardSection::Text);
        Self { obj, text }
    }

    /// Add a global function symbol with the given code.
    pub fn func(mut self, name: &str, code: &[u8]) -> Self {
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
    pub fn func_calling(mut self, name: &str, callee: &str) -> Self {
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
    pub fn data_ptr_to(mut self, target_sym_name: &str) -> Self {
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
    pub fn weak_func(mut self, name: &str, code: &[u8]) -> Self {
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
    pub fn undefined(mut self, name: &str) -> Self {
        self.obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: 0, size: 0,
            kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
            weak: false, section: SymbolSection::Undefined, flags: SymbolFlags::None,
        });
        self
    }

    /// Add a BSS (uninitialized data) section with a global symbol.
    pub fn bss(mut self, name: &str, size: u64) -> Self {
        let bss_sec = self.obj.section_id(StandardSection::UninitializedData);
        let off = self.obj.append_section_bss(bss_sec, size, 8);
        self.obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: off, size,
            kind: SymbolKind::Data, scope: SymbolScope::Dynamic,
            weak: false, section: SymbolSection::Section(bss_sec), flags: SymbolFlags::None,
        });
        self
    }

    /// Get mutable access to the underlying WriteObject for advanced construction.
    pub fn inner_mut(&mut self) -> &mut WriteObject<'static> {
        &mut self.obj
    }

    pub fn build(self) -> Vec<u8> {
        self.obj.write().unwrap()
    }

    pub fn named(self, name: &str) -> (String, Vec<u8>) {
        (name.into(), self.build())
    }
}

pub fn build_minimal_obj(name: &str, code: &[u8]) -> Vec<u8> {
    ObjBuilder::elf().func(name, code).build()
}

pub fn build_minimal_coff(name: &str, code: &[u8]) -> Vec<u8> {
    ObjBuilder::coff().func(name, code).build()
}

// ── ELF parsing helpers ─────────────────────────────────────────────────

pub fn parse_elf(data: &[u8]) -> ElfFile64<'_> {
    ElfFile64::parse(data).expect("output should be valid ELF")
}

pub fn has_phdr(elf: &ElfFile64<'_>, p_type: u32) -> bool {
    let endian = elf.endian();
    elf.elf_header()
        .program_headers(endian, elf.data())
        .unwrap()
        .iter()
        .any(|ph| ph.p_type.get(endian) == p_type)
}

pub fn find_section<'a>(elf: &'a ElfFile64<'a>, name: &str) -> Option<object::read::elf::ElfSection64<'a, 'a>> {
    elf.sections().find(|s| s.name().unwrap_or("") == name)
}

pub fn dynsym_names(elf: &ElfFile64<'_>) -> Vec<String> {
    elf.dynamic_symbols()
        .filter_map(|s| s.name().ok().map(|n| n.to_string()))
        .filter(|n| !n.is_empty())
        .collect()
}

/// Parse .dynamic section entries, returning (tag, value) pairs.
pub fn parse_dynamic(elf: &ElfFile64<'_>) -> Vec<(i64, u64)> {
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

pub fn symtab_names(elf: &ElfFile64<'_>) -> Vec<String> {
    elf.symbols()
        .filter(|s| !s.name().unwrap_or("").is_empty())
        .map(|s| s.name().unwrap().to_string())
        .collect()
}

// ── PE parsing helpers ──────────────────────────────────────────────────

pub fn pe_u16(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(data[off..off + 2].try_into().unwrap())
}

pub fn pe_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
}

pub fn pe_u64(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
}

pub fn pe_section_name(data: &[u8], sh_off: usize) -> String {
    let raw = &data[sh_off..sh_off + 8];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(8);
    String::from_utf8_lossy(&raw[..end]).to_string()
}

pub fn pe_section_characteristics(data: &[u8], sh_off: usize) -> u32 {
    pe_u32(data, sh_off + 36)
}

/// Parse PE section info. Returns vec of (name, va, virt_size, raw_ptr, raw_size).
pub fn pe_section_list(pe: &[u8]) -> Vec<(String, u32, u32, u32, u32)> {
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
pub fn pe_read_at_rva(pe: &[u8], rva: u32, len: usize) -> &[u8] {
    for (_, va, vs, rp, _) in pe_section_list(pe) {
        if rva >= va && rva + len as u32 <= va + vs {
            let off = (rp + (rva - va)) as usize;
            return &pe[off..off + len];
        }
    }
    panic!("RVA {rva:#x} not in any PE section");
}

pub fn pe_read_i32_at_rva(pe: &[u8], rva: u32) -> i32 {
    let b = pe_read_at_rva(pe, rva, 4);
    i32::from_le_bytes(b.try_into().unwrap())
}

pub fn pe_entry_rva(pe: &[u8]) -> u32 {
    let pe_off = pe_u32(pe, 0x3C) as usize;
    pe_u32(pe, pe_off + 4 + 20 + 16) // OptionalHeader.AddressOfEntryPoint
}

// ── Specialized object builders ─────────────────────────────────────────

pub fn build_coff_with_weak_external(weak_name: &str, code: &[u8]) -> Vec<u8> {
    ObjBuilder::coff().weak_func(weak_name, code).build()
}

/// Build a minimal ar archive from named .o byte slices.
pub fn build_archive(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut ar = b"!<arch>\n".to_vec();
    for (name, data) in members {
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}\x60\n",
            format!("{name}/"), "0", "0", "0", "100644", data.len()
        );
        ar.extend_from_slice(header.as_bytes());
        ar.extend_from_slice(data);
        if data.len() % 2 != 0 {
            ar.push(b'\n');
        }
    }
    ar
}

/// Build an object with a relocation of the given type from .text to a .data symbol.
pub fn build_reloc_overflow_obj(r_type: u32) -> Vec<u8> {
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let code = [0x90; 16];
    let off = obj.append_section_data(text, &code, 16);
    let data_sec = obj.section_id(StandardSection::Data);
    let data_off = obj.append_section_data(data_sec, &[0u8; 8], 8);
    let data_sym = obj.add_symbol(Symbol {
        name: b"my_data".to_vec(),
        value: data_off, size: 8,
        kind: SymbolKind::Data, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(data_sec), flags: SymbolFlags::None,
    });
    obj.add_symbol(Symbol {
        name: b"_start".to_vec(),
        value: off, size: code.len() as u64,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });
    obj.add_relocation(text, object::write::Relocation {
        offset: off + 4, symbol: data_sym, addend: 0,
        flags: RelocationFlags::Elf { r_type },
    }).unwrap();
    obj.write().unwrap()
}

/// Build an object with _start and an .init_array section containing a pointer to _start.
pub fn build_init_array_obj() -> Vec<u8> {
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let code = &[0xC3u8];
    let off = obj.append_section_data(text, code, 16);
    let start_sym = obj.add_symbol(Symbol {
        name: b"_start".to_vec(),
        value: off, size: code.len() as u64,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    let init_sec = obj.add_section(vec![], b".init_array".to_vec(),
        object::SectionKind::Elf(elf::SHT_INIT_ARRAY));
    obj.section_mut(init_sec).flags = object::SectionFlags::Elf {
        sh_flags: (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
    };
    let ptr_off = obj.append_section_data(init_sec, &[0u8; 8], 8);
    obj.add_relocation(init_sec, object::write::Relocation {
        offset: ptr_off, symbol: start_sym, addend: 0,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_64 },
    }).unwrap();

    obj.write().unwrap()
}

/// Build a test object with _start + a valid .eh_frame containing one CIE and one FDE.
pub fn build_eh_frame_obj() -> Vec<u8> {
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let code = [0xC3u8];
    let off = obj.append_section_data(text, &code, 16);
    let start_sym = obj.add_symbol(Symbol {
        name: b"_start".to_vec(),
        value: off, size: code.len() as u64,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    let mut eh_frame = Vec::new();

    // CIE record
    let mut cie_body = Vec::new();
    cie_body.extend_from_slice(&0u32.to_le_bytes());
    cie_body.push(1);
    cie_body.extend_from_slice(b"zR\0");
    cie_body.push(1);
    cie_body.push(0x78);
    cie_body.push(16);
    cie_body.push(1);
    cie_body.push(0x1B); // DW_EH_PE_pcrel | DW_EH_PE_sdata4
    cie_body.push(0x0C); cie_body.push(7); cie_body.push(8);
    while (4 + cie_body.len()) % 4 != 0 { cie_body.push(0); }
    let cie_length = cie_body.len() as u32;
    eh_frame.extend_from_slice(&cie_length.to_le_bytes());
    let cie_offset = 0;
    eh_frame.extend_from_slice(&cie_body);

    // FDE record
    let fde_start = eh_frame.len();
    let mut fde_body = Vec::new();
    let cie_ptr = (fde_start + 4) as u32 - cie_offset as u32;
    fde_body.extend_from_slice(&cie_ptr.to_le_bytes());
    let loc_offset_in_fde = fde_body.len();
    fde_body.extend_from_slice(&0i32.to_le_bytes());
    fde_body.extend_from_slice(&(code.len() as u32).to_le_bytes());
    fde_body.push(0);
    while (4 + fde_body.len()) % 4 != 0 { fde_body.push(0); }
    let fde_length = fde_body.len() as u32;
    eh_frame.extend_from_slice(&fde_length.to_le_bytes());
    eh_frame.extend_from_slice(&fde_body);

    // Null terminator
    eh_frame.extend_from_slice(&0u32.to_le_bytes());

    let eh_sec = obj.add_section(vec![], b".eh_frame".to_vec(), object::SectionKind::ReadOnlyData);
    obj.section_mut(eh_sec).flags = object::SectionFlags::Elf {
        sh_flags: (elf::SHF_ALLOC) as u64,
    };
    let eh_data_off = obj.append_section_data(eh_sec, &eh_frame, 8);

    let reloc_offset = eh_data_off + fde_start as u64 + 4 + loc_offset_in_fde as u64;
    obj.add_relocation(eh_sec, object::write::Relocation {
        offset: reloc_offset,
        symbol: start_sym,
        addend: 0,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PC32 },
    }).unwrap();

    obj.write().unwrap()
}

/// Build an ELF .o with a function and a .rodata.str1.1 section (SHF_MERGE|SHF_STRINGS).
pub fn build_merge_string_obj(func_name: &str, strings: &[&[u8]]) -> Vec<u8> {
    let mut obj = WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    let code = &[0xC3];
    let off = obj.append_section_data(text, code, 16);
    obj.add_symbol(Symbol {
        name: func_name.as_bytes().to_vec(),
        value: off, size: code.len() as u64,
        kind: SymbolKind::Text, scope: SymbolScope::Dynamic,
        weak: false, section: SymbolSection::Section(text), flags: SymbolFlags::None,
    });

    let str_sec = obj.add_section(vec![], b".rodata.str1.1".to_vec(),
        object::SectionKind::ReadOnlyString);
    obj.section_mut(str_sec).flags = object::SectionFlags::Elf {
        sh_flags: (elf::SHF_ALLOC | elf::SHF_MERGE | elf::SHF_STRINGS) as u64,
    };
    for s in strings {
        obj.append_section_data(str_sec, s, 1);
    }

    obj.write().unwrap()
}

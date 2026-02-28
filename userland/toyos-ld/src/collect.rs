use object::elf;
use object::pe;
use object::read::elf::ElfFile64;
use object::read::{self, Object, ObjectSection, ObjectSymbol};
use object::RelocationFlags;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

#[derive(Clone)]
pub(crate) struct InputSection {
    pub(crate) obj_idx: usize,
    pub(crate) name: String,
    pub(crate) data: Vec<u8>,
    pub(crate) align: u64,
    pub(crate) size: u64,
    pub(crate) vaddr: u64,
    pub(crate) writable: bool,
}

#[derive(Clone)]
pub(crate) struct InputReloc {
    pub(crate) section_global_idx: usize,
    pub(crate) offset: u64,
    pub(crate) r_type: u32,
    pub(crate) symbol_name: String,
    pub(crate) addend: i64,
}

#[derive(Clone, Copy)]
pub(crate) struct SymbolDef {
    pub(crate) section_global_idx: usize,
    pub(crate) value: u64,
}

/// Sentinel: symbols provided by .so inputs have this section index.
pub(crate) const DYNAMIC_SYMBOL_SENTINEL: usize = usize::MAX;

pub(crate) struct LinkState {
    pub(crate) sections: Vec<InputSection>,
    pub(crate) relocs: Vec<InputReloc>,
    pub(crate) globals: HashMap<String, SymbolDef>,
    pub(crate) locals: HashMap<(usize, String), SymbolDef>,
    pub(crate) tls_sections: Vec<usize>,
    /// Non-loadable metadata sections (e.g. .rustc) preserved in shared library output.
    pub(crate) metadata: Vec<(String, Vec<u8>)>,
    /// Symbol names provided by shared library (.so) inputs.
    pub(crate) dynamic_imports: HashSet<String>,
    /// Bare filenames of .so inputs (for DT_NEEDED entries).
    pub(crate) dynamic_libs: Vec<String>,
}

pub(crate) fn collect(objects: &[(String, Vec<u8>)]) -> LinkState {
    let mut state = LinkState {
        sections: Vec::new(),
        relocs: Vec::new(),
        globals: HashMap::new(),
        locals: HashMap::new(),
        tls_sections: Vec::new(),
        metadata: Vec::new(),
        dynamic_imports: HashSet::new(),
        dynamic_libs: Vec::new(),
    };

    let mut sec_map: HashMap<(usize, object::SectionIndex), usize> = HashMap::new();

    for (obj_idx, (name, data)) in objects.iter().enumerate() {
        // ELF shared library input: extract dynamic symbols, skip section processing.
        // Shared libraries are always ELF, so try ELF-specific parse first.
        if let Ok(elf) = ElfFile64::parse(data.as_slice()) {
            if elf.elf_header().e_type.get(object::Endianness::Little) == elf::ET_DYN {
                collect_so_symbols(&elf, &mut state.globals, &mut state.dynamic_imports);
                let filename = std::path::Path::new(name)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if !state.dynamic_libs.contains(&filename) {
                    state.dynamic_libs.push(filename);
                }
                continue;
            }
        }

        // Generic parse: handles both ELF .o and COFF .o
        let obj = object::File::parse(data.as_slice())
            .unwrap_or_else(|e| panic!("toyos-ld: cannot parse {name}: {e}"));

        collect_object(&mut state, &obj, obj_idx, &mut sec_map);
    }

    state
}

/// Extract exported dynamic symbols from an ET_DYN ELF (.so) and add them
/// to `globals` with a sentinel section index. These symbols satisfy undefined
/// references without contributing any code/data to the output.
fn collect_so_symbols(elf: &ElfFile64, globals: &mut HashMap<String, SymbolDef>, dynamic_imports: &mut HashSet<String>) {
    for sym in elf.dynamic_symbols() {
        let name = match sym.name() {
            Ok(n) if !n.is_empty() => n,
            _ => continue,
        };
        // Only defined symbols (not UND)
        if sym.is_undefined() {
            continue;
        }
        let name = name.to_string();
        globals.entry(name.clone()).or_insert(SymbolDef {
            section_global_idx: DYNAMIC_SYMBOL_SENTINEL,
            value: 0,
        });
        dynamic_imports.insert(name);
    }
}

/// Resolve a relocation's target to a symbol name. Section symbols get synthetic
/// names keyed on (obj_idx, global_section_idx) for unique resolution.
fn resolve_reloc_target(
    obj: &object::File,
    reloc: &read::Relocation,
    obj_idx: usize,
    sec_map: &HashMap<(usize, object::SectionIndex), usize>,
    state: &mut LinkState,
) -> Option<String> {
    let sym_idx = match reloc.target() {
        read::RelocationTarget::Symbol(idx) => idx,
        _ => return None,
    };
    let sym = obj.symbol_by_index(sym_idx).ok()?;
    let name = sym.name().unwrap_or("");

    // Section symbols need unique synthetic names because COFF objects can have
    // multiple sections with the same name (e.g. many `.rdata` COMDAT sections).
    let is_section_sym = name.is_empty() || sym.kind() == read::SymbolKind::Section;
    if is_section_sym {
        let si = match sym.section() {
            read::SymbolSection::Section(si) => si,
            _ => return None,
        };
        let &gsec = sec_map.get(&(obj_idx, si))?;
        let syn = format!("__section_sym_{}_{}", obj_idx, gsec);
        state.locals.entry((obj_idx, syn.clone())).or_insert(SymbolDef {
            section_global_idx: gsec,
            value: sym.address(),
        });
        Some(syn)
    } else {
        Some(name.to_string())
    }
}

/// Collect sections, symbols, and relocations from a single object file.
/// Works with both ELF and COFF objects via the generic `Object` trait.
fn collect_object(
    state: &mut LinkState,
    obj: &object::File,
    obj_idx: usize,
    sec_map: &mut HashMap<(usize, object::SectionIndex), usize>,
) {
    for section in obj.sections() {
        let sec_name = section.name().unwrap_or("");

        // Capture metadata sections (e.g. .rustc) regardless of SectionKind
        if sec_name.starts_with(".rustc") {
            let data = section.data().unwrap_or(&[]).to_vec();
            if !data.is_empty() {
                state.metadata.push((sec_name.to_string(), data));
            }
            continue;
        }

        match section.kind() {
            read::SectionKind::Text
            | read::SectionKind::Data
            | read::SectionKind::ReadOnlyData
            | read::SectionKind::ReadOnlyDataWithRel
            | read::SectionKind::ReadOnlyString
            | read::SectionKind::UninitializedData
            | read::SectionKind::OtherString
            | read::SectionKind::Tls
            | read::SectionKind::UninitializedTls => {}
            _ => continue,
        }

        let sec_data = section.data().unwrap_or(&[]).to_vec();
        let global_idx = state.sections.len();
        sec_map.insert((obj_idx, section.index()), global_idx);

        let is_tls = matches!(
            section.kind(),
            read::SectionKind::Tls | read::SectionKind::UninitializedTls
        );
        let writable = matches!(
            section.kind(),
            read::SectionKind::Data | read::SectionKind::UninitializedData
        );

        state.sections.push(InputSection {
            obj_idx,
            name: sec_name.to_string(),
            data: sec_data,
            align: section.align().max(1),
            size: section.size(),
            vaddr: 0,
            writable,
        });

        if is_tls {
            state.tls_sections.push(global_idx);
        }
    }

    for symbol in obj.symbols() {
        let sym_name = match symbol.name() {
            Ok(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };
        if symbol.is_undefined() {
            continue;
        }
        // Skip section symbols — relocations resolve these via synthetic
        // names keyed on section index, so they don't belong in locals.
        if symbol.kind() == read::SymbolKind::Section {
            continue;
        }
        let sec_idx = match symbol.section() {
            read::SymbolSection::Section(idx) => idx,
            _ => continue,
        };
        let global_sec = match sec_map.get(&(obj_idx, sec_idx)) {
            Some(&g) => g,
            None => continue,
        };
        let def = SymbolDef {
            section_global_idx: global_sec,
            value: symbol.address(),
        };
        if symbol.is_global() {
            // COFF weak externals: `.weak.FOO.default` (LLVM) or `.weak.FOO` (object crate)
            // provides the actual code for `FOO`. Register the alias as a global too.
            if let Some(rest) = sym_name.strip_prefix(".weak.") {
                let alias = rest.strip_suffix(".default").unwrap_or(rest);
                let alias = alias.to_string();
                match state.globals.get(&alias) {
                    Some(existing) if existing.section_global_idx != DYNAMIC_SYMBOL_SENTINEL => {}
                    _ => { state.globals.insert(alias, def); }
                }
            }
            // Concrete .o definitions always override .so dynamic imports
            match state.globals.get(&sym_name) {
                Some(existing) if existing.section_global_idx != DYNAMIC_SYMBOL_SENTINEL => {}
                _ => { state.globals.insert(sym_name, def); }
            }
        } else {
            if let Some(existing) = state.locals.get(&(obj_idx, sym_name.clone())) {
                assert_eq!(
                    existing.section_global_idx, def.section_global_idx,
                    "local symbol {sym_name:?} in obj {obj_idx} defined in two \
                     different sections ({} vs {})",
                    existing.section_global_idx, def.section_global_idx
                );
            }
            state.locals.insert((obj_idx, sym_name), def);
        }
    }

    for section in obj.sections() {
        match section.kind() {
            read::SectionKind::Text
            | read::SectionKind::Data
            | read::SectionKind::ReadOnlyData
            | read::SectionKind::ReadOnlyDataWithRel
            | read::SectionKind::ReadOnlyString
            | read::SectionKind::OtherString
            | read::SectionKind::Tls => {}
            _ => continue,
        }
        let global_sec = match sec_map.get(&(obj_idx, section.index())) {
            Some(&g) => g,
            None => continue,
        };

        for (offset, reloc) in section.relocations() {
            let sym_name = match resolve_reloc_target(obj, &reloc, obj_idx, sec_map, state) {
                Some(name) => name,
                None => continue,
            };
            let r_type = match reloc.flags() {
                RelocationFlags::Elf { r_type } => r_type,
                RelocationFlags::Coff { typ } => coff_to_elf_r_type(typ),
                _ => continue,
            };

            // COFF uses implicit addends stored in the section data, while ELF RELA
            // uses explicit addends. The `object` crate returns the COFF-specific
            // base adjustment (e.g. -4 for REL32) but sets `has_implicit_addend`,
            // meaning we must also read the value from the section data and add it.
            let addend = if reloc.has_implicit_addend() {
                let data = &state.sections[global_sec].data;
                let off = offset as usize;
                let implicit = match reloc.size() {
                    64 => i64::from_le_bytes(data[off..off + 8].try_into().unwrap()),
                    32 => i32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as i64,
                    16 => i16::from_le_bytes(data[off..off + 2].try_into().unwrap()) as i64,
                    _ => 0,
                };
                reloc.addend() + implicit
            } else {
                reloc.addend()
            };

            state.relocs.push(InputReloc {
                section_global_idx: global_sec,
                offset,
                r_type,
                symbol_name: sym_name,
                addend,
            });
        }
    }
}

/// Map COFF x86_64 relocation types to their ELF equivalents.
fn coff_to_elf_r_type(typ: u16) -> u32 {
    match typ {
        pe::IMAGE_REL_AMD64_ADDR64 => elf::R_X86_64_64,
        pe::IMAGE_REL_AMD64_ADDR32 => elf::R_X86_64_32,
        pe::IMAGE_REL_AMD64_ADDR32NB => elf::R_X86_64_32S,
        pe::IMAGE_REL_AMD64_REL32
        | pe::IMAGE_REL_AMD64_REL32_1
        | pe::IMAGE_REL_AMD64_REL32_2
        | pe::IMAGE_REL_AMD64_REL32_3
        | pe::IMAGE_REL_AMD64_REL32_4
        | pe::IMAGE_REL_AMD64_REL32_5 => elf::R_X86_64_PLT32,
        pe::IMAGE_REL_AMD64_SECREL => elf::R_X86_64_32,
        other => panic!("toyos-ld: unsupported COFF relocation type 0x{other:04x}"),
    }
}

pub(crate) fn is_archive(data: &[u8]) -> bool {
    data.starts_with(b"!<arch>\n") || data.starts_with(b"!<thin>\n")
}

pub(crate) fn extract_archive(name: &str, data: &[u8], out: &mut Vec<(String, Vec<u8>)>) {
    let archive = object::read::archive::ArchiveFile::parse(data)
        .unwrap_or_else(|e| panic!("toyos-ld: cannot parse archive {name}: {e}"));
    for member in archive.members() {
        let member = member
            .unwrap_or_else(|e| panic!("toyos-ld: bad archive member in {name}: {e}"));
        let member_name = String::from_utf8_lossy(member.name()).to_string();
        if !member_name.ends_with(".o") {
            continue;
        }
        let member_data = member.data(data)
            .unwrap_or_else(|e| panic!("toyos-ld: cannot read {member_name} in {name}: {e}"));
        out.push((format!("{name}({member_name})"), member_data.to_vec()));
    }
}

pub(crate) fn find_lib(name: &str, paths: &[PathBuf]) -> Option<(String, Vec<u8>)> {
    let exact = [format!("lib{name}.rlib"), format!("lib{name}.a")];
    for dir in paths {
        for candidate in &exact {
            let path = dir.join(candidate);
            if let Ok(data) = fs::read(&path) {
                return Some((path.display().to_string(), data));
            }
        }
    }
    // Hash-suffixed Rust rlibs (e.g. libstd-abc123.rlib)
    let prefix = format!("lib{name}-");
    for dir in paths {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let fname = fname.to_string_lossy();
                if fname.starts_with(&prefix)
                    && (fname.ends_with(".rlib") || fname.ends_with(".a"))
                {
                    if let Ok(data) = fs::read(entry.path()) {
                        return Some((entry.path().display().to_string(), data));
                    }
                }
            }
        }
    }
    None
}

// ── Allocator shim synthesis ─────────────────────────────────────────────
// rustc normally generates these during final linking. We synthesize them
// as real code sections: each __rust_X is a `jmp __rdl_X` trampoline,
// and __rust_no_alloc_shim_is_unstable_v2 is a single `ret`.

const ALLOC_SHIMS: &[(&str, &str)] = &[
    (
        "_RNvCs2fcwfXhWpkc_7___rustc12___rust_alloc",
        "_RNvCs2fcwfXhWpkc_7___rustc11___rdl_alloc",
    ),
    (
        "_RNvCs2fcwfXhWpkc_7___rustc14___rust_dealloc",
        "_RNvCs2fcwfXhWpkc_7___rustc13___rdl_dealloc",
    ),
    (
        "_RNvCs2fcwfXhWpkc_7___rustc14___rust_realloc",
        "_RNvCs2fcwfXhWpkc_7___rustc13___rdl_realloc",
    ),
    (
        "_RNvCs2fcwfXhWpkc_7___rustc19___rust_alloc_zeroed",
        "_RNvCs2fcwfXhWpkc_7___rustc18___rdl_alloc_zeroed",
    ),
];

const SHIM_NO_ALLOC_UNSTABLE: &str =
    "_RNvCs2fcwfXhWpkc_7___rustc35___rust_no_alloc_shim_is_unstable_v2";

pub(crate) fn synthesize_alloc_shims(state: &mut LinkState) {
    // Only create shims for symbols that are actually referenced but undefined
    let undefined: HashSet<String> = state
        .relocs
        .iter()
        .map(|r| r.symbol_name.clone())
        .filter(|name| !state.globals.contains_key(name))
        .collect();

    let synthetic_obj_idx = usize::MAX;

    // Each trampoline is: `jmp rel32` (E9 xx xx xx xx) = 5 bytes, padded to 16
    for &(shim_name, target_name) in ALLOC_SHIMS {
        if !undefined.contains(shim_name) {
            continue;
        }
        let mut code = vec![0xE9, 0, 0, 0, 0];
        code.resize(16, 0xCC); // pad with int3
        let sec_idx = state.sections.len();
        state.sections.push(InputSection {
            obj_idx: synthetic_obj_idx,
            name: format!(".text.{shim_name}"),
            data: code,
            align: 16,
            size: 16,
            vaddr: 0,
            writable: false,
        });
        state.globals.insert(
            shim_name.to_string(),
            SymbolDef { section_global_idx: sec_idx, value: 0 },
        );
        state.relocs.push(InputReloc {
            section_global_idx: sec_idx,
            offset: 1,
            r_type: elf::R_X86_64_PLT32,
            symbol_name: target_name.to_string(),
            addend: -4,
        });
    }

    // __rust_no_alloc_shim_is_unstable_v2: single `ret` (C3)
    if undefined.contains(SHIM_NO_ALLOC_UNSTABLE)
        && !state.globals.contains_key(SHIM_NO_ALLOC_UNSTABLE)
    {
        let mut code = vec![0xC3];
        code.resize(16, 0xCC);
        let sec_idx = state.sections.len();
        state.sections.push(InputSection {
            obj_idx: synthetic_obj_idx,
            name: format!(".text.{SHIM_NO_ALLOC_UNSTABLE}"),
            data: code,
            align: 16,
            size: 16,
            vaddr: 0,
            writable: false,
        });
        state.globals.insert(
            SHIM_NO_ALLOC_UNSTABLE.to_string(),
            SymbolDef { section_global_idx: sec_idx, value: 0 },
        );
    }
}

/// Collect unique symbols in insertion order (deduplicating with a HashSet).
pub(crate) fn collect_unique_symbols<'a>(
    relocs: impl Iterator<Item = &'a InputReloc>,
    predicate: impl Fn(&InputReloc) -> bool,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for reloc in relocs {
        if predicate(reloc) && seen.insert(reloc.symbol_name.clone()) {
            result.push(reloc.symbol_name.clone());
        }
    }
    result
}

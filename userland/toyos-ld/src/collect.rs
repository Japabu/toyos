use object::elf;
use object::pe;
use object::macho;
use object::read::elf::ElfFile64;
use object::read::{self, Object, ObjectSection, ObjectSymbol};
use object::RelocationFlags;
use crate::LinkError;
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
    pub(crate) nobits: bool,
    /// Section has SHF_MERGE flag (eligible for deduplication)
    pub(crate) merge: bool,
    /// Section has SHF_STRINGS flag (null-terminated string entries)
    pub(crate) strings: bool,
    /// Entry size for merge sections (e.g., 1 for .rodata.str1.1)
    pub(crate) entsize: u64,
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

pub(crate) fn collect(objects: &[(String, Vec<u8>)]) -> Result<LinkState, LinkError> {
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
            .map_err(|e| LinkError::Parse { file: name.clone(), message: e.to_string() })?;

        collect_object(&mut state, &obj, obj_idx, &mut sec_map);
    }

    Ok(state)
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
        let sec_addr = obj.section_by_index(si).map(|s| s.address()).unwrap_or(0);
        state.locals.entry((obj_idx, syn.clone())).or_insert(SymbolDef {
            section_global_idx: gsec,
            value: sym.address() - sec_addr,
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
            read::SectionKind::Elf(elf::SHT_INIT_ARRAY)
            | read::SectionKind::Elf(elf::SHT_FINI_ARRAY) => {}
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
        ) || matches!(
            section.kind(),
            read::SectionKind::Elf(elf::SHT_INIT_ARRAY)
            | read::SectionKind::Elf(elf::SHT_FINI_ARRAY)
        );
        let nobits = matches!(
            section.kind(),
            read::SectionKind::UninitializedData | read::SectionKind::UninitializedTls
        );

        // Extract ELF merge/strings flags
        let (merge, strings, entsize) = match section.flags() {
            read::SectionFlags::Elf { sh_flags } => {
                let m = (sh_flags & elf::SHF_MERGE as u64) != 0;
                let s = (sh_flags & elf::SHF_STRINGS as u64) != 0;
                (m, s, if m { section.file_range().map(|_| {
                    // entsize from ELF section header — parse from raw header
                    // For ReadOnlyString, the object crate detects SHF_STRINGS
                    // but doesn't directly expose entsize via ObjectSection.
                    // Common convention: section name .rodata.str1.N → entsize = N
                    // Fallback to 1 for string sections
                    if s { 1u64 } else { 0 }
                }).unwrap_or(0) } else { 0 })
            }
            _ => (false, false, 0),
        };

        state.sections.push(InputSection {
            obj_idx,
            name: sec_name.to_string(),
            data: sec_data,
            align: section.align().max(1),
            size: section.size(),
            vaddr: 0,
            writable,
            nobits,
            merge,
            strings,
            entsize,
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
        let sec_addr = obj.section_by_index(sec_idx).map(|s| s.address()).unwrap_or(0);
        let def = SymbolDef {
            section_global_idx: global_sec,
            value: symbol.address() - sec_addr,
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
            | read::SectionKind::Tls
            | read::SectionKind::Elf(elf::SHT_INIT_ARRAY)
            | read::SectionKind::Elf(elf::SHT_FINI_ARRAY) => {}
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
            let (r_type, is_macho_instruction) = match reloc.flags() {
                RelocationFlags::Elf { r_type } => (r_type, false),
                RelocationFlags::Coff { typ } => match coff_to_elf_r_type(typ) {
                    Some(r) => (r, false),
                    None => continue,
                },
                RelocationFlags::MachO { r_type, r_length, .. } => {
                    match macho_arm64_to_elf_r_type(r_type, r_length) {
                        Some(r) => (r, macho_arm64_is_instruction_reloc(r_type)),
                        None => continue,
                    }
                }
                _ => continue,
            };

            // COFF and Mach-O data relocations use implicit addends stored in
            // section data. Mach-O instruction relocations (ADRP, LDR, BL) encode
            // the immediate in instruction bits — don't read raw bytes as addend.
            let addend = if is_macho_instruction {
                0
            } else if reloc.has_implicit_addend() {
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
fn coff_to_elf_r_type(typ: u16) -> Option<u32> {
    Some(match typ {
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
        _ => return None,
    })
}

/// Map Mach-O arm64 relocation types to ELF aarch64 equivalents.
fn macho_arm64_to_elf_r_type(r_type: u8, r_length: u8) -> Option<u32> {
    Some(match r_type {
        macho::ARM64_RELOC_UNSIGNED if r_length == 3 => elf::R_AARCH64_ABS64,
        macho::ARM64_RELOC_BRANCH26 => elf::R_AARCH64_CALL26,
        macho::ARM64_RELOC_PAGE21 => elf::R_AARCH64_ADR_PREL_PG_HI21,
        macho::ARM64_RELOC_PAGEOFF12 => elf::R_AARCH64_ADD_ABS_LO12_NC,
        macho::ARM64_RELOC_GOT_LOAD_PAGE21 => elf::R_AARCH64_ADR_GOT_PAGE,
        macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12 => elf::R_AARCH64_LD64_GOT_LO12_NC,
        _ => return None,
    })
}

/// Returns true if this Mach-O arm64 relocation type encodes its value inside
/// an instruction (as opposed to a raw data pointer).
fn macho_arm64_is_instruction_reloc(r_type: u8) -> bool {
    !matches!(r_type, macho::ARM64_RELOC_UNSIGNED | macho::ARM64_RELOC_SUBTRACTOR)
}

pub(crate) fn is_archive(data: &[u8]) -> bool {
    data.starts_with(b"!<arch>\n") || data.starts_with(b"!<thin>\n")
}

pub(crate) fn extract_archive(name: &str, data: &[u8], out: &mut Vec<(String, Vec<u8>)>) -> Result<(), LinkError> {
    let archive = object::read::archive::ArchiveFile::parse(data)
        .map_err(|e| LinkError::Parse { file: name.to_string(), message: e.to_string() })?;
    for member in archive.members() {
        let member = member
            .map_err(|e| LinkError::Parse { file: name.to_string(), message: e.to_string() })?;
        let member_name = String::from_utf8_lossy(member.name()).to_string();
        if !member_name.ends_with(".o") {
            continue;
        }
        let member_data = member.data(data)
            .map_err(|e| LinkError::Parse { file: format!("{name}({member_name})"), message: e.to_string() })?;
        out.push((format!("{name}({member_name})"), member_data.to_vec()));
    }
    Ok(())
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

/// Quickly scan an object file for its defined and referenced (undefined) symbols.
/// Used for selective archive member extraction.
pub(crate) fn scan_symbols(data: &[u8]) -> (HashSet<String>, HashSet<String>) {
    let mut defined = HashSet::new();
    let mut referenced = HashSet::new();

    let obj = match read::File::parse(data) {
        Ok(o) => o,
        Err(_) => return (defined, referenced),
    };

    for sym in obj.symbols() {
        let name = match sym.name() {
            Ok(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };
        if sym.is_undefined() {
            referenced.insert(name);
        } else if sym.is_global() {
            defined.insert(name);
        }
    }

    (defined, referenced)
}

/// Remove unreachable sections (dead code elimination).
/// Roots: entry symbol's section + .init_array/.fini_array sections.
pub(crate) fn gc_sections(state: &mut LinkState, entry: &str) {
    let num_sections = state.sections.len();
    if num_sections == 0 { return; }

    // Resolve symbol name → section index
    let sym_to_section = |name: &str| -> Option<usize> {
        if let Some(def) = state.globals.get(name) {
            if def.section_global_idx != DYNAMIC_SYMBOL_SENTINEL {
                return Some(def.section_global_idx);
            }
        }
        None
    };

    // Build adjacency list: source_section → set of target sections
    let mut edges: Vec<HashSet<usize>> = vec![HashSet::new(); num_sections];
    for reloc in &state.relocs {
        if let Some(target) = sym_to_section(&reloc.symbol_name) {
            edges[reloc.section_global_idx].insert(target);
        }
        // Also check locals
        let obj_idx = state.sections[reloc.section_global_idx].obj_idx;
        if let Some(def) = state.locals.get(&(obj_idx, reloc.symbol_name.clone())) {
            edges[reloc.section_global_idx].insert(def.section_global_idx);
        }
    }

    // Find roots
    let mut reachable = vec![false; num_sections];
    let mut queue = std::collections::VecDeque::new();

    // Entry symbol's section
    if let Some(sec_idx) = sym_to_section(entry) {
        reachable[sec_idx] = true;
        queue.push_back(sec_idx);
    }

    // .init_array / .fini_array sections are always roots
    for (idx, sec) in state.sections.iter().enumerate() {
        if sec.name.starts_with(".init_array") || sec.name.starts_with(".fini_array") {
            if !reachable[idx] {
                reachable[idx] = true;
                queue.push_back(idx);
            }
        }
    }

    // BFS
    while let Some(idx) = queue.pop_front() {
        for &target in &edges[idx] {
            if !reachable[target] {
                reachable[target] = true;
                queue.push_back(target);
            }
        }
    }

    // Build old→new index mapping
    let mut remap = vec![0usize; num_sections];
    let mut new_idx = 0;
    for old_idx in 0..num_sections {
        if reachable[old_idx] {
            remap[old_idx] = new_idx;
            new_idx += 1;
        }
    }

    // Remove dead sections
    let mut new_sections = Vec::new();
    for (idx, sec) in state.sections.drain(..).enumerate() {
        if reachable[idx] {
            new_sections.push(sec);
        }
    }
    state.sections = new_sections;

    // Remap relocs and remove dead ones
    state.relocs.retain(|r| reachable[r.section_global_idx]);
    for reloc in &mut state.relocs {
        reloc.section_global_idx = remap[reloc.section_global_idx];
    }

    // Remap globals
    state.globals.retain(|_, def| {
        def.section_global_idx == DYNAMIC_SYMBOL_SENTINEL || reachable[def.section_global_idx]
    });
    for def in state.globals.values_mut() {
        if def.section_global_idx != DYNAMIC_SYMBOL_SENTINEL {
            def.section_global_idx = remap[def.section_global_idx];
        }
    }

    // Remap locals
    state.locals.retain(|_, def| reachable[def.section_global_idx]);
    for def in state.locals.values_mut() {
        def.section_global_idx = remap[def.section_global_idx];
    }

    // Remap tls_sections
    state.tls_sections.retain(|&idx| reachable[idx]);
    for idx in &mut state.tls_sections {
        *idx = remap[*idx];
    }
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
            nobits: false,
            merge: false,
            strings: false,
            entsize: 0,
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
            nobits: false,
            merge: false,
            strings: false,
            entsize: 0,
        });
        state.globals.insert(
            SHIM_NO_ALLOC_UNSTABLE.to_string(),
            SymbolDef { section_global_idx: sec_idx, value: 0 },
        );
    }
}

/// Merge SHF_MERGE|SHF_STRINGS sections with entsize=1 (null-terminated strings).
/// Deduplicates identical strings across input sections with the same name.
/// Updates symbol values and relocation addends to reflect merged offsets.
pub(crate) fn merge_string_sections(state: &mut LinkState) {
    // Find sections eligible for string merging
    let merge_indices: Vec<usize> = (0..state.sections.len())
        .filter(|&i| state.sections[i].merge && state.sections[i].strings && state.sections[i].entsize == 1)
        .collect();

    if merge_indices.is_empty() { return; }

    // Group by section name
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for &idx in &merge_indices {
        groups.entry(state.sections[idx].name.clone()).or_default().push(idx);
    }

    // For each group with more than one section, merge them
    // offset_remap: (old_section_idx, old_offset) → new_offset in merged section
    let mut offset_remap: HashMap<(usize, u64), u64> = HashMap::new();
    let mut replaced_sections: HashSet<usize> = HashSet::new();

    for (_, group) in &groups {
        if group.len() < 2 { continue; }

        // Parse strings from all sections and intern them
        let mut string_map: HashMap<Vec<u8>, u64> = HashMap::new(); // string → offset in merged data
        let mut merged_data = Vec::new();

        for &sec_idx in group {
            let data = &state.sections[sec_idx].data;
            let mut pos = 0;
            while pos < data.len() {
                // Find null terminator
                let end = data[pos..].iter().position(|&b| b == 0)
                    .map(|p| pos + p + 1)
                    .unwrap_or(data.len());
                let string = &data[pos..end];

                let new_offset = if let Some(&existing_off) = string_map.get(string) {
                    existing_off
                } else {
                    let off = merged_data.len() as u64;
                    string_map.insert(string.to_vec(), off);
                    merged_data.extend_from_slice(string);
                    off
                };

                offset_remap.insert((sec_idx, pos as u64), new_offset);
                pos = end;
            }
        }

        // Replace the first section with merged data; mark the rest for removal
        let keep_idx = group[0];
        state.sections[keep_idx].data = merged_data;
        state.sections[keep_idx].size = state.sections[keep_idx].data.len() as u64;

        for &sec_idx in &group[1..] {
            replaced_sections.insert(sec_idx);
        }
    }

    if replaced_sections.is_empty() && offset_remap.is_empty() { return; }

    let merge_set: HashSet<usize> = merge_indices.iter().copied().collect();

    // Save old symbol state before remapping (needed for relocation addend updates)
    let old_globals: HashMap<String, SymbolDef> = state.globals.clone();
    let old_locals: HashMap<(usize, String), SymbolDef> = state.locals.clone();

    // Update symbol definitions pointing into merged sections
    for def in state.globals.values_mut() {
        if let Some(&new_off) = offset_remap.get(&(def.section_global_idx, def.value)) {
            let old_name = &state.sections[def.section_global_idx].name;
            if let Some(group) = groups.get(old_name) {
                def.section_global_idx = group[0];
            }
            def.value = new_off;
        }
    }
    for def in state.locals.values_mut() {
        if let Some(&new_off) = offset_remap.get(&(def.section_global_idx, def.value)) {
            let old_name = &state.sections[def.section_global_idx].name;
            if let Some(group) = groups.get(old_name) {
                def.section_global_idx = group[0];
            }
            def.value = new_off;
        }
    }

    // Update relocation addends: when a relocation targets a symbol in a merged
    // section, the addend may encode an offset into that section that needs remapping.
    for reloc in &mut state.relocs {
        let obj_idx = state.sections[reloc.section_global_idx].obj_idx;
        let old_def = old_globals.get(&reloc.symbol_name)
            .or_else(|| old_locals.get(&(obj_idx, reloc.symbol_name.clone())));
        if let Some(old_def) = old_def {
            if !merge_set.contains(&old_def.section_global_idx) { continue; }
            let old_offset = old_def.value + reloc.addend as u64;
            if let Some(&new_offset) = offset_remap.get(&(old_def.section_global_idx, old_offset)) {
                let new_def = state.globals.get(&reloc.symbol_name)
                    .or_else(|| state.locals.get(&(obj_idx, reloc.symbol_name.clone())));
                if let Some(new_def) = new_def {
                    reloc.addend = new_offset as i64 - new_def.value as i64;
                }
            }
        }
    }

    // Remove replaced sections and remap all indices
    if !replaced_sections.is_empty() {
        let mut index_map: Vec<Option<usize>> = Vec::with_capacity(state.sections.len());
        let mut new_idx = 0;
        for i in 0..state.sections.len() {
            if replaced_sections.contains(&i) {
                index_map.push(None);
            } else {
                index_map.push(Some(new_idx));
                new_idx += 1;
            }
        }

        // Remap section indices in symbols
        for def in state.globals.values_mut() {
            if def.section_global_idx != DYNAMIC_SYMBOL_SENTINEL {
                if let Some(new) = index_map.get(def.section_global_idx).and_then(|x| *x) {
                    def.section_global_idx = new;
                }
            }
        }
        for def in state.locals.values_mut() {
            if let Some(new) = index_map.get(def.section_global_idx).and_then(|x| *x) {
                def.section_global_idx = new;
            }
        }

        // Remap section indices in relocations, drop relocs targeting removed sections
        state.relocs.retain(|r| {
            index_map.get(r.section_global_idx).and_then(|x| *x).is_some()
        });
        for reloc in &mut state.relocs {
            if let Some(new) = index_map.get(reloc.section_global_idx).and_then(|x| *x) {
                reloc.section_global_idx = new;
            }
        }

        // Remap TLS section indices
        state.tls_sections = state.tls_sections.iter()
            .filter_map(|&idx| index_map.get(idx).and_then(|x| *x))
            .collect();

        // Remove the sections
        let mut i = 0;
        state.sections.retain(|_| {
            let keep = !replaced_sections.contains(&i);
            i += 1;
            keep
        });
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

//! toyos-ld: Minimal linker for ToyOS.
//!
//! Reads ELF and COFF object files. Produces PIE ELF, static ELF, or PE32+.
//! Supports .o object files and .rlib/.a archives (ar format).

use bytemuck::{bytes_of, Pod, Zeroable};
use object::{elf, pe};
use object::read::elf::ElfFile64;
use object::read::{self, Object, ObjectSection, ObjectSymbol};
use object::RelocationFlags;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::fs;

const BASE_VADDR: u64 = 0;
const PAGE_SIZE: u64 = 0x1000;

// ── ELF binary format structs ────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Elf64Ehdr {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Elf64Shdr {
    sh_name: u32,
    sh_type: u32,
    sh_flags: u64,
    sh_addr: u64,
    sh_offset: u64,
    sh_size: u64,
    sh_link: u32,
    sh_info: u32,
    sh_addralign: u64,
    sh_entsize: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Elf64Rela {
    r_offset: u64,
    r_info: u64,
    r_addend: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Elf64Sym {
    st_name: u32,
    st_info: u8,
    st_other: u8,
    st_shndx: u16,
    st_value: u64,
    st_size: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Elf64Dyn {
    d_tag: i64,
    d_val: u64,
}

// ── PE binary format structs ─────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PeDosHeader {
    e_magic: u16,
    _pad1: [u8; 32],
    _pad2: [u8; 26],
    e_lfanew: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PeCoffHeader {
    machine: u16,
    number_of_sections: u16,
    time_date_stamp: u32,
    pointer_to_symbol_table: u32,
    number_of_symbols: u32,
    size_of_optional_header: u16,
    characteristics: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Pe32PlusOptHeader {
    magic: u16,
    major_linker_version: u8,
    minor_linker_version: u8,
    size_of_code: u32,
    size_of_initialized_data: u32,
    size_of_uninitialized_data: u32,
    address_of_entry_point: u32,
    base_of_code: u32,
    image_base: u64,
    section_alignment: u32,
    file_alignment: u32,
    major_os_version: u16,
    minor_os_version: u16,
    major_image_version: u16,
    minor_image_version: u16,
    major_subsystem_version: u16,
    minor_subsystem_version: u16,
    win32_version_value: u32,
    size_of_image: u32,
    size_of_headers: u32,
    checksum: u32,
    subsystem: u16,
    dll_characteristics: u16,
    size_of_stack_reserve: u64,
    size_of_stack_commit: u64,
    size_of_heap_reserve: u64,
    size_of_heap_commit: u64,
    loader_flags: u32,
    number_of_rva_and_sizes: u32,
    data_directories: [PeDataDirectory; 16],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PeDataDirectory {
    virtual_address: u32,
    size: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PeSectionHeader {
    name: [u8; 8],
    virtual_size: u32,
    virtual_address: u32,
    size_of_raw_data: u32,
    pointer_to_raw_data: u32,
    pointer_to_relocations: u32,
    pointer_to_line_numbers: u32,
    number_of_relocations: u16,
    number_of_line_numbers: u16,
    characteristics: u32,
}

fn elf_ident() -> [u8; 16] {
    let mut ident = [0u8; 16];
    ident[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    ident[4] = 2; // ELFCLASS64
    ident[5] = 1; // ELFDATA2LSB
    ident[6] = 1; // EV_CURRENT
    ident
}

fn write_struct<T: Pod>(buf: &mut [u8], offset: usize, val: &T) {
    let bytes = bytes_of(val);
    buf[offset..offset + bytes.len()].copy_from_slice(bytes);
}

// ── Public API ──────────────────────────────────────────────────────────

/// Link object files and produce a PE32+ executable for UEFI.
/// Input is ELF .o files; output is PE/COFF.
/// `entry` is the entry point symbol name (e.g. "efi_main").
/// `subsystem` is the PE subsystem (10 = EFI_APPLICATION).
/// Returns the raw PE bytes on success, or a list of undefined symbols on failure.
pub fn link_pe(
    objects: &[(String, Vec<u8>)],
    entry: &str,
    subsystem: u16,
) -> Result<Vec<u8>, Vec<String>> {
    let mut state = collect(objects);
    synthesize_alloc_shims(&mut state);
    let pe_layout = layout_pe(&mut state);
    let base_relocs = apply_relocs_pe(&mut state, &pe_layout)?;
    Ok(emit_pe_bytes(&state, &pe_layout, entry, subsystem, &base_relocs))
}

/// Link object files and produce a static ELF executable (ET_EXEC).
/// Used for bare-metal targets like x86_64-unknown-none (kernel).
/// `base_addr` sets the load address (e.g. 0xFFFF800000000000 for kernel code model).
/// Returns the raw ELF bytes on success, or a list of undefined symbols on failure.
pub fn link_static(
    objects: &[(String, Vec<u8>)],
    entry: &str,
    base_addr: u64,
) -> Result<Vec<u8>, Vec<String>> {
    let mut state = collect(objects);
    synthesize_alloc_shims(&mut state);
    let layout = layout_elf(&mut state, base_addr, None);
    let empty_dyn_got = HashMap::new();
    let params = ElfRelocParams {
        got: &layout.got,
        tls_start: layout.tls_start,
        tls_memsz: layout.tls_memsz,
        plt: None,
        dyn_got: &empty_dyn_got,
        record_relatives: false,
        allow_undefined: false,
    };
    apply_relocs(&mut state, &params)?;
    Ok(emit_static_bytes(&state, &layout, entry))
}

/// Link object files and produce a PIE ELF executable.
/// Returns the raw ELF bytes on success, or a list of undefined symbols on failure.
pub fn link(objects: &[(String, Vec<u8>)], entry: &str) -> Result<Vec<u8>, Vec<String>> {
    let mut state = collect(objects);
    synthesize_alloc_shims(&mut state);
    let layout = layout_elf(&mut state, BASE_VADDR, Some(entry));
    let params = ElfRelocParams {
        got: &layout.got,
        tls_start: layout.tls_start,
        tls_memsz: layout.tls_memsz,
        plt: Some(&layout.plt),
        dyn_got: &layout.dyn_got,
        record_relatives: true,
        allow_undefined: false,
    };
    let reloc_output = apply_relocs(&mut state, &params)?;
    Ok(emit_bytes(&state, &layout, &reloc_output, entry))
}

/// Resolve library names (-l flags) against search paths (-L flags),
/// reading and extracting archives. Returns (name, data) pairs.
pub fn resolve_libs(
    inputs: &[PathBuf],
    lib_paths: &[PathBuf],
    libs: &[String],
) -> Vec<(String, Vec<u8>)> {
    let mut objects = Vec::new();

    for path in inputs {
        let data = fs::read(path)
            .unwrap_or_else(|e| panic!("toyos-ld: cannot read {}: {e}", path.display()));
        if is_archive(&data) {
            extract_archive(&path.display().to_string(), &data, &mut objects);
        } else {
            objects.push((path.display().to_string(), data));
        }
    }

    for lib in libs {
        let (name, data) = find_lib(lib, lib_paths)
            .unwrap_or_else(|| panic!("toyos-ld: cannot find -l{lib}"));
        if is_archive(&data) {
            extract_archive(&name, &data, &mut objects);
        } else {
            objects.push((name, data));
        }
    }

    objects
}

// ── Input file reading ───────────────────────────────────────────────────

fn is_archive(data: &[u8]) -> bool {
    data.starts_with(b"!<arch>\n") || data.starts_with(b"!<thin>\n")
}

fn extract_archive(name: &str, data: &[u8], out: &mut Vec<(String, Vec<u8>)>) {
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

fn find_lib(name: &str, paths: &[PathBuf]) -> Option<(String, Vec<u8>)> {
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

// ── Shared library symbol extraction ─────────────────────────────────────

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

// ── Symbol + Section collection ──────────────────────────────────────────

#[derive(Clone)]
struct InputSection {
    obj_idx: usize,
    name: String,
    data: Vec<u8>,
    align: u64,
    size: u64,
    vaddr: u64,
}

#[derive(Clone)]
struct InputReloc {
    section_global_idx: usize,
    offset: u64,
    r_type: u32,
    symbol_name: String,
    addend: i64,
}

#[derive(Clone, Copy)]
struct SymbolDef {
    section_global_idx: usize,
    value: u64,
}

/// Sentinel: symbols provided by .so inputs have this section index.
const DYNAMIC_SYMBOL_SENTINEL: usize = usize::MAX;

struct LinkState {
    sections: Vec<InputSection>,
    relocs: Vec<InputReloc>,
    globals: HashMap<String, SymbolDef>,
    locals: HashMap<(usize, String), SymbolDef>,
    tls_sections: Vec<usize>,
    /// Non-loadable metadata sections (e.g. .rustc) preserved in shared library output.
    metadata: Vec<(String, Vec<u8>)>,
    /// Symbol names provided by shared library (.so) inputs.
    dynamic_imports: HashSet<String>,
    /// Bare filenames of .so inputs (for DT_NEEDED entries).
    dynamic_libs: Vec<String>,
}

fn collect(objects: &[(String, Vec<u8>)]) -> LinkState {
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

        state.sections.push(InputSection {
            obj_idx,
            name: sec_name.to_string(),
            data: sec_data,
            align: section.align().max(1),
            size: section.size(),
            vaddr: 0,
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
            let sym_name = match reloc.target() {
                read::RelocationTarget::Symbol(sym_idx) => {
                    match obj.symbol_by_index(sym_idx) {
                        Ok(s) => {
                            let name = s.name().unwrap_or("");
                            // Section symbols need unique synthetic names because
                            // COFF objects can have multiple sections with the same
                            // name (e.g. many `.rdata` COMDAT sections). ELF section
                            // symbols have empty names; COFF section symbols have
                            // the section name. Both cases use the section index to
                            // create a unique key for correct resolution.
                            let is_section_sym = name.is_empty()
                                || s.kind() == read::SymbolKind::Section;
                            if is_section_sym {
                                if let read::SymbolSection::Section(si) = s.section() {
                                    if let Some(&gsec) = sec_map.get(&(obj_idx, si)) {
                                        let syn =
                                            format!("__section_sym_{}_{}", obj_idx, gsec);
                                        state
                                            .locals
                                            .entry((obj_idx, syn.clone()))
                                            .or_insert(SymbolDef {
                                                section_global_idx: gsec,
                                                value: s.address(),
                                            });
                                        syn
                                    } else {
                                        continue;
                                    }
                                } else {
                                    continue;
                                }
                            } else {
                                name.to_string()
                            }
                        }
                        Err(_) => continue,
                    }
                }
                _ => continue,
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

fn synthesize_alloc_shims(state: &mut LinkState) {
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
        });
        state.globals.insert(
            SHIM_NO_ALLOC_UNSTABLE.to_string(),
            SymbolDef { section_global_idx: sec_idx, value: 0 },
        );
    }
}

// ── Layout ───────────────────────────────────────────────────────────────

fn is_tls_section(name: &str) -> bool {
    name.starts_with(".tdata") || name.starts_with(".tbss")
}

fn is_rx_section(name: &str) -> bool {
    name.starts_with(".text")
        || name.starts_with(".rodata")
        || name.starts_with(".rdata")  // COFF naming for read-only data
        || name.starts_with(".eh_frame")
        || name == ".gcc_except_table"
        || name.starts_with(".data.rel.ro")
        || name.starts_with(".xdata")  // COFF unwind info (read-only)
        || name.starts_with(".pdata")  // COFF exception directory (read-only)
}

struct ElfLayout {
    base_addr: u64,
    rx_start: u64,
    rx_end: u64,
    rw_start: u64,
    rw_end: u64,
    tls_start: u64,
    tls_filesz: u64,
    tls_memsz: u64,
    got: HashMap<String, u64>,
    plt: HashMap<String, u64>,
    plt_data: Vec<u8>,
    plt_vaddr: u64,
    dyn_got: HashMap<String, u64>,
}

fn layout_elf(state: &mut LinkState, base_addr: u64, entry_name: Option<&str>) -> ElfLayout {
    let headers_size = 0x1000u64;

    let mut rx_sections = Vec::new();
    let mut rw_sections = Vec::new();
    let mut tls_sections = Vec::new();

    for (idx, sec) in state.sections.iter().enumerate() {
        if state.tls_sections.contains(&idx) {
            tls_sections.push(idx);
        } else if is_tls_section(&sec.name) {
            tls_sections.push(idx);
            state.tls_sections.push(idx);
        } else if is_rx_section(&sec.name) {
            rx_sections.push(idx);
        } else {
            rw_sections.push(idx);
        }
    }

    let mut cursor = base_addr + headers_size;

    let rx_start = cursor;
    for &idx in &rx_sections {
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
    for &idx in &rw_sections {
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
    for &idx in &tls_sections {
        let sec = &mut state.sections[idx];
        tls_cursor = align_up(tls_cursor, sec.align);
        sec.vaddr = tls_cursor;
        tls_cursor += sec.size;
    }
    let tls_filesz = tls_sections
        .iter()
        .filter(|&&idx| !state.sections[idx].name.starts_with(".tbss"))
        .map(|&idx| state.sections[idx].size)
        .sum::<u64>();
    let tls_memsz = if tls_sections.is_empty() { 0 } else { tls_cursor - tls_start };

    ElfLayout {
        base_addr, rx_start, rx_end, rw_start, rw_end,
        tls_start, tls_filesz, tls_memsz,
        got, plt, plt_data, plt_vaddr, dyn_got,
    }
}

fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

/// Collect unique symbols in insertion order (deduplicating with a HashSet).
fn collect_unique_symbols<'a>(
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

// ── Relocation ───────────────────────────────────────────────────────────

struct RelocOutput {
    relatives: Vec<(u64, i64)>,
    /// Dynamic GOT entries needing GLOB_DAT relocations: (GOT slot vaddr, symbol name).
    glob_dats: Vec<(u64, String)>,
}

/// Resolve a symbol to its virtual address.
/// `plt` provides PLT stubs for dynamic symbols (PIE mode). Pass `None` for
/// static/PE modes where dynamic symbols are unsupported.
fn resolve_symbol(
    state: &LinkState,
    name: &str,
    from_sec: usize,
    plt: Option<&HashMap<String, u64>>,
) -> Option<u64> {
    if let Some(def) = state.globals.get(name) {
        if def.section_global_idx == DYNAMIC_SYMBOL_SENTINEL {
            return plt.and_then(|p| p.get(name).copied());
        }
        return Some(state.sections[def.section_global_idx].vaddr + def.value);
    }
    let obj_idx = state.sections[from_sec].obj_idx;
    if let Some(def) = state.locals.get(&(obj_idx, name.to_string())) {
        return Some(state.sections[def.section_global_idx].vaddr + def.value);
    }
    None
}

/// x86-64 Variant II: TP points to end of TLS block.
/// TPOFF = symbol_vaddr - (tls_start + tls_memsz)
fn tpoff(sym_addr: u64, tls_start: u64, tls_memsz: u64) -> i64 {
    sym_addr as i64 - (tls_start as i64 + tls_memsz as i64)
}

fn write_bytes(state: &mut LinkState, sec_idx: usize, offset: u64, bytes: &[u8]) {
    let sec = &mut state.sections[sec_idx];
    let off = offset as usize;
    sec.data[off..off + bytes.len()].copy_from_slice(bytes);
}

fn write_u64(state: &mut LinkState, sec_idx: usize, offset: u64, value: u64) {
    write_bytes(state, sec_idx, offset, &value.to_le_bytes());
}

fn write_i32(state: &mut LinkState, sec_idx: usize, offset: u64, value: i32) {
    write_bytes(state, sec_idx, offset, &value.to_le_bytes());
}

fn write_u32(state: &mut LinkState, sec_idx: usize, offset: u64, value: u32) {
    write_bytes(state, sec_idx, offset, &value.to_le_bytes());
}

/// Detect whether a TLS GD/LD relocation uses the 16-byte padded or 12-byte
/// unpadded instruction sequence by examining the byte before the leaq.
/// Padded: `data16; leaq ...` → byte at offset-4 is 0x66
/// Unpadded: `leaq ...`       → byte at offset-3 is 0x48 (REX.W)
fn is_padded_tls_sequence(sec_data: &[u8], reloc_offset: u64) -> bool {
    let off = reloc_offset as usize;
    off >= 4 && sec_data[off - 4] == 0x66
}

/// Parameters for ELF relocation application (shared between PIE and static modes).
struct ElfRelocParams<'a> {
    got: &'a HashMap<String, u64>,
    tls_start: u64,
    tls_memsz: u64,
    plt: Option<&'a HashMap<String, u64>>,
    dyn_got: &'a HashMap<String, u64>,
    /// PIE mode: record R_X86_64_RELATIVE entries for runtime relocation.
    /// Static mode: addresses are fixed at link time, no RELATIVE needed.
    record_relatives: bool,
    allow_undefined: bool,
}

fn apply_relocs(
    state: &mut LinkState,
    params: &ElfRelocParams,
) -> Result<RelocOutput, Vec<String>> {
    let mut relatives = Vec::new();
    let mut undefined = HashSet::new();

    let relocs = std::mem::take(&mut state.relocs);

    // Pass 1: TLS GD/LD/DTPOFF relaxations. These rewrite instruction bytes
    // and overwrite the companion `call __tls_get_addr` instruction, so we
    // track which (section, offset) ranges were relaxed.
    let mut relaxed_calls: HashSet<(usize, u64)> = HashSet::new();

    for reloc in &relocs {
        match reloc.r_type {
            elf::R_X86_64_TLSGD => {
                let sym_addr = resolve_symbol(state, &reloc.symbol_name, reloc.section_global_idx, params.plt)
                    .unwrap_or_else(|| panic!("toyos-ld: undefined TLS symbol: {}", reloc.symbol_name));
                let padded = is_padded_tls_sequence(
                    &state.sections[reloc.section_global_idx].data,
                    reloc.offset,
                );
                if padded {
                    // GD → LE (16-byte padded): `data16; leaq; data16*2; rex64; call`
                    // → `mov %fs:0,%rax; lea tpoff(%rax),%rax`
                    #[rustfmt::skip]
                    let inst: [u8; 16] = [
                        0x64, 0x48, 0x8b, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00, // mov %fs:0,%rax
                        0x48, 0x8d, 0x80, 0x00, 0x00, 0x00, 0x00,             // lea 0(%rax),%rax
                    ];
                    write_bytes(state, reloc.section_global_idx, reloc.offset - 4, &inst);
                    write_i32(state, reloc.section_global_idx, reloc.offset + 8,
                        tpoff(sym_addr, params.tls_start, params.tls_memsz) as i32);
                    relaxed_calls.insert((reloc.section_global_idx, reloc.offset + 8));
                } else {
                    panic!("toyos-ld: unpadded 12-byte TLSGD sequence not supported");
                }
            }
            elf::R_X86_64_TLSLD => {
                let padded = is_padded_tls_sequence(
                    &state.sections[reloc.section_global_idx].data,
                    reloc.offset,
                );
                if padded {
                    // LD → LE (16-byte padded)
                    #[rustfmt::skip]
                    let inst: [u8; 16] = [
                        0x66, 0x66, 0x66,                                           // 3x data16
                        0x64, 0x48, 0x8b, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00,     // mov %fs:0,%rax
                        0x0f, 0x1f, 0x40, 0x00,                                     // nopl 0(%rax)
                    ];
                    write_bytes(state, reloc.section_global_idx, reloc.offset - 4, &inst);
                    relaxed_calls.insert((reloc.section_global_idx, reloc.offset + 8));
                } else {
                    // LD → LE (12-byte unpadded)
                    #[rustfmt::skip]
                    let inst: [u8; 12] = [
                        0x66, 0x66, 0x66,                                           // 3x data16
                        0x64, 0x48, 0x8b, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00,     // mov %fs:0,%rax
                    ];
                    write_bytes(state, reloc.section_global_idx, reloc.offset - 3, &inst);
                    relaxed_calls.insert((reloc.section_global_idx, reloc.offset + 5));
                }
            }
            elf::R_X86_64_DTPOFF32 => {
                let sym_addr = resolve_symbol(state, &reloc.symbol_name, reloc.section_global_idx, params.plt)
                    .unwrap_or_else(|| panic!("toyos-ld: undefined TLS symbol: {}", reloc.symbol_name));
                write_i32(state, reloc.section_global_idx, reloc.offset,
                    (tpoff(sym_addr, params.tls_start, params.tls_memsz) + reloc.addend) as i32);
            }
            _ => {}
        }
    }

    // Pass 2: all other relocations
    for reloc in &relocs {
        match reloc.r_type {
            elf::R_X86_64_TLSGD | elf::R_X86_64_TLSLD | elf::R_X86_64_DTPOFF32 => continue,
            _ => {}
        }
        if relaxed_calls.contains(&(reloc.section_global_idx, reloc.offset)) {
            continue;
        }

        let sec = &state.sections[reloc.section_global_idx];
        let reloc_vaddr = sec.vaddr + reloc.offset;

        let sym_addr = match resolve_symbol(state, &reloc.symbol_name, reloc.section_global_idx, params.plt) {
            Some(a) => a,
            None => {
                if reloc.symbol_name.is_empty() {
                    0
                } else {
                    undefined.insert(reloc.symbol_name.clone());
                    continue;
                }
            }
        };

        match reloc.r_type {
            elf::R_X86_64_64 => {
                let value = (sym_addr as i64 + reloc.addend) as u64;
                write_u64(state, reloc.section_global_idx, reloc.offset, value);
                if params.record_relatives {
                    relatives.push((reloc_vaddr, sym_addr as i64 + reloc.addend));
                }
            }
            elf::R_X86_64_PC32 | elf::R_X86_64_PLT32 => {
                let value = sym_addr as i64 + reloc.addend - reloc_vaddr as i64;
                write_i32(state, reloc.section_global_idx, reloc.offset, value as i32);
            }
            elf::R_X86_64_32 => {
                let value = (sym_addr as i64 + reloc.addend) as u32;
                write_u32(state, reloc.section_global_idx, reloc.offset, value);
            }
            elf::R_X86_64_32S => {
                let value = (sym_addr as i64 + reloc.addend) as i32;
                write_i32(state, reloc.section_global_idx, reloc.offset, value);
            }
            elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX
            | elf::R_X86_64_REX_GOTPCRELX => {
                let got_slot = params.got[&reloc.symbol_name];
                let value = got_slot as i64 + reloc.addend - reloc_vaddr as i64;
                write_i32(state, reloc.section_global_idx, reloc.offset, value as i32);
            }
            elf::R_X86_64_TPOFF32 => {
                let tp = tpoff(sym_addr, params.tls_start, params.tls_memsz);
                write_i32(
                    state,
                    reloc.section_global_idx,
                    reloc.offset,
                    (tp + reloc.addend) as i32,
                );
            }
            elf::R_X86_64_GOTTPOFF => {
                let got_slot = params.got[&reloc.symbol_name];
                let value = got_slot as i64 + reloc.addend - reloc_vaddr as i64;
                write_i32(state, reloc.section_global_idx, reloc.offset, value as i32);
            }
            other => panic!(
                "toyos-ld: unsupported relocation type {other} for symbol {}",
                reloc.symbol_name,
            ),
        }
    }

    // Fill GOT entries (PIE mode records as RELATIVE; static mode handles in emit)
    if params.record_relatives {
        let gottpoff_syms: HashSet<String> = relocs
            .iter()
            .filter(|r| r.r_type == elf::R_X86_64_GOTTPOFF)
            .map(|r| r.symbol_name.clone())
            .collect();

        for (sym_name, &got_vaddr) in params.got {
            let sym_addr = resolve_symbol(state, sym_name, 0, params.plt)
                .unwrap_or_else(|| panic!("toyos-ld: undefined GOT symbol: {sym_name}"));
            if gottpoff_syms.contains(sym_name) {
                let tp = tpoff(sym_addr, params.tls_start, params.tls_memsz);
                relatives.push((got_vaddr, tp));
            } else {
                relatives.push((got_vaddr, sym_addr as i64));
            }
        }
    }

    // Collect dynamic GOT entries as GLOB_DAT relocations (resolved at load time)
    let mut glob_dats = Vec::new();
    for (sym_name, &got_vaddr) in params.dyn_got {
        glob_dats.push((got_vaddr, sym_name.clone()));
    }

    if !params.allow_undefined && !undefined.is_empty() {
        let mut syms: Vec<String> = undefined.into_iter().collect();
        syms.sort();
        return Err(syms);
    }

    Ok(RelocOutput { relatives, glob_dats })
}

// ── ELF output ───────────────────────────────────────────────────────────

use std::mem::size_of;

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

fn copy_to_buf(buf: &mut [u8], offset: u64, data: &[u8]) {
    let off = offset as usize;
    buf[off..off + data.len()].copy_from_slice(data);
}

fn copy_sections_to_buf(buf: &mut [u8], sections: &[InputSection], base_vaddr: u64) {
    for sec in sections {
        if sec.vaddr == 0 || sec.data.is_empty() { continue; }
        let file_off = (sec.vaddr - base_vaddr) as usize;
        buf[file_off..file_off + sec.data.len()].copy_from_slice(&sec.data);
    }
}

fn write_rela_entries(
    buf: &mut [u8],
    mut cursor: usize,
    relatives: &[(u64, i64)],
    glob_dats: &[(u64, String)],
    sym_indices: &HashMap<String, u32>,
) {
    for &(offset, addend) in relatives {
        write_struct(buf, cursor, &Elf64Rela {
            r_offset: offset,
            r_info: elf::R_X86_64_RELATIVE as u64,
            r_addend: addend,
        });
        cursor += size_of::<Elf64Rela>();
    }
    for (got_vaddr, sym_name) in glob_dats {
        let sym_idx = sym_indices.get(sym_name).copied().unwrap_or(0) as u64;
        write_struct(buf, cursor, &Elf64Rela {
            r_offset: *got_vaddr,
            r_info: (sym_idx << 32) | elf::R_X86_64_GLOB_DAT as u64,
            r_addend: 0,
        });
        cursor += size_of::<Elf64Rela>();
    }
}

fn build_import_dynamic(
    needed_offsets: &[u32],
    symtab_vaddr: u64,
    strtab_vaddr: u64,
    strsz: u64,
    rela_vaddr: u64,
    relasz: u64,
) -> Vec<u8> {
    let mut data = Vec::new();
    for &offset in needed_offsets {
        data.extend_from_slice(bytes_of(&Elf64Dyn { d_tag: elf::DT_NEEDED.into(), d_val: offset as u64 }));
    }
    for (tag, val) in [
        (elf::DT_SYMTAB, symtab_vaddr), (elf::DT_STRTAB, strtab_vaddr),
        (elf::DT_STRSZ, strsz), (elf::DT_SYMENT, 24),
        (elf::DT_RELA, rela_vaddr), (elf::DT_RELASZ, relasz), (elf::DT_RELAENT, 24),
    ] {
        data.extend_from_slice(bytes_of(&Elf64Dyn { d_tag: tag.into(), d_val: val }));
    }
    data.extend_from_slice(bytes_of(&Elf64Dyn { d_tag: elf::DT_NULL.into(), d_val: 0 }));
    data
}

fn emit_bytes(
    state: &LinkState,
    layout: &ElfLayout,
    relocs: &RelocOutput,
    entry_name: &str,
) -> Vec<u8> {
    let is_dynamic = !state.dynamic_libs.is_empty();

    let entry = resolve_entry(state, entry_name, Some(&layout.plt));
    let after_rw = layout.rw_end.max(layout.tls_start + layout.tls_memsz);

    // Build dynamic sections for import-style dynsym/dynstr/.dynamic
    let (dynsym_data, dynstr_data, needed_offsets, sym_indices) = if is_dynamic {
        build_import_dynsym(&relocs.glob_dats, &state.dynamic_libs)
    } else {
        (Vec::new(), Vec::new(), Vec::new(), HashMap::new())
    };

    // Layout the dynamic segment (if dynamic) or just .rela.dyn (if static PIE)
    let (dynsym_vaddr, dynstr_vaddr, rela_dyn_vaddr, dynamic_vaddr, dyn_segment_end);
    let rela_dyn_size = (relocs.relatives.len() + relocs.glob_dats.len()) as u64 * size_of::<Elf64Rela>() as u64;
    let dynamic_data;

    if is_dynamic {
        dynsym_vaddr = align_up(after_rw, 8);
        dynstr_vaddr = dynsym_vaddr + dynsym_data.len() as u64;
        rela_dyn_vaddr = align_up(dynstr_vaddr + dynstr_data.len() as u64, 8);
        dynamic_vaddr = align_up(rela_dyn_vaddr + rela_dyn_size, 8);

        dynamic_data = build_import_dynamic(
            &needed_offsets, dynsym_vaddr, dynstr_vaddr,
            dynstr_data.len() as u64, rela_dyn_vaddr, rela_dyn_size,
        );
        dyn_segment_end = align_up(dynamic_vaddr + dynamic_data.len() as u64, PAGE_SIZE);
    } else {
        dynsym_vaddr = 0;
        dynstr_vaddr = 0;
        rela_dyn_vaddr = align_up(after_rw, 8);
        dynamic_vaddr = 0;
        dyn_segment_end = 0;
        dynamic_data = Vec::new();
    }

    let shstrtab_file_offset = if is_dynamic { dyn_segment_end } else { rela_dyn_vaddr + rela_dyn_size };
    let shstrtab = if is_dynamic { build_dynamic_shstrtab() } else { build_shstrtab() };
    let num_shdrs: u16 = if is_dynamic { 8 } else { 5 };
    let shdr_offset = align_up(shstrtab_file_offset + shstrtab.len() as u64, 8);
    let total_size = shdr_offset + num_shdrs as u64 * size_of::<Elf64Shdr>() as u64;

    let mut buf = vec![0u8; total_size as usize];

    // ── ELF header ──
    let mut phdr_count = 2u16;
    if layout.tls_memsz > 0 { phdr_count += 1; }
    if is_dynamic { phdr_count += 2; }
    write_struct(&mut buf, 0, &Elf64Ehdr {
        e_ident: elf_ident(),
        e_type: elf::ET_DYN,
        e_machine: elf::EM_X86_64,
        e_version: 1,
        e_entry: entry,
        e_phoff: 64,
        e_shoff: shdr_offset,
        e_ehsize: 64,
        e_phentsize: size_of::<Elf64Phdr>() as u16,
        e_phnum: phdr_count,
        e_shentsize: size_of::<Elf64Shdr>() as u16,
        e_shnum: num_shdrs,
        e_shstrndx: num_shdrs - 1,
        ..Zeroable::zeroed()
    });

    // ── Program headers ──
    let mut phdrs = vec![
        phdr(elf::PT_LOAD, elf::PF_R | elf::PF_X,
            BASE_VADDR, layout.rx_end - BASE_VADDR, layout.rx_end - BASE_VADDR, PAGE_SIZE),
        phdr(elf::PT_LOAD, elf::PF_R | elf::PF_W,
            layout.rw_start, layout.rw_end - layout.rw_start, layout.rw_end - layout.rw_start, PAGE_SIZE),
    ];
    if layout.tls_memsz > 0 {
        phdrs.push(phdr(elf::PT_TLS, elf::PF_R,
            layout.tls_start, layout.tls_filesz, layout.tls_memsz, 64));
    }
    if is_dynamic {
        phdrs.push(phdr(elf::PT_LOAD, elf::PF_R,
            dynsym_vaddr, dyn_segment_end - dynsym_vaddr, dyn_segment_end - dynsym_vaddr, PAGE_SIZE));
        phdrs.push(phdr(elf::PT_DYNAMIC, elf::PF_R,
            dynamic_vaddr, dynamic_data.len() as u64, dynamic_data.len() as u64, 8));
    }
    for (i, p) in phdrs.iter().enumerate() {
        write_struct(&mut buf, 64 + i * size_of::<Elf64Phdr>(), p);
    }

    // ── Copy section data ──
    copy_sections_to_buf(&mut buf, &state.sections, BASE_VADDR);

    if !layout.plt_data.is_empty() {
        let plt_off = (layout.plt_vaddr - BASE_VADDR) as usize;
        buf[plt_off..plt_off + layout.plt_data.len()].copy_from_slice(&layout.plt_data);
    }

    if is_dynamic {
        copy_to_buf(&mut buf, dynsym_vaddr - BASE_VADDR, &dynsym_data);
        copy_to_buf(&mut buf, dynstr_vaddr - BASE_VADDR, &dynstr_data);
        copy_to_buf(&mut buf, dynamic_vaddr - BASE_VADDR, &dynamic_data);
    }

    // ── Write .rela.dyn ──
    let rela_file_off = if is_dynamic { rela_dyn_vaddr - BASE_VADDR } else { rela_dyn_vaddr };
    write_rela_entries(&mut buf, rela_file_off as usize, &relocs.relatives, &relocs.glob_dats, &sym_indices);

    // ── Write .shstrtab ──
    copy_to_buf(&mut buf, shstrtab_file_offset, &shstrtab);

    // ── Section headers ──
    let sh = shdr_offset as usize;
    if is_dynamic {
        write_struct(&mut buf, sh + 64, &shdr(1, elf::SHT_PROGBITS,
            (elf::SHF_ALLOC | elf::SHF_EXECINSTR) as u64,
            layout.rx_start, layout.rx_start - BASE_VADDR, layout.rx_end - layout.rx_start));
        write_struct(&mut buf, sh + 128, &shdr(7, elf::SHT_PROGBITS,
            (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
            layout.rw_start, layout.rw_start - BASE_VADDR, layout.rw_end - layout.rw_start));
        write_struct(&mut buf, sh + 192, &shdr_full(13, elf::SHT_DYNSYM,
            elf::SHF_ALLOC as u64,
            dynsym_vaddr, dynsym_vaddr - BASE_VADDR, dynsym_data.len() as u64, 4, 1, 8, 24));
        write_struct(&mut buf, sh + 256, &shdr(21, elf::SHT_STRTAB,
            elf::SHF_ALLOC as u64,
            dynstr_vaddr, dynstr_vaddr - BASE_VADDR, dynstr_data.len() as u64));
        write_struct(&mut buf, sh + 320, &shdr_full(29, elf::SHT_RELA,
            elf::SHF_ALLOC as u64,
            rela_dyn_vaddr, rela_dyn_vaddr - BASE_VADDR, rela_dyn_size, 3, 0, 8, 24));
        write_struct(&mut buf, sh + 384, &shdr_full(39, elf::SHT_DYNAMIC,
            (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
            dynamic_vaddr, dynamic_vaddr - BASE_VADDR, dynamic_data.len() as u64, 4, 0, 8, 16));
        write_struct(&mut buf, sh + 448, &shdr(48, elf::SHT_STRTAB,
            0, 0, shstrtab_file_offset, shstrtab.len() as u64));
    } else {
        write_struct(&mut buf, sh + 64, &shdr(1, elf::SHT_PROGBITS,
            (elf::SHF_ALLOC | elf::SHF_EXECINSTR) as u64,
            layout.rx_start, layout.rx_start - BASE_VADDR, layout.rx_end - layout.rx_start));
        write_struct(&mut buf, sh + 128, &shdr(7, elf::SHT_PROGBITS,
            (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
            layout.rw_start, layout.rw_start - BASE_VADDR, layout.rw_end - layout.rw_start));
        write_struct(&mut buf, sh + 192, &shdr_full(13, elf::SHT_RELA,
            elf::SHF_ALLOC as u64,
            0, rela_dyn_vaddr, rela_dyn_size, 0, 0, 8, 24));
        write_struct(&mut buf, sh + 256, &shdr(23, elf::SHT_STRTAB,
            0, 0, shstrtab_file_offset, shstrtab.len() as u64));
    }

    buf
}

fn build_shstrtab() -> Vec<u8> {
    let mut tab = Vec::new();
    tab.push(0);
    tab.extend_from_slice(b".text\0"); // offset 1
    tab.extend_from_slice(b".data\0"); // offset 7
    tab.extend_from_slice(b".rela.dyn\0"); // offset 13
    tab.extend_from_slice(b".shstrtab\0"); // offset 23
    tab
}

/// shstrtab for dynamic executables.
fn build_dynamic_shstrtab() -> Vec<u8> {
    let mut tab = Vec::new();
    tab.push(0);                            // offset 0
    tab.extend_from_slice(b".text\0");      // offset 1
    tab.extend_from_slice(b".data\0");      // offset 7
    tab.extend_from_slice(b".dynsym\0");    // offset 13
    tab.extend_from_slice(b".dynstr\0");    // offset 21
    tab.extend_from_slice(b".rela.dyn\0");  // offset 29
    tab.extend_from_slice(b".dynamic\0");   // offset 39
    tab.extend_from_slice(b".shstrtab\0");  // offset 48
    tab
}

/// Build import .dynsym and .dynstr for a dynamic executable.
fn build_import_dynsym(
    glob_dats: &[(u64, String)],
    dynamic_libs: &[String],
) -> (Vec<u8>, Vec<u8>, Vec<u32>, HashMap<String, u32>) {
    let mut dynstr = vec![0u8]; // leading null
    let mut dynsym = vec![0u8; size_of::<Elf64Sym>()]; // null entry

    let mut needed_offsets = Vec::new();
    for lib in dynamic_libs {
        needed_offsets.push(dynstr.len() as u32);
        dynstr.extend_from_slice(lib.as_bytes());
        dynstr.push(0);
    }

    let mut sym_indices = HashMap::new();
    let mut sym_idx = 1u32;
    for (_, sym_name) in glob_dats {
        if sym_indices.contains_key(sym_name) {
            continue;
        }
        let st_name = dynstr.len() as u32;
        dynstr.extend_from_slice(sym_name.as_bytes());
        dynstr.push(0);

        dynsym.extend_from_slice(bytes_of(&Elf64Sym {
            st_name,
            st_info: (elf::STB_GLOBAL << 4) | elf::STT_NOTYPE,
            ..Zeroable::zeroed()
        }));

        sym_indices.insert(sym_name.clone(), sym_idx);
        sym_idx += 1;
    }

    (dynsym, dynstr, needed_offsets, sym_indices)
}

fn phdr(p_type: u32, p_flags: u32, p_vaddr: u64, p_filesz: u64, p_memsz: u64, p_align: u64) -> Elf64Phdr {
    Elf64Phdr {
        p_type,
        p_flags,
        p_offset: p_vaddr - BASE_VADDR,
        p_vaddr,
        p_paddr: p_vaddr,
        p_filesz,
        p_memsz,
        p_align,
    }
}

fn shdr(sh_name: u32, sh_type: u32, sh_flags: u64, sh_addr: u64, sh_offset: u64, sh_size: u64) -> Elf64Shdr {
    Elf64Shdr { sh_name, sh_type, sh_flags, sh_addr, sh_offset, sh_size, ..Zeroable::zeroed() }
}

fn shdr_full(sh_name: u32, sh_type: u32, sh_flags: u64, sh_addr: u64, sh_offset: u64, sh_size: u64, sh_link: u32, sh_info: u32, sh_addralign: u64, sh_entsize: u64) -> Elf64Shdr {
    Elf64Shdr { sh_name, sh_type, sh_flags, sh_addr, sh_offset, sh_size, sh_link, sh_info, sh_addralign, sh_entsize }
}

// ── PE/COFF output (--pe) ─────────────────────────────────────────────────

const PE_FILE_ALIGNMENT: u32 = 0x200;
const PE_SECTION_ALIGNMENT: u32 = 0x1000;

struct PeLayout {
    /// RVA and file offset/size for each PE section
    text_rva: u32,
    text_file_off: u32,
    text_raw_size: u32,
    text_virt_size: u32,
    data_rva: u32,
    data_file_off: u32,
    data_raw_size: u32,
    data_virt_size: u32,
    has_data: bool,
    reloc_rva: u32,
    reloc_file_off: u32,
    size_of_headers: u32,
    got: HashMap<String, u64>,
}

/// PE section layout: RVAs use PE_SECTION_ALIGNMENT, file uses PE_FILE_ALIGNMENT.
/// All section vaddrs in LinkState are set relative to text_rva.
fn layout_pe(state: &mut LinkState) -> PeLayout {
    // Headers: DOS(64) + PE sig(4) + COFF(20) + OptionalHeader(240) + section headers
    // We'll have 2 or 3 sections: .text, optionally .data, .reloc
    // Determine if we have data sections
    let has_rw = state.sections.iter().any(|s| !is_rx_section(&s.name) && !is_tls_section(&s.name));
    let num_sections: u32 = if has_rw { 3 } else { 2 }; // .text [.data] .reloc
    let headers_end = 64 + 4 + 20 + 240 + num_sections * 40;
    let size_of_headers = pe_align_up(headers_end, PE_FILE_ALIGNMENT);

    let mut rx_sections = Vec::new();
    let mut rw_sections = Vec::new();

    for (idx, sec) in state.sections.iter().enumerate() {
        if is_tls_section(&sec.name) {
            // TLS not supported in UEFI — UEFI is single-threaded
            continue;
        } else if is_rx_section(&sec.name) {
            rx_sections.push(idx);
        } else {
            rw_sections.push(idx);
        }
    }

    // .text section
    let text_rva = PE_SECTION_ALIGNMENT; // first section always at 0x1000
    let mut cursor = text_rva as u64;
    for &idx in &rx_sections {
        let sec = &mut state.sections[idx];
        cursor = align_up(cursor, sec.align);
        sec.vaddr = cursor;
        cursor += sec.size;
    }
    let text_virt_size = (cursor - text_rva as u64) as u32;
    let text_raw_size = pe_align_up(text_virt_size, PE_FILE_ALIGNMENT);
    let text_file_off = size_of_headers;

    // .data section (if any RW sections exist)
    let data_rva = pe_align_up(text_rva + text_virt_size, PE_SECTION_ALIGNMENT);
    let mut data_virt_size = 0u32;
    let has_data = !rw_sections.is_empty();
    if has_data {
        cursor = data_rva as u64;
        for &idx in &rw_sections {
            let sec = &mut state.sections[idx];
            cursor = align_up(cursor, sec.align);
            sec.vaddr = cursor;
            cursor += sec.size;
        }
        data_virt_size = (cursor - data_rva as u64) as u32;
    }

    // GOT entries needed
    let got_symbols = collect_unique_symbols(state.relocs.iter(), |r| {
        matches!(r.r_type,
            elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX
            | elf::R_X86_64_REX_GOTPCRELX)
    });
    if has_data && !got_symbols.is_empty() {
        cursor = align_up(cursor, 8);
    } else if !has_data && !got_symbols.is_empty() {
        // GOT goes in the data section area
        cursor = data_rva as u64;
    }
    let mut got = HashMap::new();
    for sym in &got_symbols {
        got.insert(sym.clone(), cursor);
        cursor += 8;
    }
    if !got_symbols.is_empty() && !has_data {
        data_virt_size = (cursor - data_rva as u64) as u32;
    } else if !got_symbols.is_empty() {
        data_virt_size = (cursor - data_rva as u64) as u32;
    }

    let data_raw_size = if has_data || !got_symbols.is_empty() {
        pe_align_up(data_virt_size, PE_FILE_ALIGNMENT)
    } else { 0 };
    let data_file_off = text_file_off + text_raw_size;

    // .reloc section — will be filled later after we know the fixups
    let reloc_rva = if has_data || !got_symbols.is_empty() {
        pe_align_up(data_rva + data_virt_size, PE_SECTION_ALIGNMENT)
    } else {
        pe_align_up(text_rva + text_virt_size, PE_SECTION_ALIGNMENT)
    };
    let reloc_file_off = data_file_off + data_raw_size;

    PeLayout {
        text_rva,
        text_file_off,
        text_raw_size,
        text_virt_size,
        data_rva,
        data_file_off,
        data_raw_size,
        data_virt_size,
        has_data: has_data || !got_symbols.is_empty(),
        reloc_rva,
        reloc_file_off,
        size_of_headers,
        got,
    }
}

fn pe_align_up(value: u32, alignment: u32) -> u32 {
    (value + alignment - 1) & !(alignment - 1)
}

fn apply_relocs_pe(
    state: &mut LinkState,
    layout: &PeLayout,
) -> Result<Vec<u32>, Vec<String>> {
    let mut undefined = HashSet::new();
    let mut abs_fixups: Vec<u32> = Vec::new(); // RVAs of absolute 64-bit fixups
    let relocs = std::mem::take(&mut state.relocs);

    for reloc in &relocs {
        // Skip TLS relocations — not supported in UEFI
        match reloc.r_type {
            elf::R_X86_64_TLSGD | elf::R_X86_64_TLSLD | elf::R_X86_64_DTPOFF32
            | elf::R_X86_64_TPOFF32 | elf::R_X86_64_GOTTPOFF => continue,
            _ => {}
        }

        let sec = &state.sections[reloc.section_global_idx];
        let reloc_vaddr = sec.vaddr + reloc.offset;

        let sym_addr = match resolve_symbol(state, &reloc.symbol_name, reloc.section_global_idx, None) {
            Some(a) => a,
            None => {
                if reloc.symbol_name.is_empty() { 0 }
                else { undefined.insert(reloc.symbol_name.clone()); continue; }
            }
        };

        match reloc.r_type {
            elf::R_X86_64_64 => {
                let value = (sym_addr as i64 + reloc.addend) as u64;
                write_u64(state, reloc.section_global_idx, reloc.offset, value);
                // Record this as needing a base relocation
                abs_fixups.push(reloc_vaddr as u32);
            }
            elf::R_X86_64_PC32 | elf::R_X86_64_PLT32 => {
                let value = sym_addr as i64 + reloc.addend - reloc_vaddr as i64;
                write_i32(state, reloc.section_global_idx, reloc.offset, value as i32);
                // PC-relative — no base relocation needed
            }
            elf::R_X86_64_32 => {
                let value = (sym_addr as i64 + reloc.addend) as u32;
                write_u32(state, reloc.section_global_idx, reloc.offset, value);
            }
            elf::R_X86_64_32S => {
                let value = (sym_addr as i64 + reloc.addend) as i32;
                write_i32(state, reloc.section_global_idx, reloc.offset, value);
            }
            elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX
            | elf::R_X86_64_REX_GOTPCRELX => {
                let got_slot = layout.got[&reloc.symbol_name];
                let value = got_slot as i64 + reloc.addend - reloc_vaddr as i64;
                write_i32(state, reloc.section_global_idx, reloc.offset, value as i32);
            }
            other => panic!(
                "toyos-ld: unsupported relocation type {other} in PE mode for symbol {}",
                reloc.symbol_name,
            ),
        }
    }

    // Fill GOT entries
    for (_, &got_vaddr) in &layout.got {
        abs_fixups.push(got_vaddr as u32);
    }

    if !undefined.is_empty() {
        let mut syms: Vec<String> = undefined.into_iter().collect();
        syms.sort();
        return Err(syms);
    }

    abs_fixups.sort();
    Ok(abs_fixups)
}

fn build_base_reloc_table(fixups: &[u32]) -> Vec<u8> {
    if fixups.is_empty() {
        return Vec::new();
    }

    let mut result = Vec::new();

    // Group fixups by page (4KB aligned)
    let mut i = 0;
    while i < fixups.len() {
        let page_rva = fixups[i] & !0xFFF;
        let block_start = result.len();

        // Reserve space for header (page_rva + block_size)
        result.extend_from_slice(&page_rva.to_le_bytes());
        result.extend_from_slice(&0u32.to_le_bytes()); // placeholder for block_size

        let mut count = 0u32;
        while i < fixups.len() && (fixups[i] & !0xFFF) == page_rva {
            let offset = fixups[i] & 0xFFF;
            // Type 10 (IMAGE_REL_BASED_DIR64) in upper 4 bits
            let entry: u16 = (10 << 12) | (offset as u16);
            result.extend_from_slice(&entry.to_le_bytes());
            count += 1;
            i += 1;
        }

        // Pad to 4-byte alignment
        if count % 2 != 0 {
            result.extend_from_slice(&0u16.to_le_bytes()); // IMAGE_REL_BASED_ABSOLUTE padding
            count += 1;
        }

        let block_size = 8 + count * 2;
        result[block_start + 4..block_start + 8].copy_from_slice(&block_size.to_le_bytes());
    }

    result
}

fn emit_pe_bytes(
    state: &LinkState,
    layout: &PeLayout,
    entry_name: &str,
    subsystem: u16,
    abs_fixups: &[u32],
) -> Vec<u8> {
    let entry_rva = state
        .globals
        .get(entry_name)
        .map(|def| (state.sections[def.section_global_idx].vaddr + def.value) as u32)
        .unwrap_or_else(|| panic!("toyos-ld: entry symbol '{entry_name}' not found"));

    let reloc_data = build_base_reloc_table(abs_fixups);
    let reloc_virt_size = reloc_data.len() as u32;
    let reloc_raw_size = pe_align_up(reloc_virt_size.max(1), PE_FILE_ALIGNMENT);
    let size_of_image = pe_align_up(layout.reloc_rva + reloc_virt_size.max(1), PE_SECTION_ALIGNMENT);
    let num_sections: u16 = if layout.has_data { 3 } else { 2 };
    let total_file_size = layout.reloc_file_off + reloc_raw_size;

    let mut buf = vec![0u8; total_file_size as usize];

    // ── DOS header ──
    write_struct(&mut buf, 0, &PeDosHeader {
        e_magic: 0x5A4D, // "MZ"
        e_lfanew: 0x40,
        ..Zeroable::zeroed()
    });

    // ── PE signature ──
    buf[0x40..0x44].copy_from_slice(&0x00004550u32.to_le_bytes());

    // ── COFF header ──
    write_struct(&mut buf, 0x44, &PeCoffHeader {
        machine: 0x8664, // AMD64
        number_of_sections: num_sections,
        size_of_optional_header: size_of::<Pe32PlusOptHeader>() as u16,
        characteristics: 0x0022, // EXECUTABLE_IMAGE | LARGE_ADDRESS_AWARE
        ..Zeroable::zeroed()
    });

    // ── Optional header (PE32+) ──
    let mut data_dirs: [PeDataDirectory; 16] = Zeroable::zeroed();
    data_dirs[5] = PeDataDirectory { virtual_address: layout.reloc_rva, size: reloc_virt_size };

    write_struct(&mut buf, 0x58, &Pe32PlusOptHeader {
        magic: 0x020B,
        size_of_code: layout.text_virt_size,
        size_of_initialized_data: layout.data_virt_size + reloc_virt_size,
        address_of_entry_point: entry_rva,
        base_of_code: layout.text_rva,
        section_alignment: PE_SECTION_ALIGNMENT,
        file_alignment: PE_FILE_ALIGNMENT,
        size_of_image,
        size_of_headers: layout.size_of_headers,
        subsystem,
        dll_characteristics: 0x0160, // DYNAMIC_BASE | HIGH_ENTROPY_VA | NX_COMPAT
        size_of_stack_reserve: 0x100000,
        size_of_stack_commit: 0x1000,
        size_of_heap_reserve: 0x100000,
        size_of_heap_commit: 0x1000,
        number_of_rva_and_sizes: 16,
        data_directories: data_dirs,
        ..Zeroable::zeroed()
    });

    // ── Section headers ──
    let sh_base = 0x58 + size_of::<Pe32PlusOptHeader>();
    let mut sh_off = sh_base;

    fn pe_sec_name(name: &[u8; 8]) -> [u8; 8] { *name }

    write_struct(&mut buf, sh_off, &PeSectionHeader {
        name: pe_sec_name(b".text\0\0\0"),
        virtual_size: layout.text_virt_size,
        virtual_address: layout.text_rva,
        size_of_raw_data: layout.text_raw_size,
        pointer_to_raw_data: layout.text_file_off,
        characteristics: 0x60000020, // CODE|EXEC|READ
        ..Zeroable::zeroed()
    });
    sh_off += size_of::<PeSectionHeader>();

    if layout.has_data {
        write_struct(&mut buf, sh_off, &PeSectionHeader {
            name: pe_sec_name(b".data\0\0\0"),
            virtual_size: layout.data_virt_size,
            virtual_address: layout.data_rva,
            size_of_raw_data: layout.data_raw_size,
            pointer_to_raw_data: layout.data_file_off,
            characteristics: 0xC0000040, // INIT_DATA|READ|WRITE
            ..Zeroable::zeroed()
        });
        sh_off += size_of::<PeSectionHeader>();
    }

    write_struct(&mut buf, sh_off, &PeSectionHeader {
        name: pe_sec_name(b".reloc\0\0"),
        virtual_size: reloc_virt_size,
        virtual_address: layout.reloc_rva,
        size_of_raw_data: reloc_raw_size,
        pointer_to_raw_data: layout.reloc_file_off,
        characteristics: 0x42000040, // INIT_DATA|DISCARDABLE|READ
        ..Zeroable::zeroed()
    });

    // ── Copy section data ──
    let pe_file_off = |rva: u32| -> usize {
        let (base_off, base_rva) = if rva >= layout.data_rva && layout.has_data {
            (layout.data_file_off, layout.data_rva)
        } else {
            (layout.text_file_off, layout.text_rva)
        };
        (base_off + (rva - base_rva)) as usize
    };

    for sec in &state.sections {
        if sec.vaddr == 0 || sec.data.is_empty() { continue; }
        let off = pe_file_off(sec.vaddr as u32);
        buf[off..off + sec.data.len()].copy_from_slice(&sec.data);
    }

    for (sym_name, &got_vaddr) in &layout.got {
        let sym_addr = resolve_symbol(state, sym_name, 0, None)
            .unwrap_or_else(|| panic!("toyos-ld: undefined GOT symbol: {sym_name}"));
        let off = pe_file_off(got_vaddr as u32);
        buf[off..off + 8].copy_from_slice(&sym_addr.to_le_bytes());
    }

    if !reloc_data.is_empty() {
        let off = layout.reloc_file_off as usize;
        buf[off..off + reloc_data.len()].copy_from_slice(&reloc_data);
    }

    buf
}

// ── Static ELF output (--static) ──────────────────────────────────────────

fn emit_static_bytes(
    state: &LinkState,
    layout: &ElfLayout,
    entry_name: &str,
) -> Vec<u8> {
    let entry = resolve_entry(state, entry_name, None);
    let base = layout.base_addr;
    let after_rw = layout.rw_end.max(layout.tls_start + layout.tls_memsz);

    let shstrtab = build_static_shstrtab();
    let shstrtab_file_offset = after_rw - base;
    let num_shdrs: u16 = 4;
    let mut phdr_count = 2u16;
    if layout.tls_memsz > 0 { phdr_count += 1; }
    let shdr_offset = align_up(shstrtab_file_offset + shstrtab.len() as u64, 8);
    let total_file_size = shdr_offset + num_shdrs as u64 * size_of::<Elf64Shdr>() as u64;

    let mut buf = vec![0u8; total_file_size as usize];

    // ── ELF header ──
    write_struct(&mut buf, 0, &Elf64Ehdr {
        e_ident: elf_ident(),
        e_type: elf::ET_EXEC,
        e_machine: elf::EM_X86_64,
        e_version: 1,
        e_entry: entry,
        e_phoff: 64,
        e_shoff: shdr_offset,
        e_ehsize: 64,
        e_phentsize: size_of::<Elf64Phdr>() as u16,
        e_phnum: phdr_count,
        e_shentsize: size_of::<Elf64Shdr>() as u16,
        e_shnum: num_shdrs,
        e_shstrndx: num_shdrs - 1,
        ..Zeroable::zeroed()
    });

    // ── Program headers ──
    // Static ELF: file_offset = vaddr - base_addr
    let mut phdrs = vec![
        Elf64Phdr {
            p_type: elf::PT_LOAD,
            p_flags: elf::PF_R | elf::PF_X,
            p_offset: 0,
            p_vaddr: base,
            p_paddr: base,
            p_filesz: layout.rx_end - base,
            p_memsz: layout.rx_end - base,
            p_align: PAGE_SIZE,
        },
        Elf64Phdr {
            p_type: elf::PT_LOAD,
            p_flags: elf::PF_R | elf::PF_W,
            p_offset: layout.rw_start - base,
            p_vaddr: layout.rw_start,
            p_paddr: layout.rw_start,
            p_filesz: layout.rw_end - layout.rw_start,
            p_memsz: layout.rw_end - layout.rw_start,
            p_align: PAGE_SIZE,
        },
    ];
    if layout.tls_memsz > 0 {
        phdrs.push(Elf64Phdr {
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
    for (i, p) in phdrs.iter().enumerate() {
        write_struct(&mut buf, 64 + i * size_of::<Elf64Phdr>(), p);
    }

    // ── Copy section data ──
    copy_sections_to_buf(&mut buf, &state.sections, base);

    // ── Write GOT entries ──
    let gottpoff_syms: HashSet<String> = state.relocs
        .iter()
        .filter(|r| r.r_type == elf::R_X86_64_GOTTPOFF)
        .map(|r| r.symbol_name.clone())
        .collect();
    for (sym_name, &got_vaddr) in &layout.got {
        let sym_addr = resolve_symbol(state, sym_name, 0, None)
            .unwrap_or_else(|| panic!("toyos-ld: undefined GOT symbol: {sym_name}"));
        let value = if gottpoff_syms.contains(sym_name) {
            tpoff(sym_addr, layout.tls_start, layout.tls_memsz) as u64
        } else {
            sym_addr
        };
        let file_off = (got_vaddr - base) as usize;
        buf[file_off..file_off + 8].copy_from_slice(&value.to_le_bytes());
    }

    copy_to_buf(&mut buf, shstrtab_file_offset, &shstrtab);

    // ── Section headers ──
    let sh = shdr_offset as usize;
    write_struct(&mut buf, sh + 64, &shdr(1, elf::SHT_PROGBITS,
        (elf::SHF_ALLOC | elf::SHF_EXECINSTR) as u64,
        layout.rx_start, layout.rx_start - base, layout.rx_end - layout.rx_start));
    write_struct(&mut buf, sh + 128, &shdr(7, elf::SHT_PROGBITS,
        (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
        layout.rw_start, layout.rw_start - base, layout.rw_end - layout.rw_start));
    write_struct(&mut buf, sh + 192, &shdr(13, elf::SHT_STRTAB,
        0, 0, shstrtab_file_offset, shstrtab.len() as u64));

    buf
}

fn build_static_shstrtab() -> Vec<u8> {
    let mut tab = Vec::new();
    tab.push(0);
    tab.extend_from_slice(b".text\0");      // offset 1
    tab.extend_from_slice(b".data\0");      // offset 7
    tab.extend_from_slice(b".shstrtab\0");  // offset 13
    tab
}

// ── Shared library output (--shared) ─────────────────────────────────────

/// Link object files and produce a shared library (.so) ELF with .dynsym/.dynstr.
pub fn link_shared(objects: &[(String, Vec<u8>)]) -> Result<Vec<u8>, Vec<String>> {
    let mut state = collect(objects);
    synthesize_alloc_shims(&mut state);
    let layout = layout_elf(&mut state, BASE_VADDR, None);
    let params = ElfRelocParams {
        got: &layout.got,
        tls_start: layout.tls_start,
        tls_memsz: layout.tls_memsz,
        plt: Some(&layout.plt),
        dyn_got: &layout.dyn_got,
        record_relatives: true,
        allow_undefined: true,
    };
    let reloc_output = apply_relocs(&mut state, &params)?;
    Ok(emit_shared_bytes(&state, &layout, &reloc_output))
}

fn build_dynsym(state: &LinkState) -> (Vec<u8>, Vec<u8>) {
    let mut dynsym = vec![0u8; size_of::<Elf64Sym>()]; // null entry
    let mut dynstr = vec![0u8];

    let mut symbols: Vec<_> = state.globals.iter().collect();
    symbols.sort_by_key(|(name, _)| *name);

    for (name, def) in symbols {
        if def.section_global_idx == DYNAMIC_SYMBOL_SENTINEL {
            continue;
        }
        let st_name = dynstr.len() as u32;
        dynstr.extend_from_slice(name.as_bytes());
        dynstr.push(0);

        let st_value = state.sections[def.section_global_idx].vaddr + def.value;
        dynsym.extend_from_slice(bytes_of(&Elf64Sym {
            st_name,
            st_info: (elf::STB_GLOBAL << 4) | elf::STT_NOTYPE,
            st_shndx: 1, // defined (non-zero)
            st_value,
            ..Zeroable::zeroed()
        }));
    }

    (dynsym, dynstr)
}

fn build_dynamic(symtab_vaddr: u64, strtab_vaddr: u64, strsz: u64) -> Vec<u8> {
    let mut data = Vec::new();
    for (tag, val) in [
        (elf::DT_SYMTAB, symtab_vaddr), (elf::DT_STRTAB, strtab_vaddr),
        (elf::DT_STRSZ, strsz), (elf::DT_SYMENT, 24), (elf::DT_NULL, 0),
    ] {
        data.extend_from_slice(bytes_of(&Elf64Dyn { d_tag: tag.into(), d_val: val }));
    }
    data
}

fn build_shared_shstrtab(metadata: &[(String, Vec<u8>)]) -> (Vec<u8>, Vec<u32>) {
    let mut tab = Vec::new();
    tab.push(0);                            // offset 0
    tab.extend_from_slice(b".text\0");      // offset 1
    tab.extend_from_slice(b".data\0");      // offset 7
    tab.extend_from_slice(b".rela.dyn\0");  // offset 13
    tab.extend_from_slice(b".dynsym\0");    // offset 23
    tab.extend_from_slice(b".dynstr\0");    // offset 31
    tab.extend_from_slice(b".dynamic\0");   // offset 39
    tab.extend_from_slice(b".shstrtab\0");  // offset 48
    let mut meta_name_offsets = Vec::new();
    for (name, _) in metadata {
        meta_name_offsets.push(tab.len() as u32);
        tab.extend_from_slice(name.as_bytes());
        tab.push(0);
    }
    (tab, meta_name_offsets)
}

fn emit_shared_bytes(
    state: &LinkState,
    layout: &ElfLayout,
    relocs: &RelocOutput,
) -> Vec<u8> {
    let (dynsym_data, dynstr_data) = build_dynsym(state);

    let after_rw = layout.rw_end.max(layout.tls_start + layout.tls_memsz);
    let dynsym_vaddr = align_up(after_rw, 8);
    let dynstr_vaddr = dynsym_vaddr + dynsym_data.len() as u64;
    let dynamic_vaddr = align_up(dynstr_vaddr + dynstr_data.len() as u64, 8);
    let dynamic_data = build_dynamic(dynsym_vaddr, dynstr_vaddr, dynstr_data.len() as u64);
    let dyn_segment_end = align_up(dynamic_vaddr + dynamic_data.len() as u64, PAGE_SIZE);

    let rela_dyn_offset = dyn_segment_end;
    let rela_dyn_size = relocs.relatives.len() as u64 * size_of::<Elf64Rela>() as u64;

    // Metadata sections (e.g. .rustc)
    let mut meta_offset = rela_dyn_offset + rela_dyn_size;
    let mut meta_offsets = Vec::new();
    for (_, data) in &state.metadata {
        meta_offset = align_up(meta_offset, 8);
        meta_offsets.push(meta_offset);
        meta_offset += data.len() as u64;
    }

    let shstrtab_offset = meta_offset;
    let (shstrtab, meta_name_offsets) = build_shared_shstrtab(&state.metadata);
    let num_shdrs = 8u16 + state.metadata.len() as u16;
    let shdr_offset = align_up(shstrtab_offset + shstrtab.len() as u64, 8);
    let total_size = shdr_offset + num_shdrs as u64 * size_of::<Elf64Shdr>() as u64;

    let mut buf = vec![0u8; total_size as usize];

    // ── ELF header ──
    let mut phdr_count = 4u16;
    if layout.tls_memsz > 0 { phdr_count += 1; }
    write_struct(&mut buf, 0, &Elf64Ehdr {
        e_ident: elf_ident(),
        e_type: elf::ET_DYN,
        e_machine: elf::EM_X86_64,
        e_version: 1,
        e_phoff: 64,
        e_shoff: shdr_offset,
        e_ehsize: 64,
        e_phentsize: size_of::<Elf64Phdr>() as u16,
        e_phnum: phdr_count,
        e_shentsize: size_of::<Elf64Shdr>() as u16,
        e_shnum: num_shdrs,
        e_shstrndx: num_shdrs - 1,
        ..Zeroable::zeroed()
    });

    // ── Program headers ──
    let mut phdrs = vec![
        phdr(elf::PT_LOAD, elf::PF_R | elf::PF_X,
            BASE_VADDR, layout.rx_end - BASE_VADDR, layout.rx_end - BASE_VADDR, PAGE_SIZE),
        phdr(elf::PT_LOAD, elf::PF_R | elf::PF_W,
            layout.rw_start, layout.rw_end - layout.rw_start, layout.rw_end - layout.rw_start, PAGE_SIZE),
        phdr(elf::PT_LOAD, elf::PF_R,
            dynsym_vaddr, dyn_segment_end - dynsym_vaddr, dyn_segment_end - dynsym_vaddr, PAGE_SIZE),
        phdr(elf::PT_DYNAMIC, elf::PF_R,
            dynamic_vaddr, dynamic_data.len() as u64, dynamic_data.len() as u64, 8),
    ];
    if layout.tls_memsz > 0 {
        phdrs.push(phdr(elf::PT_TLS, elf::PF_R,
            layout.tls_start, layout.tls_filesz, layout.tls_memsz, 64));
    }
    for (i, p) in phdrs.iter().enumerate() {
        write_struct(&mut buf, 64 + i * size_of::<Elf64Phdr>(), p);
    }

    // ── Section data ──
    copy_sections_to_buf(&mut buf, &state.sections, BASE_VADDR);
    copy_to_buf(&mut buf, dynsym_vaddr - BASE_VADDR, &dynsym_data);
    copy_to_buf(&mut buf, dynstr_vaddr - BASE_VADDR, &dynstr_data);
    copy_to_buf(&mut buf, dynamic_vaddr - BASE_VADDR, &dynamic_data);

    // .rela.dyn
    let empty = HashMap::new();
    write_rela_entries(&mut buf, rela_dyn_offset as usize, &relocs.relatives, &[], &empty);

    // Metadata sections
    for (i, (_, data)) in state.metadata.iter().enumerate() {
        copy_to_buf(&mut buf, meta_offsets[i], data);
    }

    copy_to_buf(&mut buf, shstrtab_offset, &shstrtab);

    // ── Section headers ──
    let sh = shdr_offset as usize;
    write_struct(&mut buf, sh + 64, &shdr(1, elf::SHT_PROGBITS,
        (elf::SHF_ALLOC | elf::SHF_EXECINSTR) as u64,
        layout.rx_start, layout.rx_start - BASE_VADDR, layout.rx_end - layout.rx_start));
    write_struct(&mut buf, sh + 128, &shdr(7, elf::SHT_PROGBITS,
        (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
        layout.rw_start, layout.rw_start - BASE_VADDR, layout.rw_end - layout.rw_start));
    write_struct(&mut buf, sh + 192, &shdr_full(13, elf::SHT_RELA,
        elf::SHF_ALLOC as u64, 0, rela_dyn_offset, rela_dyn_size, 0, 0, 8, 24));
    write_struct(&mut buf, sh + 256, &shdr_full(23, elf::SHT_DYNSYM,
        elf::SHF_ALLOC as u64, dynsym_vaddr, dynsym_vaddr - BASE_VADDR,
        dynsym_data.len() as u64, 5, 1, 8, 24));
    write_struct(&mut buf, sh + 320, &shdr(31, elf::SHT_STRTAB,
        elf::SHF_ALLOC as u64, dynstr_vaddr, dynstr_vaddr - BASE_VADDR, dynstr_data.len() as u64));
    write_struct(&mut buf, sh + 384, &shdr_full(39, elf::SHT_DYNAMIC,
        (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
        dynamic_vaddr, dynamic_vaddr - BASE_VADDR, dynamic_data.len() as u64, 5, 0, 8, 16));
    for (i, (_, data)) in state.metadata.iter().enumerate() {
        write_struct(&mut buf, sh + (7 + i) * 64, &shdr(meta_name_offsets[i], elf::SHT_PROGBITS,
            0, 0, meta_offsets[i], data.len() as u64));
    }
    write_struct(&mut buf, sh + (num_shdrs - 1) as usize * 64, &shdr(48, elf::SHT_STRTAB,
        0, 0, shstrtab_offset, shstrtab.len() as u64));

    buf
}

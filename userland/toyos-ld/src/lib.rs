//! toyos-ld: Minimal linker for ToyOS.
//!
//! Reads ELF and COFF object files. Produces PIE ELF, static ELF, or PE32+.
//! Supports .o object files and .rlib/.a archives (ar format).

use object::{elf, pe};
use object::read::elf::ElfFile64;
use object::read::{self, Object, ObjectSection, ObjectSymbol};
use object::RelocationFlags;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::{fs, process};

const BASE_VADDR: u64 = 0;
const PAGE_SIZE: u64 = 0x1000;

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
    let pe_layout = layout_pe(&mut state, entry);
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
    let layout = layout_static(&mut state, entry, base_addr);
    apply_relocs_static(&mut state, &layout)?;
    Ok(emit_static_bytes(&state, &layout, entry))
}

/// Link object files and produce a PIE ELF executable.
/// Returns the raw ELF bytes on success, or a list of undefined symbols on failure.
pub fn link(objects: &[(String, Vec<u8>)], entry: &str) -> Result<Vec<u8>, Vec<String>> {
    let mut state = collect(objects);
    synthesize_alloc_shims(&mut state);
    let layout = layout(&mut state, Some(entry));
    let reloc_output = apply_relocs(&mut state, &layout, false)?;
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
        let data = fs::read(path).unwrap_or_else(|e| {
            eprintln!("toyos-ld: cannot read {}: {e}", path.display());
            process::exit(1);
        });
        if is_archive(&data) {
            extract_archive(&path.display().to_string(), &data, &mut objects);
        } else {
            objects.push((path.display().to_string(), data));
        }
    }

    for lib in libs {
        if let Some((name, data)) = find_lib(lib, lib_paths) {
            if is_archive(&data) {
                extract_archive(&name, &data, &mut objects);
            } else {
                objects.push((name, data));
            }
        }
    }

    objects
}

// ── Input file reading ───────────────────────────────────────────────────

fn is_archive(data: &[u8]) -> bool {
    data.starts_with(b"!<arch>\n") || data.starts_with(b"!<thin>\n")
}

fn extract_archive(name: &str, data: &[u8], out: &mut Vec<(String, Vec<u8>)>) {
    let archive = match object::read::archive::ArchiveFile::parse(data) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("toyos-ld: cannot parse archive {name}: {e}");
            return;
        }
    };
    for member in archive.members() {
        let member = match member {
            Ok(m) => m,
            Err(_) => continue,
        };
        let member_name = String::from_utf8_lossy(member.name()).to_string();
        if !member_name.ends_with(".o") {
            continue;
        }
        let member_data = match member.data(data) {
            Ok(d) => d,
            Err(_) => continue,
        };
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
        let obj = object::File::parse(data.as_slice()).unwrap_or_else(|e| {
            eprintln!("toyos-ld: cannot parse {name}: {e}");
            process::exit(1);
        });

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
        other => {
            eprintln!("toyos-ld: unsupported COFF relocation type 0x{other:04x}");
            process::exit(1);
        }
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

struct LayoutResult {
    rx_start: u64,
    rx_end: u64,
    rw_start: u64,
    rw_end: u64,
    tls_start: u64,
    tls_filesz: u64,
    tls_memsz: u64,
    got: HashMap<String, u64>,
    /// PLT stub virtual addresses for dynamic symbols.
    plt: HashMap<String, u64>,
    /// Raw PLT stub code (concatenated 6-byte stubs).
    plt_data: Vec<u8>,
    /// Base virtual address of PLT stubs.
    plt_vaddr: u64,
    /// GOT entries for dynamic symbols (symbol → GOT vaddr).
    dyn_got: HashMap<String, u64>,
}

fn layout(state: &mut LinkState, entry_name: Option<&str>) -> LayoutResult {
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

    let mut cursor = BASE_VADDR + headers_size;

    let rx_start = cursor;
    for &idx in &rx_sections {
        let sec = &mut state.sections[idx];
        cursor = align_up(cursor, sec.align);
        sec.vaddr = cursor;
        cursor += sec.size;
    }
    // Collect dynamic symbols referenced by relocations (need PLT stubs)
    let mut dyn_syms: Vec<String> = Vec::new();
    for reloc in &state.relocs {
        if state.dynamic_imports.contains(&reloc.symbol_name) && !dyn_syms.contains(&reloc.symbol_name) {
            dyn_syms.push(reloc.symbol_name.clone());
        }
    }
    // Entry symbol needs a PLT stub too if it's a dynamic import
    if let Some(entry) = entry_name {
        if state.dynamic_imports.contains(entry) && !dyn_syms.contains(&entry.to_string()) {
            dyn_syms.push(entry.to_string());
        }
    }

    // PLT stubs go at the end of the RX segment (each stub is 6 bytes: jmp *[rip+off])
    const PLT_STUB_SIZE: u64 = 6;
    let plt_vaddr = if dyn_syms.is_empty() { cursor } else { align_up(cursor, 16) };
    let plt_total = dyn_syms.len() as u64 * PLT_STUB_SIZE;
    cursor = plt_vaddr + plt_total;

    let rx_end = align_up(cursor, PAGE_SIZE);

    cursor = rx_end;
    let rw_start = cursor;
    for &idx in &rw_sections {
        let sec = &mut state.sections[idx];
        cursor = align_up(cursor, sec.align);
        sec.vaddr = cursor;
        cursor += sec.size;
    }

    // Collect GOT entries needed (GOTPCREL* and GOTTPOFF both need GOT slots)
    let mut got_symbols: Vec<String> = Vec::new();
    for reloc in &state.relocs {
        match reloc.r_type {
            elf::R_X86_64_GOTPCREL
            | elf::R_X86_64_GOTPCRELX
            | elf::R_X86_64_REX_GOTPCRELX
            | elf::R_X86_64_GOTTPOFF => {
                if !got_symbols.contains(&reloc.symbol_name) {
                    got_symbols.push(reloc.symbol_name.clone());
                }
            }
            _ => {}
        }
    }

    cursor = align_up(cursor, 8);
    let mut got = HashMap::new();
    for sym in &got_symbols {
        got.insert(sym.clone(), cursor);
        cursor += 8;
    }

    // Dynamic GOT entries for PLT stubs (each PLT stub jumps through its GOT entry)
    let mut dyn_got = HashMap::new();
    for sym in &dyn_syms {
        dyn_got.insert(sym.clone(), cursor);
        cursor += 8;
    }

    let rw_end = align_up(cursor, PAGE_SIZE);

    // Build PLT stub code: each stub is `jmp *[rip + offset]` (FF 25 xx xx xx xx)
    // RIP at execution = plt_entry_vaddr + 6 (after the 6-byte instruction)
    let mut plt = HashMap::new();
    let mut plt_data = Vec::new();
    for (i, sym) in dyn_syms.iter().enumerate() {
        let stub_vaddr = plt_vaddr + i as u64 * PLT_STUB_SIZE;
        plt.insert(sym.clone(), stub_vaddr);
        let got_vaddr = dyn_got[sym];
        let rip = stub_vaddr + 6; // RIP after instruction
        let offset = (got_vaddr as i64 - rip as i64) as i32;
        plt_data.extend_from_slice(&[0xFF, 0x25]);
        plt_data.extend_from_slice(&offset.to_le_bytes());
    }

    // TLS layout — used for TPOFF computation
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

    LayoutResult {
        rx_start,
        rx_end,
        rw_start,
        rw_end,
        tls_start,
        tls_filesz,
        tls_memsz,
        got,
        plt,
        plt_data,
        plt_vaddr,
        dyn_got,
    }
}

fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

// ── Relocation ───────────────────────────────────────────────────────────

struct RelocOutput {
    relatives: Vec<(u64, i64)>,
    /// Dynamic GOT entries needing GLOB_DAT relocations: (GOT slot vaddr, symbol name).
    glob_dats: Vec<(u64, String)>,
}

fn resolve_symbol(state: &LinkState, layout: &LayoutResult, name: &str, from_sec: usize) -> Option<u64> {
    if let Some(def) = state.globals.get(name) {
        if def.section_global_idx == DYNAMIC_SYMBOL_SENTINEL {
            // Dynamic symbol — resolve through PLT stub
            return layout.plt.get(name).copied();
        }
        let sec = &state.sections[def.section_global_idx];
        return Some(sec.vaddr + def.value);
    }
    let obj_idx = state.sections[from_sec].obj_idx;
    if let Some(def) = state.locals.get(&(obj_idx, name.to_string())) {
        let sec = &state.sections[def.section_global_idx];
        return Some(sec.vaddr + def.value);
    }
    None
}

/// x86-64 Variant II: TP points to end of TLS block.
/// TPOFF = symbol_vaddr - (tls_start + tls_memsz)
fn tpoff(sym_addr: u64, layout: &LayoutResult) -> i64 {
    sym_addr as i64 - (layout.tls_start as i64 + layout.tls_memsz as i64)
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

fn apply_relocs(
    state: &mut LinkState,
    layout: &LayoutResult,
    allow_undefined: bool,
) -> Result<RelocOutput, Vec<String>> {
    let mut relatives = Vec::new();
    let mut undefined = HashSet::new();

    let relocs: Vec<InputReloc> = state.relocs.clone();

    // Pass 1: TLS GD/LD/DTPOFF relaxations. These rewrite instruction bytes
    // and overwrite the companion `call __tls_get_addr` instruction, so we
    // track which (section, offset) ranges were relaxed.
    let mut relaxed_calls: HashSet<(usize, u64)> = HashSet::new();

    for reloc in &relocs {
        match reloc.r_type {
            elf::R_X86_64_TLSGD => {
                let sym_addr = resolve_symbol(state, layout, &reloc.symbol_name, reloc.section_global_idx)
                    .unwrap_or(0);
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
                        tpoff(sym_addr, layout) as i32);
                    relaxed_calls.insert((reloc.section_global_idx, reloc.offset + 8));
                } else {
                    // GD → LE (12-byte unpadded): `leaq; call`
                    // → `mov %fs:0,%rax; lea tpoff(%rax),%rax` (overflows into 16 bytes)
                    // Use IE-style: `addq %fs:0,%rax` doesn't work either.
                    // For 12-byte: rewrite to `mov %fs:0,%rax; nop; nop; nop`
                    // and patch the TPOFF inline at the usage site via DTPOFF32.
                    // Actually, for 12-byte we can use: movq %fs:0,%rax (9); lea off(%rax),%rax
                    // won't fit. Use: movl %fs:tpoff, %eax (9 bytes); nopl (%rax) (3 bytes)
                    // = `64 a1 XX XX XX XX 00 00 00 00; 0f 1f 00`
                    // Wait, that's for 32-bit. For 64-bit, use:
                    //   mov %fs:0,%rax (9 bytes) + nopw (%rax) (3 bytes) = 12
                    // The tpoff value is patched by the companion DTPOFF32.
                    // Actually, in practice LLVM always emits the padded form for GD.
                    // If we hit unpadded, it's an error.
                    panic!("toyos-ld: unpadded 12-byte TLSGD sequence not supported");
                }
            }
            elf::R_X86_64_TLSLD => {
                let padded = is_padded_tls_sequence(
                    &state.sections[reloc.section_global_idx].data,
                    reloc.offset,
                );
                if padded {
                    // LD → LE (16-byte padded): `data16; leaq; data16*2; rex64; call`
                    // → `data16*3; mov %fs:0,%rax; nopl 0(%rax)`
                    #[rustfmt::skip]
                    let inst: [u8; 16] = [
                        0x66, 0x66, 0x66,                                           // 3x data16
                        0x64, 0x48, 0x8b, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00,     // mov %fs:0,%rax
                        0x0f, 0x1f, 0x40, 0x00,                                     // nopl 0(%rax)
                    ];
                    write_bytes(state, reloc.section_global_idx, reloc.offset - 4, &inst);
                    relaxed_calls.insert((reloc.section_global_idx, reloc.offset + 8));
                } else {
                    // LD → LE (12-byte unpadded): `leaq; call`
                    // → `data16; data16; data16; mov %fs:0,%rax`
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
                // In a static link, DTPOFF → TPOFF.
                let sym_addr = resolve_symbol(state, layout, &reloc.symbol_name, reloc.section_global_idx)
                    .unwrap_or(0);
                write_i32(state, reloc.section_global_idx, reloc.offset,
                    (tpoff(sym_addr, layout) + reloc.addend) as i32);
            }
            _ => {}
        }
    }

    // Pass 2: all other relocations
    for reloc in &relocs {
        // Skip TLS relocations already handled
        match reloc.r_type {
            elf::R_X86_64_TLSGD | elf::R_X86_64_TLSLD | elf::R_X86_64_DTPOFF32 => continue,
            _ => {}
        }
        // Skip companion __tls_get_addr calls that were overwritten
        if relaxed_calls.contains(&(reloc.section_global_idx, reloc.offset)) {
            continue;
        }

        let sec = &state.sections[reloc.section_global_idx];
        let reloc_vaddr = sec.vaddr + reloc.offset;

        let sym_addr = match resolve_symbol(state, layout, &reloc.symbol_name, reloc.section_global_idx) {
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
                relatives.push((reloc_vaddr, sym_addr as i64 + reloc.addend));
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
                let got_slot = layout.got[&reloc.symbol_name];
                let value = got_slot as i64 + reloc.addend - reloc_vaddr as i64;
                write_i32(state, reloc.section_global_idx, reloc.offset, value as i32);
            }
            elf::R_X86_64_TPOFF32 => {
                let tp = tpoff(sym_addr, layout);
                write_i32(
                    state,
                    reloc.section_global_idx,
                    reloc.offset,
                    (tp + reloc.addend) as i32,
                );
            }
            elf::R_X86_64_GOTTPOFF => {
                let got_slot = layout.got[&reloc.symbol_name];
                let value = got_slot as i64 + reloc.addend - reloc_vaddr as i64;
                write_i32(state, reloc.section_global_idx, reloc.offset, value as i32);
            }
            other => {
                eprintln!(
                    "toyos-ld: unsupported relocation type {other} for symbol {}",
                    reloc.symbol_name
                );
            }
        }
    }

    // Fill GOT entries
    let gottpoff_syms: HashSet<String> = relocs
        .iter()
        .filter(|r| r.r_type == elf::R_X86_64_GOTTPOFF)
        .map(|r| r.symbol_name.clone())
        .collect();

    for (sym_name, &got_vaddr) in &layout.got {
        let sym_addr = resolve_symbol(state, layout, sym_name, 0).unwrap_or(0);
        if gottpoff_syms.contains(sym_name) {
            let tp = tpoff(sym_addr, layout);
            relatives.push((got_vaddr, tp));
        } else {
            relatives.push((got_vaddr, sym_addr as i64));
        }
    }

    // Collect dynamic GOT entries as GLOB_DAT relocations (resolved at load time)
    let mut glob_dats = Vec::new();
    for (sym_name, &got_vaddr) in &layout.dyn_got {
        glob_dats.push((got_vaddr, sym_name.clone()));
    }

    if !allow_undefined && !undefined.is_empty() {
        let mut syms: Vec<String> = undefined.into_iter().collect();
        syms.sort();
        return Err(syms);
    }

    Ok(RelocOutput { relatives, glob_dats })
}

// ── ELF output ───────────────────────────────────────────────────────────

fn emit_bytes(
    state: &LinkState,
    layout: &LayoutResult,
    relocs: &RelocOutput,
    entry_name: &str,
) -> Vec<u8> {
    let is_dynamic = !state.dynamic_libs.is_empty();

    // Resolve entry point — use PLT stub if entry is a dynamic import
    let entry = state
        .globals
        .get(entry_name)
        .map(|def| {
            if def.section_global_idx == DYNAMIC_SYMBOL_SENTINEL {
                *layout.plt.get(entry_name).unwrap_or_else(|| {
                    panic!("toyos-ld: entry '{entry_name}' is in .so but has no PLT entry");
                })
            } else {
                state.sections[def.section_global_idx].vaddr + def.value
            }
        })
        .unwrap_or_else(|| {
            panic!("toyos-ld: entry symbol '{entry_name}' not found");
        });

    let after_rw = layout.rw_end.max(layout.tls_start + layout.tls_memsz);

    // Build dynamic sections for import-style dynsym/dynstr/.dynamic
    let (dynsym_data, dynstr_data, needed_offsets, sym_indices) = if is_dynamic {
        build_import_dynsym(&relocs.glob_dats, &state.dynamic_libs)
    } else {
        (Vec::new(), Vec::new(), Vec::new(), HashMap::new())
    };

    // Layout the dynamic segment (if dynamic) or just .rela.dyn (if static)
    let (dynsym_vaddr, dynstr_vaddr, rela_dyn_vaddr, dynamic_vaddr, dyn_segment_end);
    let num_rela = relocs.relatives.len() + relocs.glob_dats.len();
    let rela_dyn_size = num_rela as u64 * 24;

    // .dynamic section data (built after layout is known)
    let dynamic_data;

    if is_dynamic {
        dynsym_vaddr = align_up(after_rw, 8);
        dynstr_vaddr = dynsym_vaddr + dynsym_data.len() as u64;
        rela_dyn_vaddr = align_up(dynstr_vaddr + dynstr_data.len() as u64, 8);
        dynamic_vaddr = align_up(rela_dyn_vaddr + rela_dyn_size, 8);

        // Build .dynamic entries
        let num_dynamic_entries = state.dynamic_libs.len() + 8; // DT_NEEDED×N + 7 tags + DT_NULL
        let dynamic_size = num_dynamic_entries as u64 * 16;
        dyn_segment_end = align_up(dynamic_vaddr + dynamic_size, PAGE_SIZE);

        let mut dyn_entries = Vec::with_capacity(num_dynamic_entries * 16);
        for offset in &needed_offsets {
            dyn_entries.extend_from_slice(&(elf::DT_NEEDED as i64).to_le_bytes());
            dyn_entries.extend_from_slice(&(*offset as u64).to_le_bytes());
        }
        for &(tag, val) in &[
            (elf::DT_SYMTAB as i64, dynsym_vaddr),
            (elf::DT_STRTAB as i64, dynstr_vaddr),
            (elf::DT_STRSZ as i64, dynstr_data.len() as u64),
            (elf::DT_SYMENT as i64, 24u64),
            (elf::DT_RELA as i64, rela_dyn_vaddr),
            (elf::DT_RELASZ as i64, rela_dyn_size),
            (elf::DT_RELAENT as i64, 24u64),
        ] {
            dyn_entries.extend_from_slice(&tag.to_le_bytes());
            dyn_entries.extend_from_slice(&val.to_le_bytes());
        }
        dyn_entries.extend_from_slice(&(elf::DT_NULL as i64).to_le_bytes());
        dyn_entries.extend_from_slice(&0u64.to_le_bytes());
        dynamic_data = dyn_entries;
    } else {
        dynsym_vaddr = 0;
        dynstr_vaddr = 0;
        rela_dyn_vaddr = align_up(after_rw, 8);
        dynamic_vaddr = 0;
        dyn_segment_end = 0;
        dynamic_data = Vec::new();
    }

    // Non-loadable sections: shstrtab + section headers
    let shstrtab_file_offset = if is_dynamic {
        dyn_segment_end
    } else {
        rela_dyn_vaddr + rela_dyn_size
    };

    let shstrtab = if is_dynamic { build_dynamic_shstrtab() } else { build_shstrtab() };
    let shstrtab_size = shstrtab.len() as u64;

    let num_shdrs: u16 = if is_dynamic { 8 } else { 5 };
    let shdr_offset = align_up(shstrtab_file_offset + shstrtab_size, 8);
    let total_size = shdr_offset + num_shdrs as u64 * 64;

    let mut buf = vec![0u8; total_size as usize];

    // ── ELF header ──
    let ehdr = &mut buf[..64];
    ehdr[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    ehdr[4] = 2; // ELFCLASS64
    ehdr[5] = 1; // ELFDATA2LSB
    ehdr[6] = 1; // EV_CURRENT
    ehdr[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    ehdr[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    ehdr[20..24].copy_from_slice(&1u32.to_le_bytes()); // EV_CURRENT
    ehdr[24..32].copy_from_slice(&entry.to_le_bytes());
    ehdr[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    ehdr[40..48].copy_from_slice(&shdr_offset.to_le_bytes());
    ehdr[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    ehdr[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    let mut phdr_count = 2u16; // PT_LOAD×2 (RX + RW)
    if layout.tls_memsz > 0 { phdr_count += 1; }
    if is_dynamic { phdr_count += 2; } // PT_LOAD (dynamic) + PT_DYNAMIC
    ehdr[56..58].copy_from_slice(&phdr_count.to_le_bytes());
    ehdr[58..60].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
    ehdr[60..62].copy_from_slice(&num_shdrs.to_le_bytes());
    ehdr[62..64].copy_from_slice(&(num_shdrs - 1).to_le_bytes()); // e_shstrndx

    // ── Program headers ──
    let mut ph = 64usize;
    write_phdr(&mut buf[ph..], elf::PT_LOAD, elf::PF_R | elf::PF_X,
        BASE_VADDR, BASE_VADDR,
        layout.rx_end - BASE_VADDR, layout.rx_end - BASE_VADDR, PAGE_SIZE);
    ph += 56;
    write_phdr(&mut buf[ph..], elf::PT_LOAD, elf::PF_R | elf::PF_W,
        layout.rw_start, layout.rw_start,
        layout.rw_end - layout.rw_start, layout.rw_end - layout.rw_start, PAGE_SIZE);
    ph += 56;
    if layout.tls_memsz > 0 {
        write_phdr(&mut buf[ph..], elf::PT_TLS, elf::PF_R,
            layout.tls_start, layout.tls_start,
            layout.tls_filesz, layout.tls_memsz, 64);
        ph += 56;
    }
    if is_dynamic {
        write_phdr(&mut buf[ph..], elf::PT_LOAD, elf::PF_R,
            dynsym_vaddr, dynsym_vaddr,
            dyn_segment_end - dynsym_vaddr, dyn_segment_end - dynsym_vaddr, PAGE_SIZE);
        ph += 56;
        write_phdr(&mut buf[ph..], elf::PT_DYNAMIC, elf::PF_R,
            dynamic_vaddr, dynamic_vaddr,
            dynamic_data.len() as u64, dynamic_data.len() as u64, 8);
    }

    // ── Copy section data ──
    for sec in &state.sections {
        if sec.vaddr == 0 || sec.data.is_empty() { continue; }
        let file_off = (sec.vaddr - BASE_VADDR) as usize;
        buf[file_off..file_off + sec.data.len()].copy_from_slice(&sec.data);
    }

    // Write PLT stubs
    if !layout.plt_data.is_empty() {
        let plt_off = (layout.plt_vaddr - BASE_VADDR) as usize;
        buf[plt_off..plt_off + layout.plt_data.len()].copy_from_slice(&layout.plt_data);
    }

    // Write dynamic segment data
    if is_dynamic {
        let off = (dynsym_vaddr - BASE_VADDR) as usize;
        buf[off..off + dynsym_data.len()].copy_from_slice(&dynsym_data);
        let off = (dynstr_vaddr - BASE_VADDR) as usize;
        buf[off..off + dynstr_data.len()].copy_from_slice(&dynstr_data);
        let off = (dynamic_vaddr - BASE_VADDR) as usize;
        buf[off..off + dynamic_data.len()].copy_from_slice(&dynamic_data);
    }

    // ── Write .rela.dyn ──
    let rela_off = (rela_dyn_vaddr) as usize;
    // If dynamic, rela is in a PT_LOAD at its vaddr; if static, it's at file offset = vaddr
    let rela_file_off = if is_dynamic {
        (rela_dyn_vaddr - BASE_VADDR) as usize
    } else {
        rela_off
    };
    // R_X86_64_RELATIVE entries
    let mut rela_cursor = rela_file_off;
    for &(offset, addend) in &relocs.relatives {
        buf[rela_cursor..rela_cursor + 8].copy_from_slice(&offset.to_le_bytes());
        buf[rela_cursor + 8..rela_cursor + 16].copy_from_slice(&8u64.to_le_bytes());
        buf[rela_cursor + 16..rela_cursor + 24].copy_from_slice(&addend.to_le_bytes());
        rela_cursor += 24;
    }
    // R_X86_64_GLOB_DAT entries (dynamic only)
    for &(got_vaddr, ref sym_name) in &relocs.glob_dats {
        let sym_idx = sym_indices.get(sym_name).copied().unwrap_or(0) as u64;
        let r_info = (sym_idx << 32) | 6; // R_X86_64_GLOB_DAT = 6
        buf[rela_cursor..rela_cursor + 8].copy_from_slice(&got_vaddr.to_le_bytes());
        buf[rela_cursor + 8..rela_cursor + 16].copy_from_slice(&r_info.to_le_bytes());
        buf[rela_cursor + 16..rela_cursor + 24].copy_from_slice(&0i64.to_le_bytes());
        rela_cursor += 24;
    }

    // ── Write .shstrtab ──
    let shstrtab_off = shstrtab_file_offset as usize;
    buf[shstrtab_off..shstrtab_off + shstrtab.len()].copy_from_slice(&shstrtab);

    // ── Section headers ──
    let sh = shdr_offset as usize;
    if is_dynamic {
        // 0: NULL (already zeroed)
        // 1: .text
        write_shdr(&mut buf[sh + 64..], 1, elf::SHT_PROGBITS,
            (elf::SHF_ALLOC | elf::SHF_EXECINSTR) as u64,
            layout.rx_start, layout.rx_start - BASE_VADDR,
            layout.rx_end - layout.rx_start, 0, 0, 16, 0);
        // 2: .data
        write_shdr(&mut buf[sh + 128..], 7, elf::SHT_PROGBITS,
            (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
            layout.rw_start, layout.rw_start - BASE_VADDR,
            layout.rw_end - layout.rw_start, 0, 0, 8, 0);
        // 3: .dynsym (sh_link=4 → .dynstr, sh_info=1 → first global sym)
        write_shdr(&mut buf[sh + 192..], 13, elf::SHT_DYNSYM,
            elf::SHF_ALLOC as u64,
            dynsym_vaddr, dynsym_vaddr - BASE_VADDR,
            dynsym_data.len() as u64, 4, 1, 8, 24);
        // 4: .dynstr
        write_shdr(&mut buf[sh + 256..], 21, elf::SHT_STRTAB,
            elf::SHF_ALLOC as u64,
            dynstr_vaddr, dynstr_vaddr - BASE_VADDR,
            dynstr_data.len() as u64, 0, 0, 1, 0);
        // 5: .rela.dyn (sh_link=3 → .dynsym)
        write_shdr(&mut buf[sh + 320..], 29, elf::SHT_RELA,
            elf::SHF_ALLOC as u64,
            rela_dyn_vaddr, rela_dyn_vaddr - BASE_VADDR,
            rela_dyn_size, 3, 0, 8, 24);
        // 6: .dynamic (sh_link=4 → .dynstr)
        write_shdr(&mut buf[sh + 384..], 39, elf::SHT_DYNAMIC,
            (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
            dynamic_vaddr, dynamic_vaddr - BASE_VADDR,
            dynamic_data.len() as u64, 4, 0, 8, 16);
        // 7: .shstrtab
        write_shdr(&mut buf[sh + 448..], 48, elf::SHT_STRTAB,
            0, 0, shstrtab_file_offset, shstrtab_size, 0, 0, 1, 0);
    } else {
        // Static executable section headers
        write_shdr(&mut buf[sh + 64..], 1, elf::SHT_PROGBITS,
            (elf::SHF_ALLOC | elf::SHF_EXECINSTR) as u64,
            layout.rx_start, layout.rx_start - BASE_VADDR,
            layout.rx_end - layout.rx_start, 0, 0, 16, 0);
        write_shdr(&mut buf[sh + 128..], 7, elf::SHT_PROGBITS,
            (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
            layout.rw_start, layout.rw_start - BASE_VADDR,
            layout.rw_end - layout.rw_start, 0, 0, 8, 0);
        write_shdr(&mut buf[sh + 192..], 13, elf::SHT_RELA,
            elf::SHF_ALLOC as u64,
            0, rela_dyn_vaddr, rela_dyn_size, 0, 0, 8, 24);
        write_shdr(&mut buf[sh + 256..], 23, elf::SHT_STRTAB,
            0, 0, shstrtab_file_offset, shstrtab_size, 0, 0, 1, 0);
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
/// Returns (dynsym_data, dynstr_data, needed_offsets, sym_indices).
fn build_import_dynsym(
    glob_dats: &[(u64, String)],
    dynamic_libs: &[String],
) -> (Vec<u8>, Vec<u8>, Vec<u32>, HashMap<String, u32>) {
    let mut dynstr = vec![0u8]; // leading null
    let mut dynsym = vec![0u8; 24]; // null Elf64_Sym

    // DT_NEEDED filenames go into dynstr first
    let mut needed_offsets = Vec::new();
    for lib in dynamic_libs {
        needed_offsets.push(dynstr.len() as u32);
        dynstr.extend_from_slice(lib.as_bytes());
        dynstr.push(0);
    }

    // Import symbols (one per unique GLOB_DAT entry)
    let mut sym_indices = HashMap::new();
    let mut sym_idx = 1u32; // 0 is null entry
    for (_, sym_name) in glob_dats {
        if sym_indices.contains_key(sym_name) {
            continue;
        }
        let st_name = dynstr.len() as u32;
        dynstr.extend_from_slice(sym_name.as_bytes());
        dynstr.push(0);

        let mut sym = [0u8; 24];
        sym[0..4].copy_from_slice(&st_name.to_le_bytes());
        sym[4] = (elf::STB_GLOBAL << 4) | elf::STT_NOTYPE;
        // st_shndx = 0 (SHN_UNDEF), st_value = 0, st_size = 0
        dynsym.extend_from_slice(&sym);

        sym_indices.insert(sym_name.clone(), sym_idx);
        sym_idx += 1;
    }

    (dynsym, dynstr, needed_offsets, sym_indices)
}

fn write_phdr(
    buf: &mut [u8],
    p_type: u32,
    p_flags: u32,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
) {
    buf[0..4].copy_from_slice(&p_type.to_le_bytes());
    buf[4..8].copy_from_slice(&p_flags.to_le_bytes());
    let p_offset = p_vaddr - BASE_VADDR;
    buf[8..16].copy_from_slice(&p_offset.to_le_bytes());
    buf[16..24].copy_from_slice(&p_vaddr.to_le_bytes());
    buf[24..32].copy_from_slice(&p_paddr.to_le_bytes());
    buf[32..40].copy_from_slice(&p_filesz.to_le_bytes());
    buf[40..48].copy_from_slice(&p_memsz.to_le_bytes());
    buf[48..56].copy_from_slice(&p_align.to_le_bytes());
}

fn write_shdr(
    buf: &mut [u8],
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
) {
    buf[0..4].copy_from_slice(&sh_name.to_le_bytes());
    buf[4..8].copy_from_slice(&sh_type.to_le_bytes());
    buf[8..16].copy_from_slice(&sh_flags.to_le_bytes());
    buf[16..24].copy_from_slice(&sh_addr.to_le_bytes());
    buf[24..32].copy_from_slice(&sh_offset.to_le_bytes());
    buf[32..40].copy_from_slice(&sh_size.to_le_bytes());
    buf[40..44].copy_from_slice(&sh_link.to_le_bytes());
    buf[44..48].copy_from_slice(&sh_info.to_le_bytes());
    buf[48..56].copy_from_slice(&sh_addralign.to_le_bytes());
    buf[56..64].copy_from_slice(&sh_entsize.to_le_bytes());
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
    #[allow(dead_code)]
    size_of_image: u32,
    got: HashMap<String, u64>,
}

/// PE section layout: RVAs use PE_SECTION_ALIGNMENT, file uses PE_FILE_ALIGNMENT.
/// All section vaddrs in LinkState are set relative to text_rva.
fn layout_pe(state: &mut LinkState, _entry_name: &str) -> PeLayout {
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
    let mut got_symbols: Vec<String> = Vec::new();
    for reloc in &state.relocs {
        match reloc.r_type {
            elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX
            | elf::R_X86_64_REX_GOTPCRELX => {
                if !got_symbols.contains(&reloc.symbol_name) {
                    got_symbols.push(reloc.symbol_name.clone());
                }
            }
            _ => {}
        }
    }
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

    // size_of_image will be updated after we know reloc size
    let size_of_image = 0; // placeholder

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
        size_of_image,
        got,
    }
}

fn pe_align_up(value: u32, alignment: u32) -> u32 {
    (value + alignment - 1) & !(alignment - 1)
}

fn resolve_symbol_pe(state: &LinkState, name: &str, from_sec: usize) -> Option<u64> {
    if let Some(def) = state.globals.get(name) {
        if def.section_global_idx == DYNAMIC_SYMBOL_SENTINEL {
            return None;
        }
        let sec = &state.sections[def.section_global_idx];
        return Some(sec.vaddr + def.value);
    }
    let obj_idx = state.sections[from_sec].obj_idx;
    if let Some(def) = state.locals.get(&(obj_idx, name.to_string())) {
        let sec = &state.sections[def.section_global_idx];
        return Some(sec.vaddr + def.value);
    }
    None
}

fn apply_relocs_pe(
    state: &mut LinkState,
    layout: &PeLayout,
) -> Result<Vec<u32>, Vec<String>> {
    let mut undefined = HashSet::new();
    let mut abs_fixups: Vec<u32> = Vec::new(); // RVAs of absolute 64-bit fixups
    let relocs: Vec<InputReloc> = state.relocs.clone();

    for reloc in &relocs {
        // Skip TLS relocations — not supported in UEFI
        match reloc.r_type {
            elf::R_X86_64_TLSGD | elf::R_X86_64_TLSLD | elf::R_X86_64_DTPOFF32
            | elf::R_X86_64_TPOFF32 | elf::R_X86_64_GOTTPOFF => continue,
            _ => {}
        }

        let sec = &state.sections[reloc.section_global_idx];
        let reloc_vaddr = sec.vaddr + reloc.offset;

        let sym_addr = match resolve_symbol_pe(state, &reloc.symbol_name, reloc.section_global_idx) {
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
            other => {
                eprintln!("toyos-ld: unsupported relocation type {other} in PE mode for symbol {}", reloc.symbol_name);
            }
        }
    }

    // Fill GOT entries
    for (sym_name, &got_vaddr) in &layout.got {
        let sym_addr = resolve_symbol_pe(state, sym_name, 0).unwrap_or(0);
        abs_fixups.push(got_vaddr as u32);
        // GOT entries written in emit_pe_bytes
        let _ = (got_vaddr, sym_addr);
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
        .map(|def| {
            let sec = &state.sections[def.section_global_idx];
            (sec.vaddr + def.value) as u32
        })
        .unwrap_or_else(|| panic!("toyos-ld: entry symbol '{entry_name}' not found"));

    // Build base relocation table
    let reloc_data = build_base_reloc_table(abs_fixups);
    let reloc_virt_size = reloc_data.len() as u32;
    let reloc_raw_size = pe_align_up(reloc_virt_size.max(1), PE_FILE_ALIGNMENT);

    let size_of_image = pe_align_up(
        layout.reloc_rva + reloc_virt_size.max(1),
        PE_SECTION_ALIGNMENT,
    );

    let num_sections: u32 = if layout.has_data { 3 } else { 2 };
    let total_file_size = layout.reloc_file_off + reloc_raw_size;

    let mut buf = vec![0u8; total_file_size as usize];

    // ── DOS header ──
    buf[0..2].copy_from_slice(&0x5A4Du16.to_le_bytes()); // e_magic = "MZ"
    buf[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes()); // e_lfanew

    // ── PE signature ──
    buf[0x40..0x44].copy_from_slice(&0x00004550u32.to_le_bytes()); // "PE\0\0"

    // ── COFF header ──
    let coff = &mut buf[0x44..0x58];
    coff[0..2].copy_from_slice(&0x8664u16.to_le_bytes()); // Machine = AMD64
    coff[2..4].copy_from_slice(&(num_sections as u16).to_le_bytes());
    // TimeDateStamp, PointerToSymbolTable, NumberOfSymbols = 0
    coff[16..18].copy_from_slice(&0x00F0u16.to_le_bytes()); // SizeOfOptionalHeader = 240
    coff[18..20].copy_from_slice(&0x0022u16.to_le_bytes()); // Characteristics

    // ── Optional header (PE32+) ──
    let oh = 0x58usize; // optional header start
    buf[oh..oh + 2].copy_from_slice(&0x020Bu16.to_le_bytes()); // Magic = PE32+
    // SizeOfCode
    buf[oh + 4..oh + 8].copy_from_slice(&layout.text_virt_size.to_le_bytes());
    // SizeOfInitializedData
    let init_data_size = layout.data_virt_size + reloc_virt_size;
    buf[oh + 8..oh + 12].copy_from_slice(&init_data_size.to_le_bytes());
    // AddressOfEntryPoint
    buf[oh + 0x10..oh + 0x14].copy_from_slice(&entry_rva.to_le_bytes());
    // BaseOfCode
    buf[oh + 0x14..oh + 0x18].copy_from_slice(&layout.text_rva.to_le_bytes());
    // ImageBase = 0 (UEFI will relocate)
    // SectionAlignment
    buf[oh + 0x20..oh + 0x24].copy_from_slice(&PE_SECTION_ALIGNMENT.to_le_bytes());
    // FileAlignment
    buf[oh + 0x24..oh + 0x28].copy_from_slice(&PE_FILE_ALIGNMENT.to_le_bytes());
    // SizeOfImage
    buf[oh + 0x38..oh + 0x3C].copy_from_slice(&size_of_image.to_le_bytes());
    // SizeOfHeaders
    buf[oh + 0x3C..oh + 0x40].copy_from_slice(&layout.size_of_headers.to_le_bytes());
    // Subsystem
    buf[oh + 0x44..oh + 0x46].copy_from_slice(&subsystem.to_le_bytes());
    // DllCharacteristics: DYNAMIC_BASE | HIGH_ENTROPY_VA | NX_COMPAT
    buf[oh + 0x46..oh + 0x48].copy_from_slice(&0x0160u16.to_le_bytes());
    // SizeOfStackReserve
    buf[oh + 0x48..oh + 0x50].copy_from_slice(&0x100000u64.to_le_bytes());
    // SizeOfStackCommit
    buf[oh + 0x50..oh + 0x58].copy_from_slice(&0x1000u64.to_le_bytes());
    // SizeOfHeapReserve
    buf[oh + 0x58..oh + 0x60].copy_from_slice(&0x100000u64.to_le_bytes());
    // SizeOfHeapCommit
    buf[oh + 0x60..oh + 0x68].copy_from_slice(&0x1000u64.to_le_bytes());
    // NumberOfRvaAndSizes = 16
    buf[oh + 0x6C..oh + 0x70].copy_from_slice(&16u32.to_le_bytes());

    // Data directory index 5: Base Relocation Table
    let dd5 = oh + 0x70 + 5 * 8; // each data dir entry is 8 bytes
    buf[dd5..dd5 + 4].copy_from_slice(&layout.reloc_rva.to_le_bytes());
    buf[dd5 + 4..dd5 + 8].copy_from_slice(&reloc_virt_size.to_le_bytes());

    // ── Section headers ──
    let sh_base = oh + 240; // after optional header

    // .text
    let sh = sh_base;
    buf[sh..sh + 8].copy_from_slice(b".text\0\0\0");
    buf[sh + 8..sh + 12].copy_from_slice(&layout.text_virt_size.to_le_bytes());
    buf[sh + 12..sh + 16].copy_from_slice(&layout.text_rva.to_le_bytes());
    buf[sh + 16..sh + 20].copy_from_slice(&layout.text_raw_size.to_le_bytes());
    buf[sh + 20..sh + 24].copy_from_slice(&layout.text_file_off.to_le_bytes());
    buf[sh + 36..sh + 40].copy_from_slice(&0x60000020u32.to_le_bytes()); // CODE|EXEC|READ

    let mut next_sh = sh_base + 40;

    // .data (if present)
    if layout.has_data {
        let sh = next_sh;
        buf[sh..sh + 8].copy_from_slice(b".data\0\0\0");
        buf[sh + 8..sh + 12].copy_from_slice(&layout.data_virt_size.to_le_bytes());
        buf[sh + 12..sh + 16].copy_from_slice(&layout.data_rva.to_le_bytes());
        buf[sh + 16..sh + 20].copy_from_slice(&layout.data_raw_size.to_le_bytes());
        buf[sh + 20..sh + 24].copy_from_slice(&layout.data_file_off.to_le_bytes());
        buf[sh + 36..sh + 40].copy_from_slice(&0xC0000040u32.to_le_bytes()); // INIT_DATA|READ|WRITE
        next_sh += 40;
    }

    // .reloc
    {
        let sh = next_sh;
        buf[sh..sh + 8].copy_from_slice(b".reloc\0\0");
        buf[sh + 8..sh + 12].copy_from_slice(&reloc_virt_size.to_le_bytes());
        buf[sh + 12..sh + 16].copy_from_slice(&layout.reloc_rva.to_le_bytes());
        buf[sh + 16..sh + 20].copy_from_slice(&reloc_raw_size.to_le_bytes());
        buf[sh + 20..sh + 24].copy_from_slice(&layout.reloc_file_off.to_le_bytes());
        buf[sh + 36..sh + 40].copy_from_slice(&0x42000040u32.to_le_bytes()); // INIT_DATA|DISCARDABLE|READ
    }

    // ── Copy section data ──
    for sec in &state.sections {
        if sec.vaddr == 0 || sec.data.is_empty() { continue; }
        // Determine which PE section this belongs to
        let rva = sec.vaddr as u32;
        let (file_off_base, rva_base) = if rva >= layout.data_rva && layout.has_data {
            (layout.data_file_off, layout.data_rva)
        } else {
            (layout.text_file_off, layout.text_rva)
        };
        let file_off = (file_off_base + (rva - rva_base)) as usize;
        if file_off + sec.data.len() <= buf.len() {
            buf[file_off..file_off + sec.data.len()].copy_from_slice(&sec.data);
        }
    }

    // ── Write GOT entries ──
    for (sym_name, &got_vaddr) in &layout.got {
        let sym_addr = resolve_symbol_pe(state, sym_name, 0).unwrap_or(0);
        let rva = got_vaddr as u32;
        let (file_off_base, rva_base) = if rva >= layout.data_rva && layout.has_data {
            (layout.data_file_off, layout.data_rva)
        } else {
            (layout.text_file_off, layout.text_rva)
        };
        let file_off = (file_off_base + (rva - rva_base)) as usize;
        if file_off + 8 <= buf.len() {
            buf[file_off..file_off + 8].copy_from_slice(&sym_addr.to_le_bytes());
        }
    }

    // ── Write .reloc data ──
    let reloc_off = layout.reloc_file_off as usize;
    if !reloc_data.is_empty() {
        buf[reloc_off..reloc_off + reloc_data.len()].copy_from_slice(&reloc_data);
    }

    buf
}

// ── Static ELF output (--static) ──────────────────────────────────────────

struct StaticLayout {
    base_addr: u64,
    rx_start: u64,
    rx_end: u64,
    rw_start: u64,
    rw_end: u64,
    tls_start: u64,
    tls_filesz: u64,
    tls_memsz: u64,
    got: HashMap<String, u64>,
}

fn layout_static(state: &mut LinkState, _entry_name: &str, base_addr: u64) -> StaticLayout {
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
    let rx_end = align_up(cursor, PAGE_SIZE);

    cursor = rx_end;
    let rw_start = cursor;
    for &idx in &rw_sections {
        let sec = &mut state.sections[idx];
        cursor = align_up(cursor, sec.align);
        sec.vaddr = cursor;
        cursor += sec.size;
    }

    // GOT entries (GOTPCREL* and GOTTPOFF)
    let mut got_symbols: Vec<String> = Vec::new();
    for reloc in &state.relocs {
        match reloc.r_type {
            elf::R_X86_64_GOTPCREL
            | elf::R_X86_64_GOTPCRELX
            | elf::R_X86_64_REX_GOTPCRELX
            | elf::R_X86_64_GOTTPOFF => {
                if !got_symbols.contains(&reloc.symbol_name) {
                    got_symbols.push(reloc.symbol_name.clone());
                }
            }
            _ => {}
        }
    }

    cursor = align_up(cursor, 8);
    let mut got = HashMap::new();
    for sym in &got_symbols {
        got.insert(sym.clone(), cursor);
        cursor += 8;
    }

    let rw_end = align_up(cursor, PAGE_SIZE);

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

    StaticLayout {
        base_addr,
        rx_start,
        rx_end,
        rw_start,
        rw_end,
        tls_start,
        tls_filesz,
        tls_memsz,
        got,
    }
}

fn resolve_symbol_static(state: &LinkState, name: &str, from_sec: usize) -> Option<u64> {
    if let Some(def) = state.globals.get(name) {
        if def.section_global_idx == DYNAMIC_SYMBOL_SENTINEL {
            return None; // Static linking doesn't support dynamic symbols
        }
        let sec = &state.sections[def.section_global_idx];
        return Some(sec.vaddr + def.value);
    }
    let obj_idx = state.sections[from_sec].obj_idx;
    if let Some(def) = state.locals.get(&(obj_idx, name.to_string())) {
        let sec = &state.sections[def.section_global_idx];
        return Some(sec.vaddr + def.value);
    }
    None
}

fn tpoff_static(sym_addr: u64, layout: &StaticLayout) -> i64 {
    sym_addr as i64 - (layout.tls_start as i64 + layout.tls_memsz as i64)
}

fn apply_relocs_static(
    state: &mut LinkState,
    layout: &StaticLayout,
) -> Result<(), Vec<String>> {
    let mut undefined = HashSet::new();
    let relocs: Vec<InputReloc> = state.relocs.clone();

    // Pass 1: TLS GD/LD/DTPOFF relaxations
    let mut relaxed_calls: HashSet<(usize, u64)> = HashSet::new();

    for reloc in &relocs {
        match reloc.r_type {
            elf::R_X86_64_TLSGD => {
                let sym_addr = resolve_symbol_static(state, &reloc.symbol_name, reloc.section_global_idx)
                    .unwrap_or(0);
                let padded = is_padded_tls_sequence(
                    &state.sections[reloc.section_global_idx].data,
                    reloc.offset,
                );
                if padded {
                    #[rustfmt::skip]
                    let inst: [u8; 16] = [
                        0x64, 0x48, 0x8b, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00,
                        0x48, 0x8d, 0x80, 0x00, 0x00, 0x00, 0x00,
                    ];
                    write_bytes(state, reloc.section_global_idx, reloc.offset - 4, &inst);
                    write_i32(state, reloc.section_global_idx, reloc.offset + 8,
                        tpoff_static(sym_addr, layout) as i32);
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
                    #[rustfmt::skip]
                    let inst: [u8; 16] = [
                        0x66, 0x66, 0x66,
                        0x64, 0x48, 0x8b, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00,
                        0x0f, 0x1f, 0x40, 0x00,
                    ];
                    write_bytes(state, reloc.section_global_idx, reloc.offset - 4, &inst);
                    relaxed_calls.insert((reloc.section_global_idx, reloc.offset + 8));
                } else {
                    #[rustfmt::skip]
                    let inst: [u8; 12] = [
                        0x66, 0x66, 0x66,
                        0x64, 0x48, 0x8b, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00,
                    ];
                    write_bytes(state, reloc.section_global_idx, reloc.offset - 3, &inst);
                    relaxed_calls.insert((reloc.section_global_idx, reloc.offset + 5));
                }
            }
            elf::R_X86_64_DTPOFF32 => {
                let sym_addr = resolve_symbol_static(state, &reloc.symbol_name, reloc.section_global_idx)
                    .unwrap_or(0);
                write_i32(state, reloc.section_global_idx, reloc.offset,
                    (tpoff_static(sym_addr, layout) + reloc.addend) as i32);
            }
            _ => {}
        }
    }

    // Pass 2: all other relocations — directly patch, no RELATIVE entries
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

        let sym_addr = match resolve_symbol_static(state, &reloc.symbol_name, reloc.section_global_idx) {
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
                // No RELATIVE — address is absolute and fixed at link time
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
                let got_slot = layout.got[&reloc.symbol_name];
                let value = got_slot as i64 + reloc.addend - reloc_vaddr as i64;
                write_i32(state, reloc.section_global_idx, reloc.offset, value as i32);
            }
            elf::R_X86_64_TPOFF32 => {
                let tp = tpoff_static(sym_addr, layout);
                write_i32(state, reloc.section_global_idx, reloc.offset,
                    (tp + reloc.addend) as i32);
            }
            elf::R_X86_64_GOTTPOFF => {
                let got_slot = layout.got[&reloc.symbol_name];
                let value = got_slot as i64 + reloc.addend - reloc_vaddr as i64;
                write_i32(state, reloc.section_global_idx, reloc.offset, value as i32);
            }
            other => {
                eprintln!(
                    "toyos-ld: unsupported relocation type {other} for symbol {}",
                    reloc.symbol_name
                );
            }
        }
    }

    // Fill GOT entries with absolute addresses
    let gottpoff_syms: HashSet<String> = relocs
        .iter()
        .filter(|r| r.r_type == elf::R_X86_64_GOTTPOFF)
        .map(|r| r.symbol_name.clone())
        .collect();

    for (sym_name, &got_vaddr) in &layout.got {
        let sym_addr = resolve_symbol_static(state, sym_name, 0).unwrap_or(0);
        let value = if gottpoff_syms.contains(sym_name) {
            tpoff_static(sym_addr, layout) as u64
        } else {
            sym_addr
        };
        // Write GOT entry directly into the RW section data
        // GOT is at got_vaddr, which is in the RW segment
        // Find which section contains this address, or we need to handle it differently
        // For static linking, GOT entries are just absolute values baked in
        // We handle this by writing to the output buffer in emit_static_bytes
        // Store in a side table instead
        let _ = (got_vaddr, value); // handled in emit
    }

    if !undefined.is_empty() {
        let mut syms: Vec<String> = undefined.into_iter().collect();
        syms.sort();
        return Err(syms);
    }

    Ok(())
}

fn emit_static_bytes(
    state: &LinkState,
    layout: &StaticLayout,
    entry_name: &str,
) -> Vec<u8> {
    let entry = state
        .globals
        .get(entry_name)
        .map(|def| {
            state.sections[def.section_global_idx].vaddr + def.value
        })
        .unwrap_or_else(|| {
            panic!("toyos-ld: entry symbol '{entry_name}' not found");
        });

    let after_rw = layout.rw_end.max(layout.tls_start + layout.tls_memsz);

    // shstrtab
    let shstrtab = build_static_shstrtab();
    let shstrtab_file_offset = after_rw - layout.base_addr;
    let shstrtab_size = shstrtab.len() as u64;

    let num_shdrs: u16 = 4; // NULL + .text + .data + .shstrtab
    let mut phdr_count = 2u16; // PT_LOAD×2 (RX + RW)
    if layout.tls_memsz > 0 { phdr_count += 1; }
    let shdr_offset = align_up(shstrtab_file_offset + shstrtab_size, 8);
    let total_file_size = shdr_offset + num_shdrs as u64 * 64;

    let mut buf = vec![0u8; total_file_size as usize];

    // ── ELF header ──
    let ehdr = &mut buf[..64];
    ehdr[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    ehdr[4] = 2; // ELFCLASS64
    ehdr[5] = 1; // ELFDATA2LSB
    ehdr[6] = 1; // EV_CURRENT
    ehdr[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    ehdr[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    ehdr[20..24].copy_from_slice(&1u32.to_le_bytes()); // EV_CURRENT
    ehdr[24..32].copy_from_slice(&entry.to_le_bytes());
    ehdr[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    ehdr[40..48].copy_from_slice(&shdr_offset.to_le_bytes());
    ehdr[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    ehdr[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    ehdr[56..58].copy_from_slice(&phdr_count.to_le_bytes());
    ehdr[58..60].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
    ehdr[60..62].copy_from_slice(&num_shdrs.to_le_bytes());
    ehdr[62..64].copy_from_slice(&(num_shdrs - 1).to_le_bytes()); // e_shstrndx

    // ── Program headers ──
    // File layout mirrors virtual layout: file_offset = vaddr - base_addr
    let base = layout.base_addr;
    let mut ph = 64usize;
    write_phdr(&mut buf[ph..], elf::PT_LOAD, elf::PF_R | elf::PF_X,
        base, base,
        layout.rx_end - base, layout.rx_end - base, PAGE_SIZE);
    // Fix p_offset for the phdr we just wrote (write_phdr uses BASE_VADDR)
    buf[ph + 8..ph + 16].copy_from_slice(&0u64.to_le_bytes()); // RX starts at file offset 0
    ph += 56;
    let rw_file_off = layout.rw_start - base;
    write_phdr(&mut buf[ph..], elf::PT_LOAD, elf::PF_R | elf::PF_W,
        layout.rw_start, layout.rw_start,
        layout.rw_end - layout.rw_start, layout.rw_end - layout.rw_start, PAGE_SIZE);
    buf[ph + 8..ph + 16].copy_from_slice(&rw_file_off.to_le_bytes());
    ph += 56;
    if layout.tls_memsz > 0 {
        let tls_file_off = layout.tls_start - base;
        write_phdr(&mut buf[ph..], elf::PT_TLS, elf::PF_R,
            layout.tls_start, layout.tls_start,
            layout.tls_filesz, layout.tls_memsz, 64);
        buf[ph + 8..ph + 16].copy_from_slice(&tls_file_off.to_le_bytes());
    }

    // ── Copy section data ──
    for sec in &state.sections {
        if sec.vaddr == 0 || sec.data.is_empty() { continue; }
        let file_off = (sec.vaddr - layout.base_addr) as usize;
        if file_off + sec.data.len() <= buf.len() {
            buf[file_off..file_off + sec.data.len()].copy_from_slice(&sec.data);
        }
    }

    // ── Write GOT entries ──
    let gottpoff_syms: HashSet<String> = state.relocs
        .iter()
        .filter(|r| r.r_type == elf::R_X86_64_GOTTPOFF)
        .map(|r| r.symbol_name.clone())
        .collect();
    for (sym_name, &got_vaddr) in &layout.got {
        let sym_addr = resolve_symbol_static(state, sym_name, 0).unwrap_or(0);
        let value = if gottpoff_syms.contains(sym_name) {
            tpoff_static(sym_addr, layout) as u64
        } else {
            sym_addr
        };
        let file_off = (got_vaddr - layout.base_addr) as usize;
        if file_off + 8 <= buf.len() {
            buf[file_off..file_off + 8].copy_from_slice(&value.to_le_bytes());
        }
    }

    // ── Write .shstrtab ──
    let shstrtab_off = shstrtab_file_offset as usize;
    buf[shstrtab_off..shstrtab_off + shstrtab.len()].copy_from_slice(&shstrtab);

    // ── Section headers ──
    let sh = shdr_offset as usize;
    // 0: NULL (already zeroed)
    // 1: .text
    write_shdr(&mut buf[sh + 64..], 1, elf::SHT_PROGBITS,
        (elf::SHF_ALLOC | elf::SHF_EXECINSTR) as u64,
        layout.rx_start, layout.rx_start - layout.base_addr,
        layout.rx_end - layout.rx_start, 0, 0, 16, 0);
    // 2: .data
    write_shdr(&mut buf[sh + 128..], 7, elf::SHT_PROGBITS,
        (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
        layout.rw_start, layout.rw_start - layout.base_addr,
        layout.rw_end - layout.rw_start, 0, 0, 8, 0);
    // 3: .shstrtab
    write_shdr(&mut buf[sh + 192..], 13, elf::SHT_STRTAB,
        0, 0, shstrtab_file_offset, shstrtab_size, 0, 0, 1, 0);

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
    let layout = layout(&mut state, None);
    let reloc_output = apply_relocs(&mut state, &layout, true)?;
    Ok(emit_shared_bytes(&state, &layout, &reloc_output))
}

fn build_dynsym(state: &LinkState) -> (Vec<u8>, Vec<u8>) {
    let mut dynsym = vec![0u8; 24]; // null Elf64_Sym
    let mut dynstr = vec![0u8]; // leading null byte

    let mut symbols: Vec<_> = state.globals.iter().collect();
    symbols.sort_by_key(|(name, _)| *name);

    for (name, def) in symbols {
        if def.section_global_idx == DYNAMIC_SYMBOL_SENTINEL {
            continue;
        }
        let st_name = dynstr.len() as u32;
        dynstr.extend_from_slice(name.as_bytes());
        dynstr.push(0);

        let sec = &state.sections[def.section_global_idx];
        let st_value = sec.vaddr + def.value;

        let mut sym = [0u8; 24];
        sym[0..4].copy_from_slice(&st_name.to_le_bytes());
        sym[4] = (elf::STB_GLOBAL << 4) | elf::STT_NOTYPE;
        sym[6..8].copy_from_slice(&1u16.to_le_bytes()); // st_shndx != 0 → defined
        sym[8..16].copy_from_slice(&st_value.to_le_bytes());
        dynsym.extend_from_slice(&sym);
    }

    (dynsym, dynstr)
}

fn build_dynamic(symtab_vaddr: u64, strtab_vaddr: u64, strsz: u64) -> Vec<u8> {
    let entries: &[(i64, u64)] = &[
        (elf::DT_SYMTAB.into(), symtab_vaddr),
        (elf::DT_STRTAB.into(), strtab_vaddr),
        (elf::DT_STRSZ.into(), strsz),
        (elf::DT_SYMENT.into(), 24),
        (elf::DT_NULL.into(), 0),
    ];
    let mut data = Vec::with_capacity(entries.len() * 16);
    for &(tag, val) in entries {
        data.extend_from_slice(&tag.to_le_bytes());
        data.extend_from_slice(&val.to_le_bytes());
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
    layout: &LayoutResult,
    relocs: &RelocOutput,
) -> Vec<u8> {
    let (dynsym_data, dynstr_data) = build_dynsym(state);

    // Dynamic sections go after RW/TLS, in a third PT_LOAD segment
    let after_rw = layout.rw_end.max(layout.tls_start + layout.tls_memsz);
    let dynsym_vaddr = align_up(after_rw, 8);
    let dynstr_vaddr = dynsym_vaddr + dynsym_data.len() as u64;
    let dynamic_vaddr = align_up(dynstr_vaddr + dynstr_data.len() as u64, 8);
    let dynamic_data = build_dynamic(dynsym_vaddr, dynstr_vaddr, dynstr_data.len() as u64);
    let dyn_segment_end = align_up(dynamic_vaddr + dynamic_data.len() as u64, PAGE_SIZE);

    // Metadata (not loaded)
    let rela_dyn_offset = dyn_segment_end;
    let rela_dyn_size = relocs.relatives.len() as u64 * 24;

    // Metadata sections (e.g. .rustc) — placed after rela.dyn, before shstrtab
    let mut meta_offset = rela_dyn_offset + rela_dyn_size;
    let mut meta_offsets = Vec::new();
    for (_, data) in &state.metadata {
        meta_offset = align_up(meta_offset, 8);
        meta_offsets.push(meta_offset);
        meta_offset += data.len() as u64;
    }

    let shstrtab_offset = align_up(meta_offset, 1);
    let (shstrtab, meta_name_offsets) = build_shared_shstrtab(&state.metadata);
    let shstrtab_size = shstrtab.len() as u64;

    // 8 base sections + metadata sections
    let num_shdrs = 8u16 + state.metadata.len() as u16;
    let shdr_offset = align_up(shstrtab_offset + shstrtab_size, 8);
    let total_size = shdr_offset + num_shdrs as u64 * 64;

    let mut buf = vec![0u8; total_size as usize];

    // ── ELF header ──
    let ehdr = &mut buf[..64];
    ehdr[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    ehdr[4] = 2; // ELFCLASS64
    ehdr[5] = 1; // ELFDATA2LSB
    ehdr[6] = 1; // EV_CURRENT
    ehdr[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    ehdr[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    ehdr[20..24].copy_from_slice(&1u32.to_le_bytes()); // EV_CURRENT
    ehdr[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    ehdr[40..48].copy_from_slice(&shdr_offset.to_le_bytes());
    ehdr[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    ehdr[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    let mut phdr_count = 4u16; // PT_LOAD×3 + PT_DYNAMIC
    if layout.tls_memsz > 0 { phdr_count += 1; }
    ehdr[56..58].copy_from_slice(&phdr_count.to_le_bytes());
    ehdr[58..60].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
    ehdr[60..62].copy_from_slice(&num_shdrs.to_le_bytes());
    ehdr[62..64].copy_from_slice(&(num_shdrs - 1).to_le_bytes()); // e_shstrndx

    // ── Program headers ──
    let mut ph = 64usize;
    write_phdr(&mut buf[ph..], elf::PT_LOAD, elf::PF_R | elf::PF_X,
        BASE_VADDR, BASE_VADDR,
        layout.rx_end - BASE_VADDR, layout.rx_end - BASE_VADDR, PAGE_SIZE);
    ph += 56;
    write_phdr(&mut buf[ph..], elf::PT_LOAD, elf::PF_R | elf::PF_W,
        layout.rw_start, layout.rw_start,
        layout.rw_end - layout.rw_start, layout.rw_end - layout.rw_start, PAGE_SIZE);
    ph += 56;
    write_phdr(&mut buf[ph..], elf::PT_LOAD, elf::PF_R,
        dynsym_vaddr, dynsym_vaddr,
        dyn_segment_end - dynsym_vaddr, dyn_segment_end - dynsym_vaddr, PAGE_SIZE);
    ph += 56;
    write_phdr(&mut buf[ph..], elf::PT_DYNAMIC, elf::PF_R,
        dynamic_vaddr, dynamic_vaddr,
        dynamic_data.len() as u64, dynamic_data.len() as u64, 8);
    ph += 56;
    if layout.tls_memsz > 0 {
        write_phdr(&mut buf[ph..], elf::PT_TLS, elf::PF_R,
            layout.tls_start, layout.tls_start,
            layout.tls_filesz, layout.tls_memsz, 64);
    }

    // ── Section data ──
    for sec in &state.sections {
        if sec.vaddr == 0 || sec.data.is_empty() { continue; }
        let off = (sec.vaddr - BASE_VADDR) as usize;
        buf[off..off + sec.data.len()].copy_from_slice(&sec.data);
    }

    // Dynamic sections (in PT_LOAD #3)
    let off = (dynsym_vaddr - BASE_VADDR) as usize;
    buf[off..off + dynsym_data.len()].copy_from_slice(&dynsym_data);
    let off = (dynstr_vaddr - BASE_VADDR) as usize;
    buf[off..off + dynstr_data.len()].copy_from_slice(&dynstr_data);
    let off = (dynamic_vaddr - BASE_VADDR) as usize;
    buf[off..off + dynamic_data.len()].copy_from_slice(&dynamic_data);

    // .rela.dyn
    let rela_off = rela_dyn_offset as usize;
    for (i, &(offset, addend)) in relocs.relatives.iter().enumerate() {
        let base = rela_off + i * 24;
        buf[base..base + 8].copy_from_slice(&offset.to_le_bytes());
        buf[base + 8..base + 16].copy_from_slice(&8u64.to_le_bytes()); // R_X86_64_RELATIVE
        buf[base + 16..base + 24].copy_from_slice(&addend.to_le_bytes());
    }

    // Metadata sections
    for (i, (_, data)) in state.metadata.iter().enumerate() {
        let off = meta_offsets[i] as usize;
        buf[off..off + data.len()].copy_from_slice(data);
    }

    // .shstrtab
    let off = shstrtab_offset as usize;
    buf[off..off + shstrtab.len()].copy_from_slice(&shstrtab);

    // ── Section headers ──
    let sh = shdr_offset as usize;
    // 1: .text
    write_shdr(&mut buf[sh + 64..], 1, elf::SHT_PROGBITS,
        (elf::SHF_ALLOC | elf::SHF_EXECINSTR) as u64,
        layout.rx_start, layout.rx_start - BASE_VADDR,
        layout.rx_end - layout.rx_start, 0, 0, 16, 0);
    // 2: .data
    write_shdr(&mut buf[sh + 128..], 7, elf::SHT_PROGBITS,
        (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
        layout.rw_start, layout.rw_start - BASE_VADDR,
        layout.rw_end - layout.rw_start, 0, 0, 8, 0);
    // 3: .rela.dyn
    write_shdr(&mut buf[sh + 192..], 13, elf::SHT_RELA,
        elf::SHF_ALLOC as u64, 0, rela_dyn_offset, rela_dyn_size, 0, 0, 8, 24);
    // 4: .dynsym  (sh_link=5 → .dynstr, sh_info=1 → first global sym index)
    write_shdr(&mut buf[sh + 256..], 23, elf::SHT_DYNSYM,
        elf::SHF_ALLOC as u64, dynsym_vaddr, dynsym_vaddr - BASE_VADDR,
        dynsym_data.len() as u64, 5, 1, 8, 24);
    // 5: .dynstr
    write_shdr(&mut buf[sh + 320..], 31, elf::SHT_STRTAB,
        elf::SHF_ALLOC as u64, dynstr_vaddr, dynstr_vaddr - BASE_VADDR,
        dynstr_data.len() as u64, 0, 0, 1, 0);
    // 6: .dynamic  (sh_link=5 → .dynstr)
    write_shdr(&mut buf[sh + 384..], 39, elf::SHT_DYNAMIC,
        (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
        dynamic_vaddr, dynamic_vaddr - BASE_VADDR,
        dynamic_data.len() as u64, 5, 0, 8, 16);
    // 7+: Metadata sections (e.g. .rustc) — non-loadable
    for (i, (_, data)) in state.metadata.iter().enumerate() {
        let shdr_idx = 7 + i; // after NULL(0) + .text(1) + .data(2) + .rela.dyn(3) + .dynsym(4) + .dynstr(5) + .dynamic(6)
        write_shdr(&mut buf[sh + shdr_idx * 64..], meta_name_offsets[i],
            elf::SHT_PROGBITS, 0, 0, meta_offsets[i],
            data.len() as u64, 0, 0, 8, 0);
    }
    // .shstrtab (always last section, index = num_shdrs - 1)
    let shstrtab_shdr_idx = (num_shdrs - 1) as usize;
    write_shdr(&mut buf[sh + shstrtab_shdr_idx * 64..], 48, elf::SHT_STRTAB,
        0, 0, shstrtab_offset, shstrtab_size, 0, 0, 1, 0);

    buf
}

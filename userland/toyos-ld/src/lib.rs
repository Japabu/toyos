//! toyos-ld: Minimal static ELF linker for ToyOS.
//!
//! Produces PIE (ET_DYN) x86-64 ELF executables with only R_X86_64_RELATIVE
//! relocations, which is what ToyOS's kernel ELF loader expects.
//!
//! Supports .o object files and .rlib/.a archives (ar format).

use object::elf;
use object::read::elf::ElfFile64;
use object::read::{self, Object, ObjectSection, ObjectSymbol};
use object::RelocationFlags;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::{fs, process};

const BASE_VADDR: u64 = 0x200000;
const PAGE_SIZE: u64 = 0x1000;

// ── Public API ──────────────────────────────────────────────────────────

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

#[derive(Clone)]
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
        let elf: ElfFile64 = match ElfFile64::parse(data.as_slice()) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("toyos-ld: cannot parse {name}: {e}");
                process::exit(1);
            }
        };

        // Shared library input: extract dynamic symbols, skip section processing
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

        for section in elf.sections() {
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

        for symbol in elf.symbols() {
            let sym_name = match symbol.name() {
                Ok(n) if !n.is_empty() => n.to_string(),
                _ => continue,
            };
            if symbol.is_undefined() {
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
                // Concrete .o definitions always override .so dynamic imports
                match state.globals.get(&sym_name) {
                    Some(existing) if existing.section_global_idx != DYNAMIC_SYMBOL_SENTINEL => {}
                    _ => { state.globals.insert(sym_name, def); }
                }
            } else {
                state.locals.insert((obj_idx, sym_name), def);
            }
        }

        for section in elf.sections() {
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
                        match elf.symbol_by_index(sym_idx) {
                            Ok(s) => {
                                let name = s.name().unwrap_or("");
                                if name.is_empty() {
                                    // Section symbol — create synthetic name for lookup
                                    if let read::SymbolSection::Section(si) = s.section() {
                                        if let Some(&gsec) = sec_map.get(&(obj_idx, si)) {
                                            let syn =
                                                format!("__section_sym_{}_{}", obj_idx, gsec);
                                            state
                                                .locals
                                                .entry((obj_idx, syn.clone()))
                                                .or_insert(SymbolDef {
                                                    section_global_idx: gsec,
                                                    value: 0,
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
                    _ => continue,
                };

                state.relocs.push(InputReloc {
                    section_global_idx: global_sec,
                    offset,
                    r_type,
                    symbol_name: sym_name,
                    addend: reloc.addend(),
                });
            }
        }
    }

    state
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
        || name.starts_with(".eh_frame")
        || name == ".gcc_except_table"
        || name.starts_with(".data.rel.ro")
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

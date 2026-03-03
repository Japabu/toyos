//! toyos-ld: Minimal linker for ToyOS.
//!
//! Reads ELF and COFF object files. Produces PIE ELF, static ELF, or PE32+.
//! Supports .o object files and .rlib/.a archives (ar format).

mod collect;
mod reloc;
mod emit_elf;
mod emit_pe;
mod emit_macho;

use std::collections::HashMap;
use std::path::PathBuf;
use std::fs;

use collect::{collect, synthesize_alloc_shims, gc_sections, merge_string_sections, is_archive, extract_archive, find_lib, scan_symbols};
use reloc::{ElfRelocParams, apply_relocs, apply_relocs_pe, MachORelocParams, apply_relocs_macho};
use emit_elf::{layout_elf, build_eh_frame_hdr, emit_bytes, emit_static_bytes, emit_shared_bytes};
use emit_pe::{layout_pe, emit_pe_bytes};
use emit_macho::{layout_macho, emit_macho_bytes};

pub(crate) const BASE_VADDR: u64 = 0;
pub(crate) const PAGE_SIZE: u64 = 0x1000;

// ── Error type ──────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum LinkError {
    UndefinedSymbols(Vec<String>),
    Parse { file: String, message: String },
    UnsupportedRelocation { reloc_type: u32, symbol: String },
    RelocationOverflow { reloc_type: u32, symbol: String, value: i64 },
    MissingEntry(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::UndefinedSymbols(syms) => {
                for sym in syms {
                    writeln!(f, "undefined symbol: {sym}")?;
                }
                Ok(())
            }
            LinkError::Parse { file, message } => {
                write!(f, "cannot parse {file}: {message}")
            }
            LinkError::UnsupportedRelocation { reloc_type, symbol } => {
                write!(f, "unsupported relocation type {reloc_type} for symbol {symbol}")
            }
            LinkError::RelocationOverflow { reloc_type, symbol, value } => {
                write!(f, "relocation overflow: type {reloc_type} for symbol {symbol} value {value:#x}")
            }
            LinkError::MissingEntry(name) => {
                write!(f, "entry symbol '{name}' not found")
            }
        }
    }
}

impl std::error::Error for LinkError {}

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
) -> Result<Vec<u8>, LinkError> {
    link_pe_with(objects, entry, subsystem, false)
}

pub fn link_pe_with(
    objects: &[(String, Vec<u8>)],
    entry: &str,
    subsystem: u16,
    gc: bool,
) -> Result<Vec<u8>, LinkError> {
    let mut state = collect(objects)?;
    synthesize_alloc_shims(&mut state);
    merge_string_sections(&mut state);
    if gc { gc_sections(&mut state, entry); }
    let pe_layout = layout_pe(&mut state);
    let base_relocs = apply_relocs_pe(&mut state, &pe_layout)?;
    emit_pe_bytes(&state, &pe_layout, entry, subsystem, &base_relocs)
}

/// Link object files and produce a static ELF executable (ET_EXEC).
/// Used for bare-metal targets like x86_64-unknown-none (kernel).
/// `base_addr` sets the load address (e.g. 0xFFFF800000000000 for kernel code model).
/// Returns the raw ELF bytes on success, or a list of undefined symbols on failure.
pub fn link_static(
    objects: &[(String, Vec<u8>)],
    entry: &str,
    base_addr: u64,
) -> Result<Vec<u8>, LinkError> {
    link_static_with(objects, entry, base_addr, false)
}

pub fn link_static_with(
    objects: &[(String, Vec<u8>)],
    entry: &str,
    base_addr: u64,
    gc: bool,
) -> Result<Vec<u8>, LinkError> {
    link_static_full(objects, entry, base_addr, gc, false)
}

pub fn link_static_full(
    objects: &[(String, Vec<u8>)],
    entry: &str,
    base_addr: u64,
    gc: bool,
    build_id: bool,
) -> Result<Vec<u8>, LinkError> {
    let mut state = collect(objects)?;
    synthesize_alloc_shims(&mut state);
    merge_string_sections(&mut state);
    if gc { gc_sections(&mut state, entry); }
    let layout = layout_elf(&mut state, base_addr, None, build_id);
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
    emit_static_bytes(&state, &layout, entry)
}

/// Link object files and produce a PIE ELF executable.
/// Returns the raw ELF bytes on success, or a list of undefined symbols on failure.
pub fn link(objects: &[(String, Vec<u8>)], entry: &str) -> Result<Vec<u8>, LinkError> {
    link_with(objects, entry, false)
}

pub fn link_with(
    objects: &[(String, Vec<u8>)],
    entry: &str,
    gc: bool,
) -> Result<Vec<u8>, LinkError> {
    link_full(objects, entry, gc, false)
}

pub fn link_full(
    objects: &[(String, Vec<u8>)],
    entry: &str,
    gc: bool,
    build_id: bool,
) -> Result<Vec<u8>, LinkError> {
    let mut state = collect(objects)?;
    synthesize_alloc_shims(&mut state);
    merge_string_sections(&mut state);
    if gc { gc_sections(&mut state, entry); }
    let layout = layout_elf(&mut state, BASE_VADDR, Some(entry), build_id);
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
    let eh_hdr = build_eh_frame_hdr(&state, &layout);
    emit_bytes(&state, &layout, &reloc_output, entry, &eh_hdr)
}

/// Resolve library names (-l flags) against search paths (-L flags),
/// reading and extracting archives. Only includes archive members that
/// define symbols needed by already-included objects (transitive pull-in).
pub fn resolve_libs(
    inputs: &[PathBuf],
    lib_paths: &[PathBuf],
    libs: &[String],
) -> Result<Vec<(String, Vec<u8>)>, LinkError> {
    use std::collections::HashSet;

    let mut objects: Vec<(String, Vec<u8>)> = Vec::new();
    // Archive members available for pull-in: (archive_name, member_name, data)
    let mut archive_members: Vec<(String, Vec<u8>)> = Vec::new();

    // Collect direct inputs and archive members
    for path in inputs {
        let data = fs::read(path)
            .map_err(|e| LinkError::Parse { file: path.display().to_string(), message: e.to_string() })?;
        if is_archive(&data) {
            extract_archive(&path.display().to_string(), &data, &mut archive_members)?;
        } else {
            objects.push((path.display().to_string(), data));
        }
    }

    for lib in libs {
        let (name, data) = find_lib(lib, lib_paths)
            .ok_or_else(|| LinkError::Parse { file: format!("-l{lib}"), message: "library not found".to_string() })?;
        if is_archive(&data) {
            extract_archive(&name, &data, &mut archive_members)?;
        } else {
            objects.push((name, data));
        }
    }

    // Scan direct objects for defined/referenced symbols
    let mut defined = HashSet::new();
    let mut undefined = HashSet::new();
    for (_, data) in &objects {
        let (defs, refs) = scan_symbols(data);
        defined.extend(defs);
        undefined.extend(refs);
    }
    // Only truly undefined: referenced but not yet defined
    undefined.retain(|s| !defined.contains(s));

    // Build index: for each archive member, what symbols does it define?
    let mut member_defs: Vec<HashSet<String>> = Vec::with_capacity(archive_members.len());
    let mut member_refs: Vec<HashSet<String>> = Vec::with_capacity(archive_members.len());
    for (_, data) in &archive_members {
        let (defs, refs) = scan_symbols(data);
        member_defs.push(defs);
        member_refs.push(refs);
    }

    // Iteratively pull in archive members that satisfy undefined symbols
    let mut included = vec![false; archive_members.len()];
    loop {
        let mut changed = false;
        for i in 0..archive_members.len() {
            if included[i] { continue; }
            if member_defs[i].iter().any(|sym| undefined.contains(sym)) {
                included[i] = true;
                changed = true;
                defined.extend(member_defs[i].iter().cloned());
                undefined.extend(member_refs[i].iter().cloned());
                undefined.retain(|s| !defined.contains(s));
            }
        }
        if !changed { break; }
    }

    // Collect the selected archive members in order
    for (i, (name, data)) in archive_members.into_iter().enumerate() {
        if included[i] {
            objects.push((name, data));
        }
    }

    Ok(objects)
}

/// Link object files and produce a shared library (.so) ELF with .dynsym/.dynstr.
pub fn link_shared(objects: &[(String, Vec<u8>)]) -> Result<Vec<u8>, LinkError> {
    link_shared_full(objects, false)
}

pub fn link_shared_full(objects: &[(String, Vec<u8>)], build_id: bool) -> Result<Vec<u8>, LinkError> {
    let mut state = collect(objects)?;
    synthesize_alloc_shims(&mut state);
    merge_string_sections(&mut state);
    let layout = layout_elf(&mut state, BASE_VADDR, None, build_id);
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
    let eh_hdr = build_eh_frame_hdr(&state, &layout);
    Ok(emit_shared_bytes(&state, &layout, &reloc_output, &eh_hdr))
}

/// Link object files and produce a Mach-O executable for macOS.
/// Undefined symbols are resolved against /usr/lib/libSystem.B.dylib at runtime.
pub fn link_macho(
    objects: &[(String, Vec<u8>)],
    entry: &str,
    gc: bool,
) -> Result<Vec<u8>, LinkError> {
    let mut state = collect(objects)?;
    synthesize_alloc_shims(&mut state);
    merge_string_sections(&mut state);

    // Mark any truly undefined symbols as dynamic (dylib) imports
    {
        use std::collections::HashSet;
        let referenced: HashSet<String> = state.relocs.iter()
            .map(|r| r.symbol_name.clone())
            .collect();
        let undefined: Vec<String> = referenced.into_iter()
            .filter(|sym| {
                !state.globals.contains_key(sym)
                    && !state.locals.keys().any(|(_, n)| n == sym)
            })
            .collect();
        for sym in undefined {
            state.globals.insert(sym, collect::SymbolDef {
                section_global_idx: collect::DYNAMIC_SYMBOL_SENTINEL,
                value: 0,
            });
        }
    }

    if gc { gc_sections(&mut state, entry); }
    let layout = layout_macho(&mut state, entry);

    // Apply relocations
    let params = MachORelocParams { got: &layout.got };
    let reloc_output = apply_relocs_macho(&mut state, &params)?;

    // Build bind entries for external GOT slots
    let bind_entries: Vec<(String, u64)> = layout.got_entries.iter()
        .filter(|(_, ext)| *ext)
        .map(|(name, _)| (name.clone(), layout.got[name]))
        .collect();

    // Rebase entries: internal absolute pointers + internal GOT entries
    let mut rebase_entries = reloc_output.rebase_entries;
    for (name, ext) in &layout.got_entries {
        if !ext {
            rebase_entries.push((layout.got[name], 0)); // value already written
        }
    }

    emit_macho_bytes(&state, &layout, entry, &rebase_entries, &bind_entries)
}

// ── Shared helpers ──────────────────────────────────────────────────────

pub(crate) fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

pub(crate) fn is_tls_section(name: &str) -> bool {
    name.starts_with(".tdata") || name.starts_with(".tbss")
}

pub(crate) struct SectionBuckets {
    pub(crate) rx: Vec<usize>,
    pub(crate) rw: Vec<usize>,
    pub(crate) tls: Vec<usize>,
}

pub(crate) fn classify_sections(state: &mut collect::LinkState) -> SectionBuckets {
    let mut buckets = SectionBuckets { rx: Vec::new(), rw: Vec::new(), tls: Vec::new() };
    for (idx, sec) in state.sections.iter().enumerate() {
        if state.tls_sections.contains(&idx) || is_tls_section(&sec.name) {
            buckets.tls.push(idx);
            if !state.tls_sections.contains(&idx) {
                state.tls_sections.push(idx);
            }
        } else if sec.writable {
            buckets.rw.push(idx);
        } else {
            buckets.rx.push(idx);
        }
    }
    // Sort RX: .eh_frame at end (grouped for .eh_frame_hdr generation)
    buckets.rx.sort_by_key(|&idx| if state.sections[idx].name == ".eh_frame" { 1u8 } else { 0 });
    // Sort RW: .init_array first, .fini_array second, other PROGBITS, then NOBITS (.bss)
    buckets.rw.sort_by_key(|&idx| {
        let sec = &state.sections[idx];
        if sec.name.starts_with(".init_array") { 0u8 }
        else if sec.name.starts_with(".fini_array") { 1 }
        else if sec.nobits { 3 }
        else { 2 }
    });
    buckets
}

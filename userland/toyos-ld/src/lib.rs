//! toyos-ld: Minimal linker for ToyOS.
//!
//! Reads ELF and COFF object files. Produces PIE ELF, static ELF, or PE32+.
//! Supports .o object files and .rlib/.a archives (ar format).

mod collect;
mod reloc;
mod emit_elf;
mod emit_pe;

use std::collections::HashMap;
use std::path::PathBuf;
use std::fs;

use collect::{collect, synthesize_alloc_shims, is_archive, extract_archive, find_lib};
use reloc::{ElfRelocParams, apply_relocs, apply_relocs_pe};
use emit_elf::{layout_elf, emit_bytes, emit_static_bytes, emit_shared_bytes};
use emit_pe::{layout_pe, emit_pe_bytes};

pub(crate) const BASE_VADDR: u64 = 0;
pub(crate) const PAGE_SIZE: u64 = 0x1000;

// ── Error type ──────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum LinkError {
    UndefinedSymbols(Vec<String>),
}

impl LinkError {
    pub fn undefined_symbols(&self) -> &[String] {
        match self {
            LinkError::UndefinedSymbols(syms) => syms,
        }
    }
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
) -> Result<Vec<u8>, LinkError> {
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
pub fn link(objects: &[(String, Vec<u8>)], entry: &str) -> Result<Vec<u8>, LinkError> {
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

/// Link object files and produce a shared library (.so) ELF with .dynsym/.dynstr.
pub fn link_shared(objects: &[(String, Vec<u8>)]) -> Result<Vec<u8>, LinkError> {
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
    buckets
}

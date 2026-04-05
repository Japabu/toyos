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

pub use collect::RelocType;
use collect::{Arch, collect, synthesize_alloc_shims, gc_sections, merge_string_sections, is_archive, is_shared_lib, extract_archive, find_lib, has_toyos_libc_note, scan_symbols, SectionIdx, SectionKind, SymbolDef, SymbolRef};
use reloc::{ElfRelocParams, apply_relocs, apply_relocs_pe, MachORelocParams, apply_relocs_macho};
use emit_elf::{layout_elf, build_eh_frame_hdr, ElfEmitMode, ElfLayout};
use emit_pe::{layout_pe, emit_pe_bytes, PeLayout};
use emit_macho::{layout_macho, emit_macho_bytes, MachOLayout};

pub(crate) const BASE_VADDR: u64 = 0;
pub(crate) const PAGE_SIZE: u64 = 0x1000;

// ── Error type ──────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("undefined symbol: {}", .0.join(", "))]
    UndefinedSymbols(Vec<String>),
    #[error("cannot parse {file}: {message}")]
    Parse { file: String, message: String },
    #[error("unsupported relocation type {reloc_type} for symbol {symbol}")]
    UnsupportedRelocation { reloc_type: RelocType, symbol: String },
    #[error("unsupported raw relocation type {raw_type} for symbol {symbol}")]
    UnsupportedRawRelocation { raw_type: String, symbol: String },
    #[error("relocation overflow: type {reloc_type} for symbol {symbol} value {value:#x}")]
    RelocationOverflow { reloc_type: RelocType, symbol: String, value: i64 },
    #[error("entry symbol '{0}' not found")]
    MissingEntry(String),
}

// ── Pipeline typestate ──────────────────────────────────────────────────
//
// The linker pipeline flows:  Collected → LaidOut<L> → Vec<u8>
//
// `Collected` bundles collect + synthesize + merge into one step. Format-
// specific prep (GC, Mach-O stubs) happens via `&mut self` before layout.
//
// `layout_*` methods consume `Collected`, producing `LaidOut<L>`. This
// prevents re-layout or forgetting to collect.
//
// `relocate_and_emit*` methods consume `LaidOut<L>`, applying relocations
// and emitting the final binary. This prevents emitting without layout or
// relocating twice.

/// Linker state after object collection + preparation. Ready for layout.
pub(crate) struct Collected {
    pub(crate) state: collect::LinkState,
}

/// Linker state after layout. Ready for relocation and emission.
pub(crate) struct LaidOut<L> {
    pub(crate) state: collect::LinkState,
    pub(crate) layout: L,
}

impl Collected {
    fn new(objects: &[(String, Vec<u8>)]) -> Result<Self, LinkError> {
        let mut state = collect(objects)?;
        synthesize_alloc_shims(&mut state);
        merge_string_sections(&mut state);
        Ok(Collected { state })
    }

    fn gc_sections(&mut self, entry: &str) {
        gc_sections(&mut self.state, entry);
    }

    /// Mark undefined symbols as dynamic (dylib) imports for Mach-O linking.
    /// Determines whether each symbol is a function or data by checking if it
    /// has any call-type relocations (functions) vs only GOT/data relocations (data).
    fn mark_dynamic_symbols(&mut self) {
        use std::collections::HashMap as Map;
        let call_relocs: &[RelocType] = match self.state.arch {
            Arch::Aarch64 => &[RelocType::Aarch64Call26, RelocType::Aarch64Jump26],
            Arch::X86_64 => &[RelocType::X86Plt32, RelocType::X86Pc32],
        };
        // Collect referenced symbols and whether they have call relocations
        let mut referenced: Map<String, bool> = Map::new();
        for r in &self.state.relocs {
            let name = r.target.name().to_string();
            let entry = referenced.entry(name).or_insert(false);
            if call_relocs.contains(&r.r_type) {
                *entry = true;
            }
        }
        let local_names: std::collections::HashSet<&str> = self.state.locals.keys().map(|(_, n)| n.as_str()).collect();
        for (sym, has_call) in referenced {
            if !self.state.globals.contains_key(&sym)
                && !local_names.contains(sym.as_str())
            {
                self.state.dynamic_imports.insert(sym.clone());
                self.state.globals.insert(sym, SymbolDef::Dynamic { is_func: has_call, is_tls: false });
            }
        }
    }

    /// Mark sections with absolute relocations as writable (needed for Mach-O rebasing).
    fn mark_abs_reloc_sections_writable(&mut self) {
        let abs_reloc_sections: std::collections::HashSet<SectionIdx> = self.state.relocs.iter()
            .filter(|r| matches!(r.r_type,
                RelocType::Aarch64Abs64 | RelocType::Aarch64Abs32
                | RelocType::X86_64 | RelocType::X86_32))
            .map(|r| r.section)
            .collect();
        for &idx in &abs_reloc_sections {
            let kind = self.state.sections[idx].kind;
            if !kind.is_writable() && !kind.is_tls() {
                self.state.sections[idx].kind = SectionKind::Data;
            }
        }
    }

    fn layout_elf(mut self, base_addr: u64, entry: Option<&str>, build_id: bool) -> LaidOut<ElfLayout> {
        let layout = layout_elf(&mut self.state, base_addr, entry, build_id);
        LaidOut { state: self.state, layout }
    }

    fn layout_pe(mut self) -> LaidOut<PeLayout> {
        let layout = layout_pe(&mut self.state);
        LaidOut { state: self.state, layout }
    }

    fn layout_macho(mut self) -> LaidOut<MachOLayout> {
        let layout = layout_macho(&mut self.state);
        LaidOut { state: self.state, layout }
    }
}

impl LaidOut<ElfLayout> {
    fn relocate_and_emit_pie(mut self, entry: &str) -> Result<Vec<u8>, LinkError> {
        let params = ElfRelocParams {
            got: &self.layout.got,
            tls_start: self.layout.tls_start,
            tls_memsz: self.layout.tls_memsz,
            plt: Some(&self.layout.plt),
            dyn_got: &self.layout.dyn_got,
            record_relatives: true,
            allow_undefined: false,
            is_shared: false,
            ld_got_pair: self.layout.ld_got_pair,
            gd_got: &self.layout.gd_got,
        };
        let relocs = apply_relocs(&mut self.state, &params)?;
        let eh_hdr = build_eh_frame_hdr(&self.state, &self.layout);
        emit_elf::emit_elf(&self.state, &self.layout, ElfEmitMode::Pie {
            entry_name: entry,
            relocs: &relocs,
            eh_frame_hdr: &eh_hdr,
        })
    }

    fn relocate_and_emit_static(mut self, entry: &str) -> Result<Vec<u8>, LinkError> {
        let empty_dyn_got = HashMap::new();
        let empty_gd_got = HashMap::new();
        let params = ElfRelocParams {
            got: &self.layout.got,
            tls_start: self.layout.tls_start,
            tls_memsz: self.layout.tls_memsz,
            plt: None,
            dyn_got: &empty_dyn_got,
            record_relatives: false,
            allow_undefined: false,
            is_shared: false,
            ld_got_pair: None,
            gd_got: &empty_gd_got,
        };
        apply_relocs(&mut self.state, &params)?;
        emit_elf::emit_elf(&self.state, &self.layout, ElfEmitMode::Static { entry_name: entry })
    }

    fn relocate_and_emit_shared(mut self) -> Result<Vec<u8>, LinkError> {
        let params = ElfRelocParams {
            got: &self.layout.got,
            tls_start: self.layout.tls_start,
            tls_memsz: self.layout.tls_memsz,
            plt: Some(&self.layout.plt),
            dyn_got: &self.layout.dyn_got,
            record_relatives: true,
            allow_undefined: true,
            is_shared: true,
            ld_got_pair: self.layout.ld_got_pair,
            gd_got: &self.layout.gd_got,
        };
        let relocs = apply_relocs(&mut self.state, &params)?;
        let eh_hdr = build_eh_frame_hdr(&self.state, &self.layout);
        emit_elf::emit_elf(&self.state, &self.layout, ElfEmitMode::Shared {
            relocs: &relocs,
            eh_frame_hdr: &eh_hdr,
        })
    }
}

impl LaidOut<PeLayout> {
    fn relocate_and_emit(mut self, entry: &str, subsystem: u16) -> Result<Vec<u8>, LinkError> {
        let abs_fixups = apply_relocs_pe(&mut self.state, &self.layout)?;
        emit_pe_bytes(&self.state, &self.layout, entry, subsystem, &abs_fixups)
    }
}

impl LaidOut<MachOLayout> {
    fn relocate_and_emit(mut self, entry: &str) -> Result<Vec<u8>, LinkError> {
        let params = MachORelocParams {
            got: &self.layout.got,
            tls_template_start: self.layout.tls_template_start,
        };
        let reloc_output = apply_relocs_macho(&mut self.state, &params)?;

        let mut bind_entries: Vec<(String, u64)> = self.layout.got_entries.iter()
            .filter(|(_, ext)| *ext)
            .map(|(sym, _)| (sym.name().to_string(), self.layout.got[sym]))
            .collect();
        // Add non-GOT dynamic binds (e.g. function pointers stored in data)
        bind_entries.extend(reloc_output.bind_entries);

        let mut rebase_entries = reloc_output.rebase_entries;
        for (sym, ext) in &self.layout.got_entries {
            if !ext {
                rebase_entries.push(self.layout.got[sym]);
            }
        }

        emit_macho_bytes(&self.state, &self.layout, entry, &rebase_entries, &bind_entries)
    }
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
) -> Result<Vec<u8>, LinkError> {
    link_pe_with(objects, entry, subsystem, false)
}

pub fn link_pe_with(
    objects: &[(String, Vec<u8>)],
    entry: &str,
    subsystem: u16,
    gc: bool,
) -> Result<Vec<u8>, LinkError> {
    let mut collected = Collected::new(objects)?;
    if gc { collected.gc_sections(entry); }
    collected.layout_pe().relocate_and_emit(entry, subsystem)
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
    link_static_full(objects, entry, base_addr, false, false)
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
    let mut collected = Collected::new(objects)?;
    if gc { collected.gc_sections(entry); }
    collected.layout_elf(base_addr, None, build_id)
        .relocate_and_emit_static(entry)
}

/// Link object files and produce a PIE ELF executable.
/// Returns the raw ELF bytes on success, or a list of undefined symbols on failure.
pub fn link(objects: &[(String, Vec<u8>)], entry: &str) -> Result<Vec<u8>, LinkError> {
    link_full(objects, entry, false, false)
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
    let mut collected = Collected::new(objects)?;
    create_call_stubs(&mut collected.state);
    if gc { collected.gc_sections(entry); }
    collected.layout_elf(BASE_VADDR, Some(entry), build_id)
        .relocate_and_emit_pie(entry)
}

/// Resolve library names (-l flags) against search paths (-L flags),
/// reading and extracting archives. Only includes archive members that
/// define symbols needed by already-included objects (transitive pull-in).
/// The entry point symbol is treated as an initial undefined, ensuring the
/// archive member that defines it (e.g. std's `_start`) gets pulled in.
pub fn resolve_libs(
    inputs: &[PathBuf],
    lib_paths: &[PathBuf],
    libs: &[String],
) -> Result<Vec<(String, Vec<u8>)>, LinkError> {
    resolve_libs_with_entry(inputs, lib_paths, libs, None)
}

/// Like `resolve_libs`, but seeds the undefined symbol set with `entry` so
/// that archive members providing the entry point are pulled in.
pub fn resolve_libs_with_entry(
    inputs: &[PathBuf],
    lib_paths: &[PathBuf],
    libs: &[String],
    entry: Option<&str>,
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

    // If any object was compiled by toyos-cc (has .note.toyos.libc section),
    // implicitly add libtoyos_c from -L paths to provide C standard library symbols.
    let needs_libc = objects.iter().chain(archive_members.iter())
        .any(|(_, data)| has_toyos_libc_note(data));
    if needs_libc {
        if let Some((name, data)) = find_lib("toyos_c", lib_paths) {
            if is_archive(&data) {
                extract_archive(&name, &data, &mut archive_members)?;
            } else {
                objects.push((name, data));
            }
        }
    }

    // Scan direct objects for defined/referenced symbols.
    // Symbols from shared libraries (.so) must NOT count as "defined" for archive pull-in —
    // they're resolved at load time, not link time. Only .o file definitions prevent pull-in.
    let mut defined = HashSet::new();
    let mut undefined = HashSet::new();
    // The entry point is an implicit undefined — ensure its archive member
    // gets pulled in even if no direct object references it.
    if let Some(entry) = entry {
        undefined.insert(entry.to_string());
    }
    for (_name, data) in &objects {
        // Skip shared libraries for defined-symbol scanning
        if is_shared_lib(data) { continue; }
        let (defs, refs) = scan_symbols(data);
        defined.extend(defs);
        undefined.extend(refs);
    }
    // Only truly undefined: referenced but not yet defined by .o files
    undefined.retain(|s| !defined.contains(s));

    // Scan archive members and build a symbol → member index for O(1) lookup.
    let mut member_defs: Vec<HashSet<String>> = Vec::with_capacity(archive_members.len());
    let mut member_refs: Vec<HashSet<String>> = Vec::with_capacity(archive_members.len());
    let mut sym_to_members: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, (_, data)) in archive_members.iter().enumerate() {
        let (defs, refs) = scan_symbols(data);
        for sym in &defs {
            sym_to_members.entry(sym.clone()).or_default().push(i);
        }
        member_defs.push(defs);
        member_refs.push(refs);
    }

    // Worklist-driven pull-in: process each undefined symbol once
    let mut included = vec![false; archive_members.len()];
    let mut worklist: Vec<String> = undefined.iter().cloned().collect();
    while let Some(sym) = worklist.pop() {
        if let Some(members) = sym_to_members.get(&sym) {
            for &i in members {
                if included[i] { continue; }
                included[i] = true;
                defined.extend(member_defs[i].iter().cloned());
                for r in &member_refs[i] {
                    if !defined.contains(r) && undefined.insert(r.clone()) {
                        worklist.push(r.clone());
                    }
                }
            }
        }
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
    let mut collected = Collected::new(objects)?;
    collected.mark_dynamic_symbols();
    create_call_stubs(&mut collected.state);
    collected.layout_elf(BASE_VADDR, None, build_id)
        .relocate_and_emit_shared()
}

/// Link object files and produce a Mach-O executable for macOS.
/// Undefined symbols are resolved against /usr/lib/libSystem.B.dylib at runtime.
pub fn link_macho(
    objects: &[(String, Vec<u8>)],
    entry: &str,
    gc: bool,
) -> Result<Vec<u8>, LinkError> {
    // Internally all symbol names use ELF convention (no `_` prefix).
    // Accept Mach-O-style `_main` from CLI by stripping the prefix.
    let entry = entry.strip_prefix('_').unwrap_or(entry);
    let mut collected = Collected::new(objects)?;
    collected.mark_dynamic_symbols();
    create_call_stubs(&mut collected.state);
    collected.mark_abs_reloc_sections_writable();
    if gc { collected.gc_sections(entry); }
    collected.layout_macho()
        .relocate_and_emit(entry)
}

/// Create call stubs for relocations targeting dynamic (undefined) symbols.
/// On Mach-O, direct call instructions can't reach dylib functions — the
/// linker must create stubs that load the address from a GOT slot and branch.
fn create_call_stubs(state: &mut collect::LinkState) {
    use std::collections::BTreeSet;

    // Find all dynamic symbols with call relocations
    let mut stub_syms = BTreeSet::new();
    let call_relocs: &[RelocType] = match state.arch {
        Arch::Aarch64 => &[RelocType::Aarch64Call26, RelocType::Aarch64Jump26],
        Arch::X86_64 => &[RelocType::X86Plt32, RelocType::X86Pc32],
    };
    for reloc in &state.relocs {
        if !call_relocs.contains(&reloc.r_type) { continue; }
        if let Some(SymbolDef::Dynamic { .. }) = state.globals.get(reloc.target.name()) {
            stub_syms.insert(reloc.target.name().to_string());
        }
    }

    // On x86_64, Cranelift emits `call *sym@GOTPCREL(%rip)` (FF 15 disp32) for
    // external calls instead of `call sym@PLT`. Detect these and redirect them
    // through stubs so we can set %al for the x86_64 SysV variadic ABI.
    if matches!(state.arch, Arch::X86_64) {
        rewrite_x86_64_got_calls(state, &mut stub_syms);
    }

    if stub_syms.is_empty() { return; }

    let stub_sec_idx = SectionIdx(state.sections.len());

    match state.arch {
        Arch::Aarch64 => create_aarch64_stubs(state, &stub_syms, stub_sec_idx),
        Arch::X86_64 => create_x86_64_stubs(state, &stub_syms, stub_sec_idx),
    }

    // Rewrite call relocations to target the stub symbols
    let stub_syms_set: BTreeSet<&str> = stub_syms.iter().map(|s| s.as_str()).collect();
    for reloc in &mut state.relocs {
        if call_relocs.contains(&reloc.r_type)
            && stub_syms_set.contains(reloc.target.name())
        {
            reloc.target = SymbolRef::Global(format!("{}.__stub", reloc.target.name()));
        }
    }
}

/// Rewrite `movq sym@GOTPCREL(%rip), %reg` to `leaq sym.__stub(%rip), %reg`
/// for function symbols on x86_64. Cranelift loads function addresses from GOT
/// then does `call *%reg`. By making the register hold the stub address instead,
/// all calls go through stubs that set %al for the x86_64 SysV variadic ABI.
/// Non-variadic callees ignore %al, so this is safe for all function calls.
///
/// Only function symbols (defined in Code sections or Dynamic imports) are
/// rewritten. Data/string GOT loads are left untouched.
fn rewrite_x86_64_got_calls(
    state: &mut collect::LinkState,
    stub_syms: &mut std::collections::BTreeSet<String>,
) {
    let is_function_symbol = |name: &str| -> bool {
        match state.globals.get(name) {
            Some(SymbolDef::Dynamic { is_func, .. }) => *is_func,
            Some(SymbolDef::Defined { section, .. }) => {
                state.sections[*section].kind == SectionKind::Code
            }
            None => false,
        }
    };

    let rewrite_indices: Vec<usize> = state.relocs.iter().enumerate()
        .filter(|(_, r)| {
            if !matches!(r.r_type, RelocType::X86Gotpcrel | RelocType::X86Gotpcrelx | RelocType::X86RexGotpcrelx) {
                return false;
            }
            // Only rewrite MOV instructions (opcode 8B at offset-2).
            let sec = &state.sections[r.section];
            let off = r.offset as usize;
            if off < 2 || sec.data.get(off - 2).copied() != Some(0x8B) {
                return false;
            }
            is_function_symbol(r.target.name())
        })
        .map(|(i, _)| i)
        .collect();

    for &i in &rewrite_indices {
        let reloc = &state.relocs[i];
        let sym_name = reloc.target.name().to_string();
        let sec_idx = reloc.section;
        let off = reloc.offset as usize;

        // Change opcode from MOV (8B) to LEA (8D) — same encoding otherwise.
        // This turns `movq *disp(%rip), %reg` (dereference GOT) into
        // `leaq disp(%rip), %reg` (compute stub address).
        state.sections[sec_idx].data[off - 2] = 0x8D;

        stub_syms.insert(sym_name.clone());

        // Change relocation from GOTPCREL → PC32 targeting the stub
        state.relocs[i].r_type = RelocType::X86Pc32;
        state.relocs[i].target = SymbolRef::Global(format!("{sym_name}.__stub"));
        state.relocs[i].addend = -4;
    }
}

fn create_aarch64_stubs(
    state: &mut collect::LinkState,
    stub_syms: &std::collections::BTreeSet<String>,
    stub_sec_idx: SectionIdx,
) {
    let stub_count = stub_syms.len();
    let mut stub_data = Vec::with_capacity(stub_count * 12);
    let mut stub_relocs = Vec::new();

    for (i, sym_name) in stub_syms.iter().enumerate() {
        let offset = (i * 12) as u64;
        // adrp x16, sym@GOTPAGE
        stub_data.extend_from_slice(&0x9000_0010u32.to_le_bytes());
        // ldr x16, [x16, sym@GOTLO12]
        stub_data.extend_from_slice(&0xF940_0210u32.to_le_bytes());
        // br x16
        stub_data.extend_from_slice(&0xD61F_0200u32.to_le_bytes());

        stub_relocs.push(collect::InputReloc {
            section: stub_sec_idx, offset,
            r_type: RelocType::Aarch64AdrGotPage,
            target: SymbolRef::Global(sym_name.clone()), addend: 0,
            subtrahend: None,
        });
        stub_relocs.push(collect::InputReloc {
            section: stub_sec_idx, offset: offset + 4,
            r_type: RelocType::Aarch64Ld64GotLo12Nc,
            target: SymbolRef::Global(sym_name.clone()), addend: 0,
            subtrahend: None,
        });

        state.globals.insert(format!("{sym_name}.__stub"), SymbolDef::Defined {
            section: stub_sec_idx, value: offset, size: 0,
        });
    }

    state.sections.push(collect::InputSection {
        name: ".text".to_string(), data: stub_data, align: 4,
        size: (stub_count * 12) as u64, vaddr: None,
        kind: SectionKind::Code, merge: false, strings: false, entsize: 0,
    });
    state.relocs.extend(stub_relocs);
}

fn create_x86_64_stubs(
    state: &mut collect::LinkState,
    stub_syms: &std::collections::BTreeSet<String>,
    stub_sec_idx: SectionIdx,
) {
    // x86-64 stub: 8 bytes per stub
    // Each stub:  mov $8, %al              [B0 08]
    //             jmpq *sym@GOTPCREL(%rip) [FF 25 xx xx xx xx]
    // The mov sets %al = 8 (max XMM arg count) for x86_64 SysV variadic calls.
    // Non-variadic callees ignore %al, so this is safe for all dynamic calls.
    let stub_count = stub_syms.len();
    let mut stub_data = Vec::with_capacity(stub_count * 8);
    let mut stub_relocs = Vec::new();

    for (i, sym_name) in stub_syms.iter().enumerate() {
        let offset = (i * 8) as u64;
        // B0 08 = mov $8, %al — conservative upper bound on vector register args
        stub_data.extend_from_slice(&[0xB0, 0x08]);
        // FF 25 00000000 = jmpq *0(%rip) — placeholder, patched by GOTPCREL reloc
        stub_data.extend_from_slice(&[0xFF, 0x25, 0x00, 0x00, 0x00, 0x00]);

        // GOTPCREL reloc at offset+4 (the 4-byte displacement after FF 25)
        stub_relocs.push(collect::InputReloc {
            section: stub_sec_idx, offset: offset + 4,
            r_type: RelocType::X86Gotpcrel,
            target: SymbolRef::Global(sym_name.clone()), addend: -4,
            subtrahend: None,
        });

        state.globals.insert(format!("{sym_name}.__stub"), SymbolDef::Defined {
            section: stub_sec_idx, value: offset, size: 0,
        });
    }

    state.sections.push(collect::InputSection {
        name: ".text".to_string(), data: stub_data, align: 8,
        size: (stub_count * 8) as u64, vaddr: None,
        kind: SectionKind::Code, merge: false, strings: false, entsize: 0,
    });
    state.relocs.extend(stub_relocs);
}

// ── Shared helpers ──────────────────────────────────────────────────────

pub(crate) fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

pub(crate) struct SectionBuckets {
    pub(crate) rx: Vec<SectionIdx>,
    pub(crate) rw: Vec<SectionIdx>,
    pub(crate) tls: Vec<SectionIdx>,
}

pub(crate) fn classify_sections(state: &mut collect::LinkState) -> SectionBuckets {
    let mut buckets = SectionBuckets { rx: Vec::new(), rw: Vec::new(), tls: Vec::new() };
    for (idx, sec) in state.sections.iter().enumerate() {
        let idx = SectionIdx(idx);
        if sec.kind.is_tls() {
            buckets.tls.push(idx);
            if !state.tls_sections.contains(&idx) {
                state.tls_sections.push(idx);
            }
        } else if sec.kind.is_writable() {
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
        match sec.kind {
            SectionKind::InitArray => 0u8,
            SectionKind::FiniArray => 1,
            SectionKind::Bss => 3,
            SectionKind::Data => 2,
            // classify_sections routes Code/ReadOnly to rx, TLS to tls
            SectionKind::Code | SectionKind::ReadOnly
            | SectionKind::Tls | SectionKind::TlsBss
            | SectionKind::TlsVariables => unreachable!(),
        }
    });
    buckets
}

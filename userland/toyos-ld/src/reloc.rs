use crate::collect::{InputReloc, LinkState, RelocType, SectionIdx, SymbolDef};
use crate::emit_pe::PeLayout;
use crate::LinkError;
use std::collections::{HashMap, HashSet};

pub(crate) struct RelocOutput {
    pub(crate) relatives: Vec<(u64, i64)>,
    /// Dynamic GOT entries needing GLOB_DAT relocations: (GOT slot vaddr, symbol name).
    pub(crate) glob_dats: Vec<(u64, String)>,
}

/// Resolve a symbol to its virtual address.
/// `plt` provides PLT stubs for dynamic symbols (PIE mode). Pass `None` for
/// static/PE modes where dynamic symbols are unsupported.
pub(crate) fn resolve_symbol(
    state: &LinkState,
    name: &str,
    from_sec: SectionIdx,
    plt: Option<&HashMap<String, u64>>,
) -> Option<u64> {
    if let Some(def) = state.globals.get(name) {
        match def {
            SymbolDef::Dynamic => return plt.and_then(|p| p.get(name).copied()),
            SymbolDef::Defined { section, value } => {
                return Some(state.sections[*section].vaddr.unwrap() + value);
            }
        }
    }
    if let Some(obj_idx) = state.sections[from_sec].obj_idx {
        if let Some(SymbolDef::Defined { section, value }) = state.locals.get(&(obj_idx, name.to_string())) {
            return Some(state.sections[*section].vaddr.unwrap() + value);
        }
    }
    None
}

/// x86-64 Variant II: TP points to end of TLS block.
/// TPOFF = symbol_vaddr - (tls_start + tls_memsz)
pub(crate) fn tpoff(sym_addr: u64, tls_start: u64, tls_memsz: u64) -> i64 {
    sym_addr as i64 - (tls_start as i64 + tls_memsz as i64)
}

fn check_i32(value: i64, reloc: &InputReloc) -> Result<(), LinkError> {
    if value < i32::MIN as i64 || value > i32::MAX as i64 {
        return Err(LinkError::RelocationOverflow {
            reloc_type: reloc.r_type,
            symbol: reloc.symbol_name.clone(),
            value,
        });
    }
    Ok(())
}

fn check_u32(value: i64, reloc: &InputReloc) -> Result<(), LinkError> {
    if value < 0 || value > u32::MAX as i64 {
        return Err(LinkError::RelocationOverflow {
            reloc_type: reloc.r_type,
            symbol: reloc.symbol_name.clone(),
            value,
        });
    }
    Ok(())
}

/// Apply a single relocation, returning `true` if it's an absolute reference
/// (needs a runtime relocation / PE base fixup).
fn apply_one_reloc(
    state: &mut LinkState,
    reloc: &InputReloc,
    sym_addr: u64,
    reloc_vaddr: u64,
    got: &HashMap<String, u64>,
) -> Result<bool, LinkError> {
    match reloc.r_type {
        RelocType::X86_64 => {
            let value = (sym_addr as i64 + reloc.addend) as u64;
            write_u64(state, reloc.section, reloc.offset, value);
            Ok(true)
        }
        RelocType::X86Pc32 | RelocType::X86Plt32 => {
            let value = sym_addr as i64 + reloc.addend - reloc_vaddr as i64;
            check_i32(value, reloc)?;
            write_i32(state, reloc.section, reloc.offset, value as i32);
            Ok(false)
        }
        RelocType::X86_32 => {
            let value = sym_addr as i64 + reloc.addend;
            check_u32(value, reloc)?;
            write_u32(state, reloc.section, reloc.offset, value as u32);
            Ok(false)
        }
        RelocType::X86_32S => {
            let value = sym_addr as i64 + reloc.addend;
            check_i32(value, reloc)?;
            write_i32(state, reloc.section, reloc.offset, value as i32);
            Ok(false)
        }
        RelocType::X86Gotpcrel | RelocType::X86Gotpcrelx
        | RelocType::X86RexGotpcrelx => {
            let got_slot = *got.get(&reloc.symbol_name).ok_or_else(|| {
                LinkError::UndefinedSymbols(vec![reloc.symbol_name.clone()])
            })?;
            let value = got_slot as i64 + reloc.addend - reloc_vaddr as i64;
            check_i32(value, reloc)?;
            write_i32(state, reloc.section, reloc.offset, value as i32);
            Ok(false)
        }
        other => Err(LinkError::UnsupportedRelocation {
            reloc_type: other,
            symbol: reloc.symbol_name.clone(),
        }),
    }
}

fn write_bytes(state: &mut LinkState, sec_idx: SectionIdx, offset: u64, bytes: &[u8]) {
    let sec = &mut state.sections[sec_idx];
    let off = offset as usize;
    sec.data[off..off + bytes.len()].copy_from_slice(bytes);
}

fn write_u64(state: &mut LinkState, sec_idx: SectionIdx, offset: u64, value: u64) {
    write_bytes(state, sec_idx, offset, &value.to_le_bytes());
}

fn write_i32(state: &mut LinkState, sec_idx: SectionIdx, offset: u64, value: i32) {
    write_bytes(state, sec_idx, offset, &value.to_le_bytes());
}

fn write_u32(state: &mut LinkState, sec_idx: SectionIdx, offset: u64, value: u32) {
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
pub(crate) struct ElfRelocParams<'a> {
    pub(crate) got: &'a HashMap<String, u64>,
    pub(crate) tls_start: u64,
    pub(crate) tls_memsz: u64,
    pub(crate) plt: Option<&'a HashMap<String, u64>>,
    pub(crate) dyn_got: &'a HashMap<String, u64>,
    /// PIE mode: record R_X86_64_RELATIVE entries for runtime relocation.
    /// Static mode: addresses are fixed at link time, no RELATIVE needed.
    pub(crate) record_relatives: bool,
    pub(crate) allow_undefined: bool,
}

pub(crate) fn apply_relocs(
    state: &mut LinkState,
    params: &ElfRelocParams,
) -> Result<RelocOutput, LinkError> {
    let mut relatives = Vec::new();
    let mut undefined = HashSet::new();

    let relocs = std::mem::take(&mut state.relocs);

    // Pass 1: TLS GD/LD/DTPOFF relaxations. These rewrite instruction bytes
    // and overwrite the companion `call __tls_get_addr` instruction, so we
    // track which (section, offset) ranges were relaxed.
    let mut relaxed_calls: HashSet<(SectionIdx, u64)> = HashSet::new();

    for reloc in &relocs {
        match reloc.r_type {
            RelocType::X86Tlsgd => {
                let sym_addr = resolve_symbol(state, &reloc.symbol_name, reloc.section, params.plt)
                    .ok_or_else(|| LinkError::UndefinedSymbols(vec![reloc.symbol_name.clone()]))?;
                let padded = is_padded_tls_sequence(
                    &state.sections[reloc.section].data,
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
                    write_bytes(state, reloc.section, reloc.offset - 4, &inst);
                    let tp_value = tpoff(sym_addr, params.tls_start, params.tls_memsz);
                    check_i32(tp_value, reloc)?;
                    write_i32(state, reloc.section, reloc.offset + 8, tp_value as i32);
                    relaxed_calls.insert((reloc.section, reloc.offset + 8));
                } else {
                    return Err(LinkError::UnsupportedRelocation {
                        reloc_type: RelocType::X86Tlsgd,
                        symbol: reloc.symbol_name.clone(),
                    });
                }
            }
            RelocType::X86Tlsld => {
                let padded = is_padded_tls_sequence(
                    &state.sections[reloc.section].data,
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
                    write_bytes(state, reloc.section, reloc.offset - 4, &inst);
                    relaxed_calls.insert((reloc.section, reloc.offset + 8));
                } else {
                    // LD → LE (12-byte unpadded)
                    #[rustfmt::skip]
                    let inst: [u8; 12] = [
                        0x66, 0x66, 0x66,                                           // 3x data16
                        0x64, 0x48, 0x8b, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00,     // mov %fs:0,%rax
                    ];
                    write_bytes(state, reloc.section, reloc.offset - 3, &inst);
                    relaxed_calls.insert((reloc.section, reloc.offset + 5));
                }
            }
            RelocType::X86Dtpoff32 => {
                let sym_addr = resolve_symbol(state, &reloc.symbol_name, reloc.section, params.plt)
                    .ok_or_else(|| LinkError::UndefinedSymbols(vec![reloc.symbol_name.clone()]))?;
                let value = tpoff(sym_addr, params.tls_start, params.tls_memsz) + reloc.addend;
                check_i32(value, reloc)?;
                write_i32(state, reloc.section, reloc.offset, value as i32);
            }
            // Handled in pass 2
            RelocType::X86_64 | RelocType::X86Pc32 | RelocType::X86Plt32
            | RelocType::X86_32 | RelocType::X86_32S
            | RelocType::X86Gotpcrel | RelocType::X86Gotpcrelx
            | RelocType::X86RexGotpcrelx
            | RelocType::X86Tpoff32 | RelocType::X86Gottpoff
            | RelocType::Aarch64Abs64 | RelocType::Aarch64Call26
            | RelocType::Aarch64AdrPrelPgHi21 | RelocType::Aarch64AddAbsLo12Nc
            | RelocType::Aarch64Ldst64AbsLo12Nc
            | RelocType::Aarch64AdrGotPage | RelocType::Aarch64Ld64GotLo12Nc => {}
        }
    }

    // Pass 2: all other relocations
    for reloc in &relocs {
        if matches!(reloc.r_type, RelocType::X86Tlsgd | RelocType::X86Tlsld | RelocType::X86Dtpoff32) {
            continue; // handled in pass 1
        }
        if relaxed_calls.contains(&(reloc.section, reloc.offset)) {
            continue;
        }

        let sec = &state.sections[reloc.section];
        let reloc_vaddr = sec.vaddr.unwrap() + reloc.offset;

        let sym_addr = match resolve_symbol(state, &reloc.symbol_name, reloc.section, params.plt) {
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
            RelocType::X86Tpoff32 => {
                let value = tpoff(sym_addr, params.tls_start, params.tls_memsz) + reloc.addend;
                check_i32(value, reloc)?;
                write_i32(state, reloc.section, reloc.offset, value as i32);
            }
            RelocType::X86Gottpoff => {
                let got_slot = *params.got.get(&reloc.symbol_name).ok_or_else(|| {
                    LinkError::UndefinedSymbols(vec![reloc.symbol_name.clone()])
                })?;
                let value = got_slot as i64 + reloc.addend - reloc_vaddr as i64;
                check_i32(value, reloc)?;
                write_i32(state, reloc.section, reloc.offset, value as i32);
            }
            // Pass 1 handled these; skipped by continue above
            RelocType::X86Tlsgd | RelocType::X86Tlsld | RelocType::X86Dtpoff32 => {
                unreachable!("handled in pass 1")
            }
            RelocType::X86_64 | RelocType::X86Pc32 | RelocType::X86Plt32
            | RelocType::X86_32 | RelocType::X86_32S
            | RelocType::X86Gotpcrel | RelocType::X86Gotpcrelx
            | RelocType::X86RexGotpcrelx
            | RelocType::Aarch64Abs64 | RelocType::Aarch64Call26
            | RelocType::Aarch64AdrPrelPgHi21 | RelocType::Aarch64AddAbsLo12Nc
            | RelocType::Aarch64Ldst64AbsLo12Nc
            | RelocType::Aarch64AdrGotPage | RelocType::Aarch64Ld64GotLo12Nc => {
                let is_abs = apply_one_reloc(state, reloc, sym_addr, reloc_vaddr, params.got)?;
                if is_abs && params.record_relatives {
                    relatives.push((reloc_vaddr, sym_addr as i64 + reloc.addend));
                }
            }
        }
    }

    // Fill GOT entries (PIE mode records as RELATIVE; static mode handles in emit)
    if params.record_relatives {
        let gottpoff_syms: HashSet<String> = relocs
            .iter()
            .filter(|r| r.r_type == RelocType::X86Gottpoff)
            .map(|r| r.symbol_name.clone())
            .collect();

        for (sym_name, &got_vaddr) in params.got {
            let sym_addr = resolve_symbol(state, sym_name, SectionIdx(0), params.plt)
                .ok_or_else(|| LinkError::UndefinedSymbols(vec![sym_name.clone()]))?;
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
        return Err(LinkError::UndefinedSymbols(syms));
    }

    Ok(RelocOutput { relatives, glob_dats })
}

// ── AArch64 relocation helpers ───────────────────────────────────────────

/// Apply a single AArch64 relocation. Returns `true` if it produced an absolute
/// reference that needs a Mach-O rebase entry.
fn apply_one_reloc_aarch64(
    state: &mut LinkState,
    reloc: &InputReloc,
    sym_addr: u64,
    reloc_vaddr: u64,
    got: &HashMap<String, u64>,
) -> Result<bool, LinkError> {
    match reloc.r_type {
        // Absolute 64-bit pointer
        RelocType::Aarch64Abs64 => {
            let value = (sym_addr as i64 + reloc.addend) as u64;
            write_u64(state, reloc.section, reloc.offset, value);
            Ok(true) // needs rebase
        }
        // BL/B instruction: 26-bit PC-relative
        RelocType::Aarch64Call26 => {
            let value = sym_addr as i64 + reloc.addend - reloc_vaddr as i64;
            // Must be 4-byte aligned and fit in 26-bit signed * 4
            let imm26 = value >> 2;
            if imm26 < -(1 << 25) || imm26 >= (1 << 25) {
                return Err(LinkError::RelocationOverflow {
                    reloc_type: reloc.r_type, symbol: reloc.symbol_name.clone(), value,
                });
            }
            patch_aarch64_insn_imm26(state, reloc.section, reloc.offset, imm26 as u32);
            Ok(false)
        }
        // ADRP: 21-bit page-relative
        RelocType::Aarch64AdrPrelPgHi21 => {
            let sym_page = (sym_addr as i64 + reloc.addend) & !0xFFF;
            let pc_page = reloc_vaddr as i64 & !0xFFF;
            let page_delta = (sym_page - pc_page) >> 12;
            if page_delta < -(1 << 20) || page_delta >= (1 << 20) {
                return Err(LinkError::RelocationOverflow {
                    reloc_type: reloc.r_type, symbol: reloc.symbol_name.clone(), value: page_delta,
                });
            }
            patch_aarch64_adrp(state, reloc.section, reloc.offset, page_delta as i32);
            Ok(false)
        }
        // ADD immediate: 12-bit page offset
        RelocType::Aarch64AddAbsLo12Nc => {
            let value = ((sym_addr as i64 + reloc.addend) & 0xFFF) as u32;
            patch_aarch64_add_imm12(state, reloc.section, reloc.offset, value);
            Ok(false)
        }
        // LDR 64-bit: 12-bit page offset (scaled by 8)
        RelocType::Aarch64Ldst64AbsLo12Nc => {
            let value = ((sym_addr as i64 + reloc.addend) & 0xFFF) as u32;
            patch_aarch64_ldr_imm12(state, reloc.section, reloc.offset, value, 3);
            Ok(false)
        }
        // ADRP for GOT entry page
        RelocType::Aarch64AdrGotPage => {
            let got_slot = *got.get(&reloc.symbol_name).ok_or_else(|| {
                LinkError::UndefinedSymbols(vec![reloc.symbol_name.clone()])
            })?;
            let sym_page = got_slot as i64 & !0xFFF;
            let pc_page = reloc_vaddr as i64 & !0xFFF;
            let page_delta = (sym_page - pc_page) >> 12;
            if page_delta < -(1 << 20) || page_delta >= (1 << 20) {
                return Err(LinkError::RelocationOverflow {
                    reloc_type: reloc.r_type, symbol: reloc.symbol_name.clone(), value: page_delta,
                });
            }
            patch_aarch64_adrp(state, reloc.section, reloc.offset, page_delta as i32);
            Ok(false)
        }
        // LDR for GOT entry page offset
        RelocType::Aarch64Ld64GotLo12Nc => {
            let got_slot = *got.get(&reloc.symbol_name).ok_or_else(|| {
                LinkError::UndefinedSymbols(vec![reloc.symbol_name.clone()])
            })?;
            let value = (got_slot & 0xFFF) as u32;
            patch_aarch64_ldr_imm12(state, reloc.section, reloc.offset, value, 3);
            Ok(false)
        }
        other => Err(LinkError::UnsupportedRelocation {
            reloc_type: other, symbol: reloc.symbol_name.clone(),
        }),
    }
}

/// Read-modify-write a 32-bit LE instruction in a section.
fn modify_insn(state: &mut LinkState, sec: SectionIdx, offset: u64, f: impl FnOnce(u32) -> u32) {
    let data = &mut state.sections[sec].data;
    let off = offset as usize;
    let insn = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
    data[off..off + 4].copy_from_slice(&f(insn).to_le_bytes());
}

/// Patch ADRP instruction's immhi:immlo fields with a page delta.
fn patch_aarch64_adrp(state: &mut LinkState, sec: SectionIdx, offset: u64, page_delta: i32) {
    let val = page_delta as u32;
    modify_insn(state, sec, offset, |insn|
        (insn & 0x9F00_001F) | ((val & 0x3) << 29) | (((val >> 2) & 0x7FFFF) << 5));
}

/// Patch BL/B instruction's imm26 field.
fn patch_aarch64_insn_imm26(state: &mut LinkState, sec: SectionIdx, offset: u64, imm26: u32) {
    modify_insn(state, sec, offset, |insn| (insn & 0xFC00_0000) | (imm26 & 0x03FF_FFFF));
}

/// Patch ADD instruction's imm12 field (bits [21:10]).
fn patch_aarch64_add_imm12(state: &mut LinkState, sec: SectionIdx, offset: u64, value: u32) {
    modify_insn(state, sec, offset, |insn| (insn & !(0xFFF << 10)) | ((value & 0xFFF) << 10));
}

/// Patch LDR/STR instruction's scaled imm12 field (bits [21:10]).
/// `scale` is the log2 of the access size (0=byte, 1=half, 2=word, 3=dword).
fn patch_aarch64_ldr_imm12(state: &mut LinkState, sec: SectionIdx, offset: u64, value: u32, scale: u32) {
    modify_insn(state, sec, offset, |insn| {
        let scaled = (value >> scale) & 0xFFF;
        (insn & !(0xFFF << 10)) | (scaled << 10)
    });
}

/// Parameters for Mach-O relocation application.
pub(crate) struct MachORelocParams<'a> {
    pub(crate) got: &'a HashMap<String, u64>,
}

/// Apply relocations for Mach-O output. Returns rebase entries (internal absolute
/// pointers) and bind entries (dynamic symbol references for dyld).
pub(crate) fn apply_relocs_macho(
    state: &mut LinkState,
    params: &MachORelocParams,
) -> Result<MachORelocOutput, LinkError> {
    let mut rebase_entries = Vec::new();
    let mut bind_entries = Vec::new();
    let mut undefined = std::collections::HashSet::new();

    let relocs = std::mem::take(&mut state.relocs);

    for reloc in &relocs {
        let sec = &state.sections[reloc.section];
        let reloc_vaddr = sec.vaddr.unwrap() + reloc.offset;

        // GOT relocations don't need the symbol address — they use the GOT
        // slot address, which is looked up by name inside the reloc handler.
        let is_got_reloc = matches!(reloc.r_type,
            RelocType::Aarch64AdrGotPage | RelocType::Aarch64Ld64GotLo12Nc);

        let sym_addr = match resolve_symbol(state, &reloc.symbol_name, reloc.section, None) {
            Some(a) => a,
            None if is_got_reloc && params.got.contains_key(&reloc.symbol_name) => 0,
            None if matches!(reloc.r_type, RelocType::Aarch64Abs64 | RelocType::X86_64) => {
                // Absolute pointer to an external symbol — dyld will bind it.
                bind_entries.push((reloc.symbol_name.clone(), reloc_vaddr));
                continue;
            }
            None => {
                undefined.insert(reloc.symbol_name.clone());
                continue;
            }
        };

        let is_abs = match reloc.r_type {
            // AArch64 relocations
            RelocType::Aarch64Abs64
            | RelocType::Aarch64Call26
            | RelocType::Aarch64AdrPrelPgHi21
            | RelocType::Aarch64AddAbsLo12Nc
            | RelocType::Aarch64Ldst64AbsLo12Nc
            | RelocType::Aarch64AdrGotPage
            | RelocType::Aarch64Ld64GotLo12Nc => {
                apply_one_reloc_aarch64(state, reloc, sym_addr, reloc_vaddr, params.got)?
            }
            // x86-64 relocations
            RelocType::X86_64
            | RelocType::X86Pc32
            | RelocType::X86Plt32
            | RelocType::X86Gotpcrel
            | RelocType::X86Gotpcrelx
            | RelocType::X86RexGotpcrelx
            | RelocType::X86_32
            | RelocType::X86_32S => {
                apply_one_reloc(state, reloc, sym_addr, reloc_vaddr, params.got)?
            }
            other => return Err(LinkError::UnsupportedRelocation {
                reloc_type: other, symbol: reloc.symbol_name.clone(),
            }),
        };
        if is_abs {
            rebase_entries.push((reloc_vaddr, sym_addr as i64 + reloc.addend));
        }
    }

    if !undefined.is_empty() {
        let mut syms: Vec<String> = undefined.into_iter().collect();
        syms.sort();
        return Err(LinkError::UndefinedSymbols(syms));
    }

    Ok(MachORelocOutput { rebase_entries, bind_entries })
}

pub(crate) struct MachORelocOutput {
    pub(crate) rebase_entries: Vec<(u64, i64)>,
    pub(crate) bind_entries: Vec<(String, u64)>,
}

pub(crate) fn apply_relocs_pe(
    state: &mut LinkState,
    layout: &PeLayout,
) -> Result<Vec<u32>, LinkError> {
    let mut undefined = HashSet::new();
    let mut abs_fixups: Vec<u32> = Vec::new(); // RVAs of absolute 64-bit fixups
    let relocs = std::mem::take(&mut state.relocs);

    for reloc in &relocs {
        // TLS not supported in UEFI PE
        if matches!(reloc.r_type,
            RelocType::X86Tlsgd | RelocType::X86Tlsld | RelocType::X86Dtpoff32
            | RelocType::X86Tpoff32 | RelocType::X86Gottpoff)
        {
            continue;
        }

        let sec = &state.sections[reloc.section];
        let reloc_vaddr = sec.vaddr.unwrap() + reloc.offset;

        let sym_addr = match resolve_symbol(state, &reloc.symbol_name, reloc.section, None) {
            Some(a) => a,
            None => {
                if reloc.symbol_name.is_empty() { 0 }
                else { undefined.insert(reloc.symbol_name.clone()); continue; }
            }
        };

        let is_abs = apply_one_reloc(state, reloc, sym_addr, reloc_vaddr, &layout.got)?;
        if is_abs {
            abs_fixups.push(reloc_vaddr as u32);
        }
    }

    // Fill GOT entries
    for (_, &got_vaddr) in &layout.got {
        abs_fixups.push(got_vaddr as u32);
    }

    if !undefined.is_empty() {
        let mut syms: Vec<String> = undefined.into_iter().collect();
        syms.sort();
        return Err(LinkError::UndefinedSymbols(syms));
    }

    abs_fixups.sort();
    Ok(abs_fixups)
}

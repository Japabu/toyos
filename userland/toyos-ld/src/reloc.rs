use crate::collect::{InputReloc, LinkState, DYNAMIC_SYMBOL_SENTINEL};
use crate::emit_pe::PeLayout;
use crate::LinkError;
use object::elf;
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
pub(crate) fn tpoff(sym_addr: u64, tls_start: u64, tls_memsz: u64) -> i64 {
    sym_addr as i64 - (tls_start as i64 + tls_memsz as i64)
}

/// Apply a single relocation, returning `true` if it's an absolute reference
/// (needs a runtime relocation / PE base fixup).
fn apply_one_reloc(
    state: &mut LinkState,
    reloc: &InputReloc,
    sym_addr: u64,
    reloc_vaddr: u64,
    got: &HashMap<String, u64>,
) -> bool {
    match reloc.r_type {
        elf::R_X86_64_64 => {
            let value = (sym_addr as i64 + reloc.addend) as u64;
            write_u64(state, reloc.section_global_idx, reloc.offset, value);
            true
        }
        elf::R_X86_64_PC32 | elf::R_X86_64_PLT32 => {
            let value = sym_addr as i64 + reloc.addend - reloc_vaddr as i64;
            write_i32(state, reloc.section_global_idx, reloc.offset, value as i32);
            false
        }
        elf::R_X86_64_32 => {
            let value = (sym_addr as i64 + reloc.addend) as u32;
            write_u32(state, reloc.section_global_idx, reloc.offset, value);
            false
        }
        elf::R_X86_64_32S => {
            let value = (sym_addr as i64 + reloc.addend) as i32;
            write_i32(state, reloc.section_global_idx, reloc.offset, value);
            false
        }
        elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX
        | elf::R_X86_64_REX_GOTPCRELX => {
            let got_slot = got[&reloc.symbol_name];
            let value = got_slot as i64 + reloc.addend - reloc_vaddr as i64;
            write_i32(state, reloc.section_global_idx, reloc.offset, value as i32);
            false
        }
        other => panic!(
            "toyos-ld: unsupported relocation type {other} for symbol {}",
            reloc.symbol_name,
        ),
    }
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
            elf::R_X86_64_TPOFF32 => {
                let tp = tpoff(sym_addr, params.tls_start, params.tls_memsz);
                write_i32(state, reloc.section_global_idx, reloc.offset,
                    (tp + reloc.addend) as i32);
            }
            elf::R_X86_64_GOTTPOFF => {
                let got_slot = params.got[&reloc.symbol_name];
                let value = got_slot as i64 + reloc.addend - reloc_vaddr as i64;
                write_i32(state, reloc.section_global_idx, reloc.offset, value as i32);
            }
            _ => {
                let is_abs = apply_one_reloc(state, reloc, sym_addr, reloc_vaddr, params.got);
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
        return Err(LinkError::UndefinedSymbols(syms));
    }

    Ok(RelocOutput { relatives, glob_dats })
}

pub(crate) fn apply_relocs_pe(
    state: &mut LinkState,
    layout: &PeLayout,
) -> Result<Vec<u32>, LinkError> {
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

        let is_abs = apply_one_reloc(state, reloc, sym_addr, reloc_vaddr, &layout.got);
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

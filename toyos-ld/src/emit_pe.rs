use crate::collect::{collect_unique_symbols, LinkState, RelocType, SymbolDef, SymbolRef};
use crate::reloc::resolve_symbol;
use crate::{align_up, classify_sections, LinkError};
use object::write::pe::{NtHeaders, Writer};
use object::pe;
use std::collections::HashMap;

const PE_FILE_ALIGNMENT: u32 = 0x200;
const PE_SECTION_ALIGNMENT: u32 = 0x1000;

// ── Layout ───────────────────────────────────────────────────────────────

pub(crate) struct PeLayout {
    pub(crate) text_rva: u32,
    pub(crate) text_virt_size: u32,
    pub(crate) data_rva: u32,
    pub(crate) data_virt_size: u32,
    pub(crate) has_data: bool,
    pub(crate) got: HashMap<SymbolRef, u64>,
}

/// PE section layout: RVAs use PE_SECTION_ALIGNMENT, file uses PE_FILE_ALIGNMENT.
/// All section vaddrs in LinkState are set relative to text_rva.
pub(crate) fn layout_pe(state: &mut LinkState) -> PeLayout {
    let buckets = classify_sections(state);

    // .text section
    let text_rva = PE_SECTION_ALIGNMENT; // first section always at 0x1000
    let mut cursor = text_rva as u64;
    for &idx in &buckets.rx {
        let sec = &mut state.sections[idx];
        cursor = align_up(cursor, sec.align);
        sec.vaddr = Some(cursor);
        cursor += sec.size;
    }
    let text_virt_size = (cursor - text_rva as u64) as u32;

    // .data section (if any RW sections exist)
    let data_rva = pe_align_up(text_rva + text_virt_size, PE_SECTION_ALIGNMENT);
    let mut data_virt_size = 0u32;
    let has_data = !buckets.rw.is_empty();
    if has_data {
        cursor = data_rva as u64;
        for &idx in &buckets.rw {
            let sec = &mut state.sections[idx];
            cursor = align_up(cursor, sec.align);
            sec.vaddr = Some(cursor);
            cursor += sec.size;
        }
        data_virt_size = (cursor - data_rva as u64) as u32;
    }

    // GOT entries needed
    let got_symbols = collect_unique_symbols(state.relocs.iter(), |r| {
        matches!(r.r_type,
            RelocType::X86Gotpcrel | RelocType::X86Gotpcrelx
            | RelocType::X86RexGotpcrelx)
    });
    if !got_symbols.is_empty() {
        cursor = if has_data { align_up(cursor, 8) } else { data_rva as u64 };
    }
    let mut got = HashMap::new();
    for sym in &got_symbols {
        got.insert(sym.clone(), cursor);
        cursor += 8;
    }
    if !got_symbols.is_empty() {
        data_virt_size = (cursor - data_rva as u64) as u32;
    }

    PeLayout {
        text_rva,
        text_virt_size,
        data_rva,
        data_virt_size,
        has_data: has_data || !got_symbols.is_empty(),
        got,
    }
}

fn pe_align_up(value: u32, alignment: u32) -> u32 {
    (value + alignment - 1) & !(alignment - 1)
}

// ── PE output ────────────────────────────────────────────────────────────

pub(crate) fn emit_pe_bytes(
    state: &LinkState,
    layout: &PeLayout,
    entry_name: &str,
    subsystem: u16,
    abs_fixups: &[u32],
) -> Result<Vec<u8>, LinkError> {
    let entry_rva = state
        .globals
        .get(entry_name)
        .map(|def| match def {
            SymbolDef::Defined { section, value, .. } => {
                (state.sections[*section].vaddr.unwrap() + value) as u32
            }
            SymbolDef::Dynamic { .. } => panic!("entry point cannot be a dynamic symbol"),
        })
        .ok_or_else(|| LinkError::MissingEntry(entry_name.to_string()))?;

    let num_sections: u16 = if layout.has_data { 3 } else { 2 }; // .text [.data] .reloc

    let mut buf = Vec::new();
    let mut w = Writer::new(true, PE_SECTION_ALIGNMENT, PE_FILE_ALIGNMENT, &mut buf);

    // ── Phase 1: Reserve ──

    w.reserve_dos_header();
    w.reserve_nt_headers(16);
    w.reserve_section_headers(num_sections);

    // .text section
    let text_range = w.reserve_text_section(layout.text_virt_size);
    debug_assert_eq!(text_range.virtual_address, layout.text_rva);

    // .data section
    let data_range = if layout.has_data {
        let range = w.reserve_data_section(layout.data_virt_size, layout.data_virt_size);
        debug_assert_eq!(range.virtual_address, layout.data_rva);
        Some(range)
    } else {
        None
    };

    // Base relocations
    for &rva in abs_fixups {
        w.add_reloc(rva, pe::IMAGE_REL_BASED_DIR64);
    }
    w.reserve_reloc_section();

    // Ensure size_of_image is section-aligned
    let aligned_vlen = pe_align_up(w.virtual_len(), PE_SECTION_ALIGNMENT);
    w.reserve_virtual_until(aligned_vlen);

    // ── Phase 2: Write ──

    w.write_empty_dos_header().unwrap();
    w.write_nt_headers(NtHeaders {
        machine: pe::IMAGE_FILE_MACHINE_AMD64,
        time_date_stamp: 0,
        characteristics: pe::IMAGE_FILE_EXECUTABLE_IMAGE | pe::IMAGE_FILE_LARGE_ADDRESS_AWARE,
        major_linker_version: 0,
        minor_linker_version: 0,
        address_of_entry_point: entry_rva,
        image_base: 0,
        major_operating_system_version: 0,
        minor_operating_system_version: 0,
        major_image_version: 0,
        minor_image_version: 0,
        major_subsystem_version: 0,
        minor_subsystem_version: 0,
        subsystem,
        dll_characteristics: pe::IMAGE_DLLCHARACTERISTICS_DYNAMIC_BASE
            | pe::IMAGE_DLLCHARACTERISTICS_HIGH_ENTROPY_VA
            | pe::IMAGE_DLLCHARACTERISTICS_NX_COMPAT,
        size_of_stack_reserve: 0x100000,
        size_of_stack_commit: 0x1000,
        size_of_heap_reserve: 0x100000,
        size_of_heap_commit: 0x1000,
    });
    w.write_section_headers();

    // ── Write section data ──

    // .text: collect all RX section data
    let mut text_data = vec![0u8; layout.text_virt_size as usize];
    for sec in &state.sections {
        let Some(vaddr) = sec.vaddr else { continue; };
        if sec.data.is_empty() { continue; }
        let rva = vaddr as u32;
        if rva >= layout.text_rva && rva < layout.text_rva + layout.text_virt_size {
            let off = (rva - layout.text_rva) as usize;
            text_data[off..off + sec.data.len()].copy_from_slice(&sec.data);
        }
    }
    w.write_section(text_range.file_offset, &text_data);

    // .data: collect all RW section data + GOT entries
    if let Some(data_range) = data_range {
        let mut data_data = vec![0u8; layout.data_virt_size as usize];
        for sec in &state.sections {
            let Some(vaddr) = sec.vaddr else { continue; };
            if sec.data.is_empty() { continue; }
            let rva = vaddr as u32;
            if rva >= layout.data_rva && rva < layout.data_rva + layout.data_virt_size {
                let off = (rva - layout.data_rva) as usize;
                data_data[off..off + sec.data.len()].copy_from_slice(&sec.data);
            }
        }
        for (sym_ref, &got_vaddr) in &layout.got {
            let sym_addr = resolve_symbol(state, sym_ref, None)
                .ok_or_else(|| LinkError::UndefinedSymbols(vec![sym_ref.name().to_string()]))?;
            let off = (got_vaddr - layout.data_rva as u64) as usize;
            data_data[off..off + 8].copy_from_slice(&sym_addr.to_le_bytes());
        }
        w.write_section(data_range.file_offset, &data_data);
    }

    // .reloc
    w.write_reloc_section();

    Ok(buf)
}

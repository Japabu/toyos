use object::write::elf::{FileHeader, ProgramHeader, SectionHeader, SectionIndex, Sym, Rel, Writer, SymbolIndex};
use object::write::StringId;
use object::Endianness;
use crate::collect::{collect_unique_symbols, InputSection, LinkState, RelocType, SectionIdx, SymbolDef};
use crate::reloc::{RelocOutput, resolve_symbol, tpoff};
use crate::{align_up, classify_sections, is_tls_section, LinkError, BASE_VADDR, PAGE_SIZE};
use object::elf;
use std::collections::{HashMap, HashSet};

// ── Layout ───────────────────────────────────────────────────────────────

pub(crate) struct ElfLayout {
    pub(crate) base_addr: u64,
    pub(crate) rx_start: u64,
    pub(crate) rx_end: u64,
    pub(crate) rw_start: u64,
    pub(crate) rw_filesz: u64,
    pub(crate) rw_end: u64,
    pub(crate) tls_start: u64,
    pub(crate) tls_filesz: u64,
    pub(crate) tls_memsz: u64,
    pub(crate) got: HashMap<String, u64>,
    pub(crate) plt: HashMap<String, u64>,
    pub(crate) plt_data: Vec<u8>,
    pub(crate) plt_vaddr: u64,
    pub(crate) dyn_got: HashMap<String, u64>,
    pub(crate) init_array_vaddr: u64,
    pub(crate) init_array_size: u64,
    pub(crate) fini_array_vaddr: u64,
    pub(crate) fini_array_size: u64,
    pub(crate) eh_frame_hdr_vaddr: u64,
    pub(crate) eh_frame_hdr_size: u64,
    pub(crate) eh_frame_vaddr: u64,
    pub(crate) build_id_note_vaddr: u64,
}

/// Size of the .note.gnu.build-id section: namesz(4) + descsz(4) + type(4) + "GNU\0"(4) + 20-byte hash
pub(crate) const BUILD_ID_NOTE_SIZE: u64 = 36;
/// Offset of the hash descriptor within the note
const BUILD_ID_DESC_OFFSET: usize = 16;
/// Size of the hash descriptor (20 bytes, same as SHA-1)
const BUILD_ID_DESC_SIZE: usize = 20;

// ── .eh_frame parsing ─────────────────────────────────────────────────────

fn read_uleb128(data: &[u8], mut offset: usize) -> (u64, usize) {
    let start = offset;
    let mut result = 0u64;
    let mut shift = 0;
    loop {
        let byte = data[offset];
        offset += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 { break; }
        shift += 7;
    }
    (result, offset - start)
}

fn read_sleb128(data: &[u8], mut offset: usize) -> (i64, usize) {
    let start = offset;
    let mut result = 0i64;
    let mut shift = 0;
    loop {
        let byte = data[offset];
        offset += 1;
        result |= ((byte & 0x7F) as i64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            if shift < 64 && byte & 0x40 != 0 {
                result |= -(1i64 << shift);
            }
            break;
        }
    }
    (result, offset - start)
}

/// Size of an encoded pointer in bytes, based on the low 4 bits of the encoding.
fn eh_pointer_size(enc: u8) -> usize {
    match enc & 0x0F {
        0x00 => 8, // DW_EH_PE_absptr (native = 8 on x86_64)
        0x02 | 0x0A => 2, // udata2 / sdata2
        0x03 | 0x0B => 4, // udata4 / sdata4
        0x04 | 0x0C => 8, // udata8 / sdata8
        _ => 0,
    }
}

/// Parse a CIE record to extract the FDE pointer encoding ('R' augmentation).
/// `cie_data` starts at the first byte after the CIE_id field.
fn parse_cie_fde_encoding(cie_data: &[u8]) -> u8 {
    let mut off = 0;
    let version = cie_data[off]; off += 1;

    // Augmentation string (null-terminated)
    let aug_start = off;
    while off < cie_data.len() && cie_data[off] != 0 { off += 1; }
    let augmentation = cie_data[aug_start..off].to_vec();
    off += 1; // skip null terminator

    // Code alignment factor (ULEB128)
    let (_, n) = read_uleb128(cie_data, off); off += n;
    // Data alignment factor (SLEB128)
    let (_, n) = read_sleb128(cie_data, off); off += n;
    // Return address register
    if version == 1 {
        off += 1;
    } else {
        let (_, n) = read_uleb128(cie_data, off); off += n;
    }

    if augmentation.first() != Some(&b'z') {
        return 0x00; // no augmentation data, assume absptr
    }

    // Augmentation data length (ULEB128)
    let (_aug_len, n) = read_uleb128(cie_data, off); off += n;

    // Parse augmentation data for each char after 'z'
    for &c in &augmentation[1..] {
        match c {
            b'L' => { off += 1; } // LSDA encoding byte
            b'P' => {
                let enc = cie_data[off]; off += 1;
                off += eh_pointer_size(enc);
            }
            b'R' => {
                return cie_data[off];
            }
            _ => break,
        }
    }

    0x00 // default: absptr
}

/// Count FDE entries in a single .eh_frame section's data.
fn count_fdes(data: &[u8]) -> usize {
    let mut count = 0;
    let mut offset = 0;
    while offset + 4 <= data.len() {
        let length = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
        if length == 0 { break; }
        let record_start = offset + 4;
        let record_end = record_start + length as usize;
        if record_end > data.len() { break; }
        let cie_id = u32::from_le_bytes(data[record_start..record_start + 4].try_into().unwrap());
        if cie_id != 0 { count += 1; }
        offset = record_end;
    }
    count
}

/// Build .eh_frame_hdr content from relocated .eh_frame sections.
/// Returns the raw bytes for the .eh_frame_hdr section.
pub(crate) fn build_eh_frame_hdr(state: &LinkState, layout: &ElfLayout) -> Vec<u8> {
    let hdr_vaddr = layout.eh_frame_hdr_vaddr;
    if hdr_vaddr == 0 { return Vec::new(); }

    // Parse each .eh_frame section to find CIE encodings and FDE entries
    let mut fdes: Vec<(u64, u64)> = Vec::new(); // (initial_location, fde_vaddr)

    for sec in &state.sections {
        if sec.name != ".eh_frame" { continue; }
        let data = &sec.data;
        let base_vaddr = sec.vaddr.unwrap();

        // First pass: build CIE offset → fde_encoding map
        let mut cie_encodings: HashMap<usize, u8> = HashMap::new();
        let mut offset = 0;
        while offset + 4 <= data.len() {
            let length = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            if length == 0 { break; }
            let record_start = offset + 4;
            let record_end = record_start + length as usize;
            if record_end > data.len() { break; }
            let cie_id = u32::from_le_bytes(data[record_start..record_start + 4].try_into().unwrap());
            if cie_id == 0 {
                // CIE: parse to find FDE encoding
                let enc = parse_cie_fde_encoding(&data[record_start + 4..record_end]);
                cie_encodings.insert(offset, enc);
            }
            offset = record_end;
        }

        // Second pass: extract FDE initial_location values
        offset = 0;
        while offset + 4 <= data.len() {
            let length = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            if length == 0 { break; }
            let record_start = offset + 4;
            let record_end = record_start + length as usize;
            if record_end > data.len() { break; }
            let cie_id = u32::from_le_bytes(data[record_start..record_start + 4].try_into().unwrap());
            if cie_id != 0 {
                // FDE: cie_pointer is relative to current position
                let cie_offset = record_start - cie_id as usize;
                let enc = cie_encodings.get(&cie_offset).copied().unwrap_or(0x1B);
                let loc_offset = record_start + 4;
                let app = enc & 0x70;
                let initial_location = match enc & 0x0F {
                    0x0B => { // sdata4
                        let val = i32::from_le_bytes(data[loc_offset..loc_offset + 4].try_into().unwrap()) as i64;
                        match app {
                            0x10 => (base_vaddr as i64 + loc_offset as i64 + val) as u64, // pcrel
                            _ => val as u64,
                        }
                    }
                    0x03 => { // udata4
                        let val = u32::from_le_bytes(data[loc_offset..loc_offset + 4].try_into().unwrap()) as i64;
                        match app {
                            0x10 => (base_vaddr as i64 + loc_offset as i64 + val) as u64,
                            _ => val as u64,
                        }
                    }
                    _ => {
                        // Default: assume sdata4 pcrel (most common on x86_64)
                        let val = i32::from_le_bytes(data[loc_offset..loc_offset + 4].try_into().unwrap()) as i64;
                        (base_vaddr as i64 + loc_offset as i64 + val) as u64
                    }
                };
                let fde_vaddr = base_vaddr + offset as u64;
                fdes.push((initial_location, fde_vaddr));
            }
            offset = record_end;
        }
    }

    fdes.sort_by_key(|&(loc, _)| loc);

    // Build .eh_frame_hdr
    let mut hdr = Vec::new();
    hdr.push(1); // version
    hdr.push(0x1B); // eh_frame_ptr encoding: DW_EH_PE_pcrel | DW_EH_PE_sdata4
    hdr.push(0x03); // fde_count encoding: DW_EH_PE_udata4
    hdr.push(0x3B); // table encoding: DW_EH_PE_datarel | DW_EH_PE_sdata4

    // eh_frame_ptr: PC-relative offset from this field to .eh_frame start
    let eh_frame_ptr = (layout.eh_frame_vaddr as i64 - (hdr_vaddr as i64 + 4)) as i32;
    hdr.extend_from_slice(&eh_frame_ptr.to_le_bytes());

    // fde_count
    hdr.extend_from_slice(&(fdes.len() as u32).to_le_bytes());

    // Sorted table: (initial_location, fde_offset) both datarel from eh_frame_hdr start
    for &(initial_location, fde_vaddr) in &fdes {
        let loc_rel = (initial_location as i64 - hdr_vaddr as i64) as i32;
        let fde_rel = (fde_vaddr as i64 - hdr_vaddr as i64) as i32;
        hdr.extend_from_slice(&loc_rel.to_le_bytes());
        hdr.extend_from_slice(&fde_rel.to_le_bytes());
    }

    hdr
}

pub(crate) fn layout_elf(state: &mut LinkState, base_addr: u64, entry_name: Option<&str>, build_id: bool) -> ElfLayout {
    let headers_size = 0x1000u64;
    let buckets = classify_sections(state);

    let mut cursor = base_addr + headers_size;

    let rx_start = cursor;
    for &idx in &buckets.rx {
        let sec = &mut state.sections[idx.0];
        cursor = align_up(cursor, sec.align);
        sec.vaddr = Some(cursor);
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

    // .eh_frame_hdr: placed after PLT stubs, before page alignment
    let mut eh_frame_vaddr = 0u64;
    let mut fde_count = 0usize;
    for &idx in &buckets.rx {
        if state.sections[idx.0].name == ".eh_frame" {
            if fde_count == 0 { eh_frame_vaddr = state.sections[idx.0].vaddr.unwrap(); }
            fde_count += count_fdes(&state.sections[idx.0].data);
        }
    }
    let (eh_frame_hdr_vaddr, eh_frame_hdr_size) = if fde_count > 0 {
        let vaddr = align_up(cursor, 4);
        let size = (12 + fde_count * 8) as u64;
        cursor = vaddr + size;
        (vaddr, size)
    } else {
        (0, 0)
    };

    // .note.gnu.build-id: placed after .eh_frame_hdr
    let build_id_note_vaddr = if build_id {
        let vaddr = align_up(cursor, 4);
        cursor = vaddr + BUILD_ID_NOTE_SIZE;
        vaddr
    } else { 0 };

    let rx_end = align_up(cursor, PAGE_SIZE);

    cursor = rx_end;
    let rw_start = cursor;

    // Place PROGBITS RW sections first
    for &idx in &buckets.rw {
        if state.sections[idx.0].nobits { continue; }
        let sec = &mut state.sections[idx.0];
        cursor = align_up(cursor, sec.align);
        sec.vaddr = Some(cursor);
        cursor += sec.size;
    }

    // Compute init/fini array ranges (sections are contiguous due to RW sort order)
    let mut init_array_vaddr = 0u64;
    let mut init_array_size = 0u64;
    let mut fini_array_vaddr = 0u64;
    let mut fini_array_size = 0u64;
    for &idx in &buckets.rw {
        let sec = &state.sections[idx.0];
        let Some(sec_vaddr) = sec.vaddr else { continue; };
        if sec.name.starts_with(".init_array") {
            if init_array_size == 0 { init_array_vaddr = sec_vaddr; }
            init_array_size = (sec_vaddr + sec.size) - init_array_vaddr;
        } else if sec.name.starts_with(".fini_array") {
            if fini_array_size == 0 { fini_array_vaddr = sec_vaddr; }
            fini_array_size = (sec_vaddr + sec.size) - fini_array_vaddr;
        }
    }

    let got_symbols = collect_unique_symbols(state.relocs.iter(), |r| {
        matches!(r.r_type,
            RelocType::X86Gotpcrel | RelocType::X86Gotpcrelx
            | RelocType::X86RexGotpcrelx | RelocType::X86Gottpoff)
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

    let rw_filesz = cursor - rw_start;

    // Place NOBITS RW sections (.bss) after all file-backed data
    for &idx in &buckets.rw {
        if !state.sections[idx.0].nobits { continue; }
        let sec = &mut state.sections[idx.0];
        cursor = align_up(cursor, sec.align);
        sec.vaddr = Some(cursor);
        cursor += sec.size;
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
    for &idx in &buckets.tls {
        let sec = &mut state.sections[idx.0];
        tls_cursor = align_up(tls_cursor, sec.align);
        sec.vaddr = Some(tls_cursor);
        tls_cursor += sec.size;
    }
    let tls_filesz = buckets.tls
        .iter()
        .filter(|&&idx| !state.sections[idx.0].name.starts_with(".tbss"))
        .map(|&idx| state.sections[idx.0].size)
        .sum::<u64>();
    let tls_memsz = if buckets.tls.is_empty() { 0 } else { tls_cursor - tls_start };

    ElfLayout {
        base_addr, rx_start, rx_end, rw_start, rw_filesz, rw_end,
        tls_start, tls_filesz, tls_memsz,
        got, plt, plt_data, plt_vaddr, dyn_got,
        init_array_vaddr, init_array_size,
        fini_array_vaddr, fini_array_size,
        eh_frame_hdr_vaddr, eh_frame_hdr_size, eh_frame_vaddr,
        build_id_note_vaddr,
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// GNU hash function for .gnu.hash section.
fn gnu_hash(name: &[u8]) -> u32 {
    let mut h = 5381u32;
    for &b in name {
        h = h.wrapping_mul(33).wrapping_add(b as u32);
    }
    h
}

/// Build the .note.gnu.build-id note content with a zero descriptor placeholder.
fn build_id_note_placeholder() -> Vec<u8> {
    let mut note = Vec::with_capacity(BUILD_ID_NOTE_SIZE as usize);
    note.extend_from_slice(&4u32.to_le_bytes());             // namesz
    note.extend_from_slice(&(BUILD_ID_DESC_SIZE as u32).to_le_bytes()); // descsz
    note.extend_from_slice(&3u32.to_le_bytes());             // type = NT_GNU_BUILD_ID
    note.extend_from_slice(b"GNU\0");                        // name
    note.extend_from_slice(&[0u8; BUILD_ID_DESC_SIZE]);      // descriptor (placeholder)
    note
}

/// Compute a 20-byte build-id hash from the output binary.
/// Uses multiple rounds of SipHash with different seeds.
pub(crate) fn compute_build_id(data: &[u8]) -> [u8; BUILD_ID_DESC_SIZE] {
    use std::hash::{Hash, Hasher};
    use std::collections::hash_map::DefaultHasher;
    let mut result = [0u8; BUILD_ID_DESC_SIZE];
    for chunk in 0..3u64 {
        let mut hasher = DefaultHasher::new();
        chunk.hash(&mut hasher);
        data.hash(&mut hasher);
        let h = hasher.finish().to_le_bytes();
        let start = (chunk as usize) * 8;
        let end = (start + 8).min(BUILD_ID_DESC_SIZE);
        result[start..end].copy_from_slice(&h[..end - start]);
    }
    result
}

/// Patch the build-id descriptor in an ELF binary at the given file offset.
pub(crate) fn patch_build_id(buf: &mut [u8], note_file_offset: usize) {
    // Zero out the descriptor before computing hash (it's already zero for fresh emit)
    let desc_start = note_file_offset + BUILD_ID_DESC_OFFSET;
    let desc_end = desc_start + BUILD_ID_DESC_SIZE;
    for b in &mut buf[desc_start..desc_end] { *b = 0; }
    let hash = compute_build_id(buf);
    buf[desc_start..desc_end].copy_from_slice(&hash);
}

fn resolve_entry(state: &LinkState, entry_name: &str, plt: Option<&HashMap<String, u64>>) -> Result<u64, LinkError> {
    state
        .globals
        .get(entry_name)
        .map(|def| match def {
            SymbolDef::Dynamic => {
                plt.and_then(|p| p.get(entry_name).copied())
                    .ok_or_else(|| LinkError::MissingEntry(entry_name.to_string()))
            }
            SymbolDef::Defined { section, value } => {
                Ok(state.sections[section.0].vaddr.unwrap() + value)
            }
        })
        .unwrap_or_else(|| Err(LinkError::MissingEntry(entry_name.to_string())))
}

/// Map an input section to an output section index (1 = .text, 2 = .data).
fn output_section_index(sec: &InputSection, text_idx: SectionIndex, data_idx: SectionIndex) -> SectionIndex {
    if sec.writable || sec.nobits || is_tls_section(&sec.name) {
        data_idx
    } else {
        text_idx
    }
}

/// A prepared symbol for the .symtab output.
struct SymEntry {
    name_id: StringId,
    st_value: u64,
    st_size: u64,
    st_info: u8,
    section_idx: SectionIndex,
    is_local: bool,
}

/// Collect symbols from LinkState and prepare them for .symtab emission.
/// Returns (entries sorted locals-first, num_local including null symbol).
fn collect_symtab_entries<'a>(
    state: &'a LinkState,
    w: &mut Writer<'a>,
    text_idx: SectionIndex,
    data_idx: SectionIndex,
) -> (Vec<SymEntry>, u32) {
    let mut entries = Vec::new();

    // Locals
    for ((_, name), def) in &state.locals {
        let SymbolDef::Defined { section, value } = def else { continue; };
        let sec = &state.sections[section.0];
        let st_value = sec.vaddr.unwrap() + value;
        let out_idx = output_section_index(sec, text_idx, data_idx);
        let st_type = if sec.writable || sec.nobits || is_tls_section(&sec.name) {
            elf::STT_OBJECT
        } else {
            elf::STT_FUNC
        };
        entries.push(SymEntry {
            name_id: w.add_string(name.as_bytes()),
            st_value,
            st_size: 0,
            st_info: (elf::STB_LOCAL << 4) | st_type,
            section_idx: out_idx,
            is_local: true,
        });
    }

    // Globals
    for (name, def) in &state.globals {
        let SymbolDef::Defined { section, value } = def else { continue; };
        let sec = &state.sections[section.0];
        let st_value = sec.vaddr.unwrap() + value;
        let out_idx = output_section_index(sec, text_idx, data_idx);
        let st_type = if sec.writable || sec.nobits || is_tls_section(&sec.name) {
            elf::STT_OBJECT
        } else {
            elf::STT_FUNC
        };
        entries.push(SymEntry {
            name_id: w.add_string(name.as_bytes()),
            st_value,
            st_size: 0,
            st_info: (elf::STB_GLOBAL << 4) | st_type,
            section_idx: out_idx,
            is_local: false,
        });
    }

    // Sort: locals first, then globals
    entries.sort_by_key(|e| !e.is_local);
    let num_local = entries.iter().filter(|e| e.is_local).count() as u32 + 1; // +1 for null symbol

    (entries, num_local)
}

/// Reserve section indices and symbol indices for symtab/strtab.
/// Call after add_string, before reserve_shstrtab_section_index.
fn reserve_symtab_indices(w: &mut Writer, entries: &[SymEntry]) {
    for e in entries {
        w.reserve_symbol_index(Some(e.section_idx));
    }
    w.reserve_symtab_section_index();
    w.reserve_strtab_section_index();
}

/// Reserve file ranges for symtab/strtab data.
/// Call after reserving loadable content, before reserve_shstrtab.
fn reserve_symtab_data(w: &mut Writer) {
    w.reserve_symtab();
    w.reserve_strtab();
}

/// Write symtab/strtab data.
fn write_symtab(w: &mut Writer, entries: &[SymEntry]) {
    w.write_null_symbol();
    for e in entries {
        w.write_symbol(&Sym {
            name: Some(e.name_id),
            section: Some(e.section_idx),
            st_info: e.st_info,
            st_other: elf::STV_DEFAULT,
            st_shndx: 0,
            st_value: e.st_value,
            st_size: e.st_size,
        });
    }
    w.write_strtab();
}

/// Write section data to the Writer in file-offset order. Skips NOBITS sections.
fn write_sections_data(w: &mut Writer, sections: &[InputSection], base: u64) {
    let mut indices: Vec<usize> = (0..sections.len())
        .filter(|&i| sections[i].vaddr.is_some() && !sections[i].data.is_empty() && !sections[i].nobits)
        .collect();
    indices.sort_by_key(|&i| sections[i].vaddr.unwrap());
    for i in indices {
        let file_off = (sections[i].vaddr.unwrap() - base) as usize;
        w.pad_until(file_off);
        w.write(&sections[i].data);
    }
}

// ── PIE ELF output ───────────────────────────────────────────────────────

pub(crate) fn emit_bytes(
    state: &LinkState,
    layout: &ElfLayout,
    relocs: &RelocOutput,
    entry_name: &str,
    eh_frame_hdr: &[u8],
) -> Result<Vec<u8>, LinkError> {
    let is_dynamic = !state.dynamic_libs.is_empty();
    let needs_dynamic = is_dynamic || layout.init_array_size > 0 || layout.fini_array_size > 0;
    let entry = resolve_entry(state, entry_name, Some(&layout.plt))?;
    let file_rw_end = layout.rw_start + layout.rw_filesz;

    let mut buf = Vec::new();
    let mut w = Writer::new(Endianness::Little, true, &mut buf);

    // ── Phase 1: Add strings and reserve ──

    let has_eh_frame_hdr = !eh_frame_hdr.is_empty();
    let has_build_id = layout.build_id_note_vaddr != 0;

    // Section names (only for sections without convenience methods)
    let text_name = w.add_section_name(b".text");
    let data_name = w.add_section_name(b".data");
    let rela_name = w.add_section_name(b".rela.dyn");
    let eh_frame_hdr_name = if has_eh_frame_hdr {
        Some(w.add_section_name(b".eh_frame_hdr"))
    } else { None };
    let build_id_name = if has_build_id {
        Some(w.add_section_name(b".note.gnu.build-id"))
    } else { None };

    // Dynamic strings
    let mut needed_str_ids = Vec::new();
    let mut sym_str_ids: Vec<(String, StringId)> = Vec::new();
    let mut sym_to_writer_idx: HashMap<String, SymbolIndex> = HashMap::new();

    if is_dynamic {
        for lib in &state.dynamic_libs {
            needed_str_ids.push(w.add_dynamic_string(lib.as_bytes()));
        }
        for (_, sym_name) in &relocs.glob_dats {
            if !sym_to_writer_idx.contains_key(sym_name) {
                let str_id = w.add_dynamic_string(sym_name.as_bytes());
                sym_str_ids.push((sym_name.clone(), str_id));
                // placeholder — real index comes from reserve below
                sym_to_writer_idx.insert(sym_name.clone(), SymbolIndex(0));
            }
        }
    }

    // Reserve section indices
    w.reserve_null_section_index();
    let text_sec_idx = w.reserve_section_index(); // .text
    let data_sec_idx = w.reserve_section_index(); // .data

    let dynsym_sec_idx = if is_dynamic {
        Some(w.reserve_dynsym_section_index())
    } else { None };
    if is_dynamic {
        w.reserve_dynstr_section_index();
    }
    w.reserve_section_index(); // .rela.dyn
    if needs_dynamic {
        w.reserve_dynamic_section_index();
    }
    if has_eh_frame_hdr {
        w.reserve_section_index(); // .eh_frame_hdr
    }
    if has_build_id {
        w.reserve_section_index(); // .note.gnu.build-id
    }

    // Symtab: collect symbols and add strings before reserving section indices
    let (sym_entries, num_local) = collect_symtab_entries(state, &mut w, text_sec_idx, data_sec_idx);
    reserve_symtab_indices(&mut w, &sym_entries);

    w.reserve_shstrtab_section_index();

    // Reserve dynamic symbol indices
    if is_dynamic {
        w.reserve_null_dynamic_symbol_index();
        for (sym_name, _) in &sym_str_ids {
            let idx = w.reserve_dynamic_symbol_index();
            sym_to_writer_idx.insert(sym_name.clone(), idx);
        }
    }

    // Reserve file layout
    w.reserve_file_header();
    let phdr_count = 2 + if layout.tls_memsz > 0 { 1 } else { 0 }
        + if needs_dynamic { 2 } else { 0 }
        + if has_eh_frame_hdr { 1 } else { 0 }
        + if has_build_id { 1 } else { 0 };
    w.reserve_program_headers(phdr_count as u32);

    // Reserve section data area (excludes NOBITS/bss)
    w.reserve_until(file_rw_end as usize);

    // Dynamic metadata
    let rela_count = relocs.relatives.len() + if is_dynamic { relocs.glob_dats.len() } else { 0 };
    let rela_size = rela_count as u64 * 24;

    let (dynsym_off, dynstr_off);
    if is_dynamic {
        dynsym_off = w.reserve_dynsym() as u64;
        dynstr_off = w.reserve_dynstr() as u64;
    } else {
        dynsym_off = 0;
        dynstr_off = 0;
    }

    let rela_off = w.reserve_relocations(rela_count, true) as u64;

    let (dynamic_count, dynamic_off, dyn_segment_end);
    if needs_dynamic {
        let mut dc = 1; // DT_NULL
        if is_dynamic { dc += needed_str_ids.len() + 4; } // DT_NEEDED * N + SYMTAB/STRTAB/STRSZ/SYMENT
        if rela_count > 0 { dc += 3; } // DT_RELA + DT_RELASZ + DT_RELAENT
        if layout.init_array_size > 0 { dc += 2; }
        if layout.fini_array_size > 0 { dc += 2; }
        dynamic_count = dc;
        dynamic_off = w.reserve_dynamic(dc) as u64;
        dyn_segment_end = align_up(w.reserved_len() as u64, PAGE_SIZE);
        w.reserve_until(dyn_segment_end as usize);
    } else {
        dynamic_count = 0;
        dynamic_off = 0;
        dyn_segment_end = 0;
    }

    reserve_symtab_data(&mut w);
    w.reserve_shstrtab();
    w.reserve_section_headers();

    // ── Phase 2: Write ──

    // File header
    w.write_file_header(&FileHeader {
        os_abi: 0,
        abi_version: 0,
        e_type: elf::ET_DYN,
        e_machine: elf::EM_X86_64,
        e_entry: entry,
        e_flags: 0,
    }).unwrap();

    // Program headers
    w.write_align_program_headers();
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R | elf::PF_X,
        p_offset: BASE_VADDR,
        p_vaddr: BASE_VADDR,
        p_paddr: BASE_VADDR,
        p_filesz: layout.rx_end - BASE_VADDR,
        p_memsz: layout.rx_end - BASE_VADDR,
        p_align: PAGE_SIZE,
    });
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R | elf::PF_W,
        p_offset: layout.rw_start,
        p_vaddr: layout.rw_start,
        p_paddr: layout.rw_start,
        p_filesz: layout.rw_filesz,
        p_memsz: layout.rw_end - layout.rw_start,
        p_align: PAGE_SIZE,
    });
    if layout.tls_memsz > 0 {
        w.write_program_header(&ProgramHeader {
            p_type: elf::PT_TLS,
            p_flags: elf::PF_R,
            p_offset: layout.tls_start,
            p_vaddr: layout.tls_start,
            p_paddr: layout.tls_start,
            p_filesz: layout.tls_filesz,
            p_memsz: layout.tls_memsz,
            p_align: 64,
        });
    }
    if has_eh_frame_hdr {
        w.write_program_header(&ProgramHeader {
            p_type: 0x6474_e550, // PT_GNU_EH_FRAME
            p_flags: elf::PF_R,
            p_offset: layout.eh_frame_hdr_vaddr,
            p_vaddr: layout.eh_frame_hdr_vaddr,
            p_paddr: layout.eh_frame_hdr_vaddr,
            p_filesz: layout.eh_frame_hdr_size,
            p_memsz: layout.eh_frame_hdr_size,
            p_align: 4,
        });
    }
    if has_build_id {
        w.write_program_header(&ProgramHeader {
            p_type: elf::PT_NOTE,
            p_flags: elf::PF_R,
            p_offset: layout.build_id_note_vaddr,
            p_vaddr: layout.build_id_note_vaddr,
            p_paddr: layout.build_id_note_vaddr,
            p_filesz: BUILD_ID_NOTE_SIZE,
            p_memsz: BUILD_ID_NOTE_SIZE,
            p_align: 4,
        });
    }
    if needs_dynamic {
        let dyn_load_start = if is_dynamic {
            dynsym_off
        } else if rela_count > 0 {
            rela_off
        } else {
            dynamic_off
        };
        let dynamic_size = dynamic_count as u64 * 16;
        w.write_program_header(&ProgramHeader {
            p_type: elf::PT_LOAD,
            p_flags: elf::PF_R,
            p_offset: dyn_load_start,
            p_vaddr: dyn_load_start,
            p_paddr: dyn_load_start,
            p_filesz: dyn_segment_end - dyn_load_start,
            p_memsz: dyn_segment_end - dyn_load_start,
            p_align: PAGE_SIZE,
        });
        w.write_program_header(&ProgramHeader {
            p_type: elf::PT_DYNAMIC,
            p_flags: elf::PF_R,
            p_offset: dynamic_off,
            p_vaddr: dynamic_off,
            p_paddr: dynamic_off,
            p_filesz: dynamic_size,
            p_memsz: dynamic_size,
            p_align: 8,
        });
    }

    // Section data
    write_sections_data(&mut w, &state.sections, BASE_VADDR);
    if !layout.plt_data.is_empty() {
        w.pad_until(layout.plt_vaddr as usize);
        w.write(&layout.plt_data);
    }
    if has_eh_frame_hdr {
        w.pad_until(layout.eh_frame_hdr_vaddr as usize);
        w.write(eh_frame_hdr);
    }
    if has_build_id {
        w.pad_until(layout.build_id_note_vaddr as usize);
        w.write(&build_id_note_placeholder());
    }
    w.pad_until(file_rw_end as usize);

    // Dynamic symbols
    if is_dynamic {
        w.write_null_dynamic_symbol();
        for (_, str_id) in &sym_str_ids {
            w.write_dynamic_symbol(&Sym {
                name: Some(*str_id),
                section: None,
                st_info: (elf::STB_GLOBAL << 4) | elf::STT_NOTYPE,
                st_other: elf::STV_DEFAULT,
                st_shndx: 0,
                st_value: 0,
                st_size: 0,
            });
        }
        w.write_dynstr();
    }

    // Relocations
    w.write_align_relocation();
    for &(offset, addend) in &relocs.relatives {
        w.write_relocation(true, &Rel {
            r_offset: offset,
            r_sym: 0,
            r_type: elf::R_X86_64_RELATIVE,
            r_addend: addend,
        });
    }
    if is_dynamic {
        for (got_vaddr, sym_name) in &relocs.glob_dats {
            let sym_idx = sym_to_writer_idx[sym_name];
            w.write_relocation(true, &Rel {
                r_offset: *got_vaddr,
                r_sym: sym_idx.0,
                r_type: elf::R_X86_64_GLOB_DAT,
                r_addend: 0,
            });
        }
    }

    // Dynamic section
    if needs_dynamic {
        w.write_align_dynamic();
        if is_dynamic {
            for &str_id in &needed_str_ids {
                w.write_dynamic_string(elf::DT_NEEDED as u32, str_id);
            }
            w.write_dynamic(elf::DT_SYMTAB as u32, dynsym_off);
            w.write_dynamic(elf::DT_STRTAB as u32, dynstr_off);
            let strsz = w.dynstr_len() as u64;
            w.write_dynamic(elf::DT_STRSZ as u32, strsz);
            w.write_dynamic(elf::DT_SYMENT as u32, 24);
        }
        if rela_count > 0 {
            w.write_dynamic(elf::DT_RELA as u32, rela_off);
            w.write_dynamic(elf::DT_RELASZ as u32, rela_size);
            w.write_dynamic(elf::DT_RELAENT as u32, 24);
        }
        if layout.init_array_size > 0 {
            w.write_dynamic(elf::DT_INIT_ARRAY as u32, layout.init_array_vaddr);
            w.write_dynamic(elf::DT_INIT_ARRAYSZ as u32, layout.init_array_size);
        }
        if layout.fini_array_size > 0 {
            w.write_dynamic(elf::DT_FINI_ARRAY as u32, layout.fini_array_vaddr);
            w.write_dynamic(elf::DT_FINI_ARRAYSZ as u32, layout.fini_array_size);
        }
        w.write_dynamic(elf::DT_NULL as u32, 0);
        w.pad_until(dyn_segment_end as usize);
    }

    // Symtab + strtab
    write_symtab(&mut w, &sym_entries);

    // shstrtab
    w.write_shstrtab();

    // Section headers
    w.write_null_section_header();
    w.write_section_header(&SectionHeader {
        name: Some(text_name),
        sh_type: elf::SHT_PROGBITS,
        sh_flags: (elf::SHF_ALLOC | elf::SHF_EXECINSTR) as u64,
        sh_addr: layout.rx_start,
        sh_offset: layout.rx_start - BASE_VADDR,
        sh_size: layout.rx_end - layout.rx_start,
        sh_link: 0, sh_info: 0, sh_addralign: 1, sh_entsize: 0,
    });
    w.write_section_header(&SectionHeader {
        name: Some(data_name),
        sh_type: elf::SHT_PROGBITS,
        sh_flags: (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
        sh_addr: layout.rw_start,
        sh_offset: layout.rw_start - BASE_VADDR,
        sh_size: layout.rw_filesz,
        sh_link: 0, sh_info: 0, sh_addralign: 1, sh_entsize: 0,
    });

    if is_dynamic {
        w.write_dynsym_section_header(dynsym_off, 1);
        w.write_dynstr_section_header(dynstr_off);
    }

    // .rela.dyn
    w.write_section_header(&SectionHeader {
        name: Some(rela_name),
        sh_type: elf::SHT_RELA,
        sh_flags: elf::SHF_ALLOC as u64,
        sh_addr: if needs_dynamic { rela_off } else { 0 },
        sh_offset: rela_off,
        sh_size: rela_size,
        sh_link: if is_dynamic { dynsym_sec_idx.unwrap().0 } else { 0 },
        sh_info: 0,
        sh_addralign: 8,
        sh_entsize: 24,
    });

    if needs_dynamic {
        w.write_dynamic_section_header(dynamic_off);
    }
    if has_eh_frame_hdr {
        w.write_section_header(&SectionHeader {
            name: eh_frame_hdr_name,
            sh_type: elf::SHT_PROGBITS,
            sh_flags: elf::SHF_ALLOC as u64,
            sh_addr: layout.eh_frame_hdr_vaddr,
            sh_offset: layout.eh_frame_hdr_vaddr,
            sh_size: layout.eh_frame_hdr_size,
            sh_link: 0, sh_info: 0, sh_addralign: 4, sh_entsize: 0,
        });
    }
    if has_build_id {
        w.write_section_header(&SectionHeader {
            name: build_id_name,
            sh_type: elf::SHT_NOTE,
            sh_flags: elf::SHF_ALLOC as u64,
            sh_addr: layout.build_id_note_vaddr,
            sh_offset: layout.build_id_note_vaddr,
            sh_size: BUILD_ID_NOTE_SIZE,
            sh_link: 0, sh_info: 0, sh_addralign: 4, sh_entsize: 0,
        });
    }

    w.write_symtab_section_header(num_local);
    w.write_strtab_section_header();
    w.write_shstrtab_section_header();

    // Patch build-id descriptor with computed hash
    if has_build_id {
        patch_build_id(&mut buf, layout.build_id_note_vaddr as usize);
    }

    Ok(buf)
}

// ── Static ELF output ────────────────────────────────────────────────────

pub(crate) fn emit_static_bytes(
    state: &LinkState,
    layout: &ElfLayout,
    entry_name: &str,
) -> Result<Vec<u8>, LinkError> {
    let entry = resolve_entry(state, entry_name, None)?;
    let base = layout.base_addr;
    let file_rw_end = layout.rw_start + layout.rw_filesz;

    let mut buf = Vec::new();
    let mut w = Writer::new(Endianness::Little, true, &mut buf);

    let has_build_id = layout.build_id_note_vaddr != 0;

    // ── Phase 1: Reserve ──
    let text_name = w.add_section_name(b".text");
    let data_name = w.add_section_name(b".data");
    let build_id_name = if has_build_id {
        Some(w.add_section_name(b".note.gnu.build-id"))
    } else { None };

    w.reserve_null_section_index();
    let text_sec_idx = w.reserve_section_index(); // .text
    let data_sec_idx = w.reserve_section_index(); // .data
    if has_build_id {
        w.reserve_section_index(); // .note.gnu.build-id
    }

    let (sym_entries, num_local) = collect_symtab_entries(state, &mut w, text_sec_idx, data_sec_idx);
    reserve_symtab_indices(&mut w, &sym_entries);

    w.reserve_shstrtab_section_index();

    w.reserve_file_header();
    let phdr_count = 2 + if layout.tls_memsz > 0 { 1 } else { 0 }
        + if has_build_id { 1 } else { 0 };
    w.reserve_program_headers(phdr_count as u32);

    // Section data + GOT entries (excludes NOBITS/bss)
    w.reserve_until((file_rw_end - base) as usize);

    reserve_symtab_data(&mut w);
    w.reserve_shstrtab();
    w.reserve_section_headers();

    // ── Phase 2: Write ──
    w.write_file_header(&FileHeader {
        os_abi: 0,
        abi_version: 0,
        e_type: elf::ET_EXEC,
        e_machine: elf::EM_X86_64,
        e_entry: entry,
        e_flags: 0,
    }).unwrap();

    // Program headers
    w.write_align_program_headers();
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R | elf::PF_X,
        p_offset: 0,
        p_vaddr: base,
        p_paddr: base,
        p_filesz: layout.rx_end - base,
        p_memsz: layout.rx_end - base,
        p_align: PAGE_SIZE,
    });
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R | elf::PF_W,
        p_offset: layout.rw_start - base,
        p_vaddr: layout.rw_start,
        p_paddr: layout.rw_start,
        p_filesz: layout.rw_filesz,
        p_memsz: layout.rw_end - layout.rw_start,
        p_align: PAGE_SIZE,
    });
    if layout.tls_memsz > 0 {
        w.write_program_header(&ProgramHeader {
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
    if has_build_id {
        w.write_program_header(&ProgramHeader {
            p_type: elf::PT_NOTE,
            p_flags: elf::PF_R,
            p_offset: layout.build_id_note_vaddr - base,
            p_vaddr: layout.build_id_note_vaddr,
            p_paddr: layout.build_id_note_vaddr,
            p_filesz: BUILD_ID_NOTE_SIZE,
            p_memsz: BUILD_ID_NOTE_SIZE,
            p_align: 4,
        });
    }

    // Section data
    write_sections_data(&mut w, &state.sections, base);
    if has_build_id {
        let file_off = (layout.build_id_note_vaddr - base) as usize;
        w.pad_until(file_off);
        w.write(&build_id_note_placeholder());
    }

    // GOT entries (filled directly in static mode)
    let gottpoff_syms: HashSet<String> = state.relocs.iter()
        .filter(|r| r.r_type == RelocType::X86Gottpoff)
        .map(|r| r.symbol_name.clone()).collect();
    let mut got_entries: Vec<_> = layout.got.iter().collect();
    got_entries.sort_by_key(|(_, &vaddr)| vaddr);
    for (sym_name, &got_vaddr) in got_entries {
        let sym_addr = resolve_symbol(state, sym_name, SectionIdx(0), None)
            .ok_or_else(|| LinkError::UndefinedSymbols(vec![sym_name.clone()]))?;
        let value = if gottpoff_syms.contains(sym_name) {
            tpoff(sym_addr, layout.tls_start, layout.tls_memsz) as u64
        } else { sym_addr };
        let file_off = (got_vaddr - base) as usize;
        w.pad_until(file_off);
        w.write(&value.to_le_bytes());
    }

    w.pad_until((file_rw_end - base) as usize);

    // Symtab + strtab
    write_symtab(&mut w, &sym_entries);

    // shstrtab + section headers
    w.write_shstrtab();

    w.write_null_section_header();
    w.write_section_header(&SectionHeader {
        name: Some(text_name),
        sh_type: elf::SHT_PROGBITS,
        sh_flags: (elf::SHF_ALLOC | elf::SHF_EXECINSTR) as u64,
        sh_addr: layout.rx_start,
        sh_offset: layout.rx_start - base,
        sh_size: layout.rx_end - layout.rx_start,
        sh_link: 0, sh_info: 0, sh_addralign: 1, sh_entsize: 0,
    });
    w.write_section_header(&SectionHeader {
        name: Some(data_name),
        sh_type: elf::SHT_PROGBITS,
        sh_flags: (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
        sh_addr: layout.rw_start,
        sh_offset: layout.rw_start - base,
        sh_size: layout.rw_filesz,
        sh_link: 0, sh_info: 0, sh_addralign: 1, sh_entsize: 0,
    });
    if has_build_id {
        w.write_section_header(&SectionHeader {
            name: build_id_name,
            sh_type: elf::SHT_NOTE,
            sh_flags: elf::SHF_ALLOC as u64,
            sh_addr: layout.build_id_note_vaddr,
            sh_offset: layout.build_id_note_vaddr - base,
            sh_size: BUILD_ID_NOTE_SIZE,
            sh_link: 0, sh_info: 0, sh_addralign: 4, sh_entsize: 0,
        });
    }
    w.write_symtab_section_header(num_local);
    w.write_strtab_section_header();
    w.write_shstrtab_section_header();

    if has_build_id {
        patch_build_id(&mut buf, (layout.build_id_note_vaddr - base) as usize);
    }

    Ok(buf)
}

// ── Shared library output ────────────────────────────────────────────────

pub(crate) fn emit_shared_bytes(
    state: &LinkState,
    layout: &ElfLayout,
    relocs: &RelocOutput,
    eh_frame_hdr: &[u8],
) -> Vec<u8> {
    let file_rw_end = layout.rw_start + layout.rw_filesz;
    let has_eh_frame_hdr = !eh_frame_hdr.is_empty();
    let has_build_id = layout.build_id_note_vaddr != 0;

    let mut buf = Vec::new();
    let mut w = Writer::new(Endianness::Little, true, &mut buf);

    // ── Phase 1: Reserve ──

    let text_name = w.add_section_name(b".text");
    let data_name = w.add_section_name(b".data");
    let rela_name = w.add_section_name(b".rela.dyn");
    let eh_frame_hdr_name = if has_eh_frame_hdr {
        Some(w.add_section_name(b".eh_frame_hdr"))
    } else { None };
    let build_id_name = if has_build_id {
        Some(w.add_section_name(b".note.gnu.build-id"))
    } else { None };

    // Add dynamic strings for exported symbols, sorted by GNU hash bucket
    let mut symbols: Vec<_> = state.globals.iter().collect();
    symbols.sort_by_key(|(name, _)| *name);
    let mut sym_str_ids: Vec<(String, StringId, u64, u32)> = Vec::new();
    for (name, def) in &symbols {
        let SymbolDef::Defined { section, value } = def else { continue; };
        let str_id = w.add_dynamic_string(name.as_bytes());
        let st_value = state.sections[section.0].vaddr.unwrap() + value;
        let hash = gnu_hash(name.as_bytes());
        sym_str_ids.push((name.to_string(), str_id, st_value, hash));
    }
    let gnu_hash_sym_count = sym_str_ids.len() as u32;
    let gnu_hash_bucket_count = gnu_hash_sym_count.max(1);
    let gnu_hash_bloom_count = 1u32; // power of 2, sufficient for small tables
    // Sort by GNU hash bucket for .gnu.hash requirements
    sym_str_ids.sort_by_key(|&(_, _, _, h)| h % gnu_hash_bucket_count);

    // Metadata section names
    let mut meta_names: Vec<StringId> = Vec::new();
    for (name, _) in &state.metadata {
        meta_names.push(w.add_section_name(name.as_bytes()));
    }

    // Reserve section indices
    w.reserve_null_section_index();
    let text_sec_idx = w.reserve_section_index(); // .text
    let data_sec_idx = w.reserve_section_index(); // .data
    w.reserve_section_index(); // .rela.dyn
    w.reserve_dynsym_section_index();
    w.reserve_dynstr_section_index();
    w.reserve_dynamic_section_index();
    w.reserve_gnu_hash_section_index();
    if has_eh_frame_hdr {
        w.reserve_section_index(); // .eh_frame_hdr
    }
    if has_build_id {
        w.reserve_section_index(); // .note.gnu.build-id
    }
    for _ in &state.metadata {
        w.reserve_section_index(); // metadata sections
    }

    let (sym_entries, num_local) = collect_symtab_entries(state, &mut w, text_sec_idx, data_sec_idx);
    reserve_symtab_indices(&mut w, &sym_entries);

    w.reserve_shstrtab_section_index();

    // Reserve dynamic symbol indices
    w.reserve_null_dynamic_symbol_index();
    for _ in &sym_str_ids {
        w.reserve_dynamic_symbol_index();
    }

    // Reserve file layout
    w.reserve_file_header();
    let phdr_count = 4 + if layout.tls_memsz > 0 { 1 } else { 0 }
        + if has_eh_frame_hdr { 1 } else { 0 }
        + if has_build_id { 1 } else { 0 };
    w.reserve_program_headers(phdr_count as u32);

    w.reserve_until(file_rw_end as usize);

    let dynsym_off = w.reserve_dynsym() as u64;
    let dynstr_off = w.reserve_dynstr() as u64;
    let gnu_hash_off = w.reserve_gnu_hash(gnu_hash_bloom_count, gnu_hash_bucket_count, gnu_hash_sym_count) as u64;
    let mut dynamic_count = 6; // SYMTAB + STRTAB + STRSZ + SYMENT + GNU_HASH + NULL
    if layout.init_array_size > 0 { dynamic_count += 2; }
    if layout.fini_array_size > 0 { dynamic_count += 2; }
    let dynamic_off = w.reserve_dynamic(dynamic_count) as u64;
    let dyn_segment_end = align_up(w.reserved_len() as u64, PAGE_SIZE);
    w.reserve_until(dyn_segment_end as usize);

    let rela_count = relocs.relatives.len();
    let rela_size = rela_count as u64 * 24;
    let rela_off = w.reserve_relocations(rela_count, true) as u64;

    // Metadata sections
    let mut meta_offsets = Vec::new();
    for (_, data) in &state.metadata {
        let off = w.reserve(data.len(), 8);
        meta_offsets.push(off as u64);
    }

    reserve_symtab_data(&mut w);
    w.reserve_shstrtab();
    w.reserve_section_headers();

    // ── Phase 2: Write ──

    w.write_file_header(&FileHeader {
        os_abi: 0,
        abi_version: 0,
        e_type: elf::ET_DYN,
        e_machine: elf::EM_X86_64,
        e_entry: 0,
        e_flags: 0,
    }).unwrap();

    // Program headers
    w.write_align_program_headers();
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R | elf::PF_X,
        p_offset: BASE_VADDR,
        p_vaddr: BASE_VADDR,
        p_paddr: BASE_VADDR,
        p_filesz: layout.rx_end - BASE_VADDR,
        p_memsz: layout.rx_end - BASE_VADDR,
        p_align: PAGE_SIZE,
    });
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R | elf::PF_W,
        p_offset: layout.rw_start,
        p_vaddr: layout.rw_start,
        p_paddr: layout.rw_start,
        p_filesz: layout.rw_filesz,
        p_memsz: layout.rw_end - layout.rw_start,
        p_align: PAGE_SIZE,
    });
    let dynamic_size = dynamic_count as u64 * 16;
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R,
        p_offset: dynsym_off,
        p_vaddr: dynsym_off,
        p_paddr: dynsym_off,
        p_filesz: dyn_segment_end - dynsym_off,
        p_memsz: dyn_segment_end - dynsym_off,
        p_align: PAGE_SIZE,
    });
    w.write_program_header(&ProgramHeader {
        p_type: elf::PT_DYNAMIC,
        p_flags: elf::PF_R,
        p_offset: dynamic_off,
        p_vaddr: dynamic_off,
        p_paddr: dynamic_off,
        p_filesz: dynamic_size,
        p_memsz: dynamic_size,
        p_align: 8,
    });
    if layout.tls_memsz > 0 {
        w.write_program_header(&ProgramHeader {
            p_type: elf::PT_TLS,
            p_flags: elf::PF_R,
            p_offset: layout.tls_start,
            p_vaddr: layout.tls_start,
            p_paddr: layout.tls_start,
            p_filesz: layout.tls_filesz,
            p_memsz: layout.tls_memsz,
            p_align: 64,
        });
    }
    if has_eh_frame_hdr {
        w.write_program_header(&ProgramHeader {
            p_type: 0x6474_e550, // PT_GNU_EH_FRAME
            p_flags: elf::PF_R,
            p_offset: layout.eh_frame_hdr_vaddr,
            p_vaddr: layout.eh_frame_hdr_vaddr,
            p_paddr: layout.eh_frame_hdr_vaddr,
            p_filesz: layout.eh_frame_hdr_size,
            p_memsz: layout.eh_frame_hdr_size,
            p_align: 4,
        });
    }
    if has_build_id {
        w.write_program_header(&ProgramHeader {
            p_type: elf::PT_NOTE,
            p_flags: elf::PF_R,
            p_offset: layout.build_id_note_vaddr,
            p_vaddr: layout.build_id_note_vaddr,
            p_paddr: layout.build_id_note_vaddr,
            p_filesz: BUILD_ID_NOTE_SIZE,
            p_memsz: BUILD_ID_NOTE_SIZE,
            p_align: 4,
        });
    }

    // Section data
    write_sections_data(&mut w, &state.sections, BASE_VADDR);
    if has_eh_frame_hdr {
        w.pad_until(layout.eh_frame_hdr_vaddr as usize);
        w.write(eh_frame_hdr);
    }
    if has_build_id {
        w.pad_until(layout.build_id_note_vaddr as usize);
        w.write(&build_id_note_placeholder());
    }
    w.pad_until(file_rw_end as usize);

    // Dynamic symbols (sorted by GNU hash bucket)
    w.write_null_dynamic_symbol();
    for (_, str_id, st_value, _) in &sym_str_ids {
        w.write_dynamic_symbol(&Sym {
            name: Some(*str_id),
            section: None,
            st_info: (elf::STB_GLOBAL << 4) | elf::STT_NOTYPE,
            st_other: elf::STV_DEFAULT,
            st_shndx: 1, // defined (non-zero)
            st_value: *st_value,
            st_size: 0,
        });
    }
    w.write_dynstr();

    // .gnu.hash
    let sym_hashes: Vec<u32> = sym_str_ids.iter().map(|(_, _, _, h)| *h).collect();
    w.write_gnu_hash(1, 6, gnu_hash_bloom_count, gnu_hash_bucket_count, gnu_hash_sym_count, |i| sym_hashes[i as usize]);

    // Dynamic section
    w.write_align_dynamic();
    w.write_dynamic(elf::DT_SYMTAB as u32, dynsym_off);
    w.write_dynamic(elf::DT_STRTAB as u32, dynstr_off);
    let strsz = w.dynstr_len() as u64;
    w.write_dynamic(elf::DT_STRSZ as u32, strsz);
    w.write_dynamic(elf::DT_SYMENT as u32, 24);
    w.write_dynamic(elf::DT_GNU_HASH as u32, gnu_hash_off);
    if layout.init_array_size > 0 {
        w.write_dynamic(elf::DT_INIT_ARRAY as u32, layout.init_array_vaddr);
        w.write_dynamic(elf::DT_INIT_ARRAYSZ as u32, layout.init_array_size);
    }
    if layout.fini_array_size > 0 {
        w.write_dynamic(elf::DT_FINI_ARRAY as u32, layout.fini_array_vaddr);
        w.write_dynamic(elf::DT_FINI_ARRAYSZ as u32, layout.fini_array_size);
    }
    w.write_dynamic(elf::DT_NULL as u32, 0);
    w.pad_until(dyn_segment_end as usize);

    // Relocations
    w.write_align_relocation();
    for &(offset, addend) in &relocs.relatives {
        w.write_relocation(true, &Rel {
            r_offset: offset,
            r_sym: 0,
            r_type: elf::R_X86_64_RELATIVE,
            r_addend: addend,
        });
    }

    // Metadata sections
    for (i, (_, data)) in state.metadata.iter().enumerate() {
        w.pad_until(meta_offsets[i] as usize);
        w.write(data);
    }

    // Symtab + strtab
    write_symtab(&mut w, &sym_entries);

    // shstrtab + section headers
    w.write_shstrtab();

    w.write_null_section_header();
    w.write_section_header(&SectionHeader {
        name: Some(text_name),
        sh_type: elf::SHT_PROGBITS,
        sh_flags: (elf::SHF_ALLOC | elf::SHF_EXECINSTR) as u64,
        sh_addr: layout.rx_start,
        sh_offset: layout.rx_start - BASE_VADDR,
        sh_size: layout.rx_end - layout.rx_start,
        sh_link: 0, sh_info: 0, sh_addralign: 1, sh_entsize: 0,
    });
    w.write_section_header(&SectionHeader {
        name: Some(data_name),
        sh_type: elf::SHT_PROGBITS,
        sh_flags: (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
        sh_addr: layout.rw_start,
        sh_offset: layout.rw_start - BASE_VADDR,
        sh_size: layout.rw_filesz,
        sh_link: 0, sh_info: 0, sh_addralign: 1, sh_entsize: 0,
    });
    w.write_section_header(&SectionHeader {
        name: Some(rela_name),
        sh_type: elf::SHT_RELA,
        sh_flags: elf::SHF_ALLOC as u64,
        sh_addr: 0,
        sh_offset: rela_off,
        sh_size: rela_size,
        sh_link: 0, sh_info: 0, sh_addralign: 8, sh_entsize: 24,
    });
    w.write_dynsym_section_header(dynsym_off, 1);
    w.write_dynstr_section_header(dynstr_off);
    w.write_dynamic_section_header(dynamic_off);
    w.write_gnu_hash_section_header(gnu_hash_off);
    if has_eh_frame_hdr {
        w.write_section_header(&SectionHeader {
            name: eh_frame_hdr_name,
            sh_type: elf::SHT_PROGBITS,
            sh_flags: elf::SHF_ALLOC as u64,
            sh_addr: layout.eh_frame_hdr_vaddr,
            sh_offset: layout.eh_frame_hdr_vaddr,
            sh_size: layout.eh_frame_hdr_size,
            sh_link: 0, sh_info: 0, sh_addralign: 4, sh_entsize: 0,
        });
    }
    if has_build_id {
        w.write_section_header(&SectionHeader {
            name: build_id_name,
            sh_type: elf::SHT_NOTE,
            sh_flags: elf::SHF_ALLOC as u64,
            sh_addr: layout.build_id_note_vaddr,
            sh_offset: layout.build_id_note_vaddr,
            sh_size: BUILD_ID_NOTE_SIZE,
            sh_link: 0, sh_info: 0, sh_addralign: 4, sh_entsize: 0,
        });
    }
    for (i, (_, data)) in state.metadata.iter().enumerate() {
        w.write_section_header(&SectionHeader {
            name: Some(meta_names[i]),
            sh_type: elf::SHT_PROGBITS,
            sh_flags: 0,
            sh_addr: 0,
            sh_offset: meta_offsets[i],
            sh_size: data.len() as u64,
            sh_link: 0, sh_info: 0, sh_addralign: 1, sh_entsize: 0,
        });
    }
    w.write_symtab_section_header(num_local);
    w.write_strtab_section_header();
    w.write_shstrtab_section_header();

    if has_build_id {
        patch_build_id(&mut buf, layout.build_id_note_vaddr as usize);
    }

    buf
}

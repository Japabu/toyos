use crate::collect::{collect_unique_symbols, LinkState, SectionIdx, SymbolDef};
use crate::reloc::resolve_symbol;
use crate::{align_up, classify_sections, LinkError};
use object::elf;
use sha2::{Sha256, Digest};
use std::collections::HashMap;

// ── Mach-O constants ────────────────────────────────────────────────────

const MH_MAGIC_64: u32 = 0xFEEDFACF;
const CPU_TYPE_ARM64: u32 = 0x0100000C;
const CPU_SUBTYPE_ARM64_ALL: u32 = 0;
const MH_EXECUTE: u32 = 2;
const MH_PIE: u32 = 0x0020_0000;
const MH_TWOLEVEL: u32 = 0x80;
const MH_DYLDLINK: u32 = 0x4;

const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x02;
const LC_DYSYMTAB: u32 = 0x0B;
const LC_LOAD_DYLIB: u32 = 0x0C;
const LC_LOAD_DYLINKER: u32 = 0x0E;
const LC_MAIN: u32 = 0x80000028;
const LC_DYLD_INFO_ONLY: u32 = 0x80000022;
const LC_BUILD_VERSION: u32 = 0x32;
const LC_CODE_SIGNATURE: u32 = 0x1D;

const REBASE_OPCODE_SET_TYPE_IMM: u8 = 0x10;
const REBASE_TYPE_POINTER: u8 = 1;
const REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB: u8 = 0x20;
const REBASE_OPCODE_DO_REBASE_IMM_TIMES: u8 = 0x50;
const REBASE_OPCODE_DONE: u8 = 0x00;

const BIND_OPCODE_SET_DYLIB_ORDINAL_IMM: u8 = 0x10;
const BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM: u8 = 0x40;
const BIND_OPCODE_SET_TYPE_IMM: u8 = 0x50;
const BIND_TYPE_POINTER: u8 = 1;
const BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB: u8 = 0x70;
const BIND_OPCODE_DO_BIND: u8 = 0x90;
const BIND_OPCODE_DONE: u8 = 0x00;

/// Section flags
const S_REGULAR: u32 = 0x0;
const S_NON_LAZY_SYMBOL_POINTERS: u32 = 0x6;
const S_ZEROFILL: u32 = 0x1;
const S_ATTR_PURE_INSTRUCTIONS: u32 = 0x8000_0000;
const S_ATTR_SOME_INSTRUCTIONS: u32 = 0x0000_0400;

const PAGE_SIZE: u64 = 0x4000; // 16KB for arm64
const PAGEZERO_SIZE: u64 = 0x1_0000_0000; // 4GB

// ── Layout ──────────────────────────────────────────────────────────────

pub(crate) struct MachOLayout {
    /// Start of __TEXT segment in VM (also load address)
    pub(crate) text_vmaddr: u64,
    pub(crate) text_vmsize: u64,
    pub(crate) text_filesize: u64,
    // __text section within __TEXT
    pub(crate) text_sec_offset: u64, // file offset
    pub(crate) text_sec_vmaddr: u64,
    pub(crate) text_sec_size: u64,
    // __const section within __TEXT (read-only data)
    pub(crate) const_sec_offset: u64,
    pub(crate) const_sec_vmaddr: u64,
    pub(crate) const_sec_size: u64,

    /// Start of __DATA segment
    pub(crate) data_vmaddr: u64,
    pub(crate) data_vmsize: u64,
    pub(crate) data_fileoff: u64,
    pub(crate) data_filesize: u64,
    // __data section within __DATA
    pub(crate) data_sec_offset: u64,
    pub(crate) data_sec_vmaddr: u64,
    pub(crate) data_sec_size: u64,
    // __got section within __DATA
    pub(crate) got_sec_offset: u64,
    pub(crate) got_sec_vmaddr: u64,
    pub(crate) got_sec_size: u64,
    // __bss section within __DATA (virtual)
    pub(crate) bss_sec_vmaddr: u64,
    pub(crate) bss_sec_size: u64,

    /// Start of __LINKEDIT segment
    pub(crate) linkedit_vmaddr: u64,
    pub(crate) linkedit_fileoff: u64,

    /// GOT slot mapping: symbol name → virtual address of GOT entry
    pub(crate) got: HashMap<String, u64>,
    /// Ordered GOT symbols: (name, is_external)
    pub(crate) got_entries: Vec<(String, bool)>,

    /// Size of load commands (needed for header)
    pub(crate) sizeofcmds: u32,
    pub(crate) ncmds: u32,
}

/// Compute the Mach-O layout: segment placement, GOT allocation.
pub(crate) fn layout_macho(state: &mut LinkState, _entry: &str) -> MachOLayout {
    let buckets = classify_sections(state);

    // Phase 1: Figure out what we need to compute sizeofcmds
    let got_symbols = collect_unique_symbols(state.relocs.iter(), |r| {
        matches!(r.r_type,
            elf::R_AARCH64_ADR_GOT_PAGE | elf::R_AARCH64_LD64_GOT_LO12_NC
            | elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX
            | elf::R_X86_64_REX_GOTPCRELX)
    });

    let has_const = buckets.rx.iter().any(|&i| state.sections[i.0].writable == false && state.sections[i.0].name != ".text");
    let has_data = !buckets.rw.is_empty();
    let has_bss = buckets.rw.iter().any(|&i| state.sections[i.0].nobits);
    let has_got = !got_symbols.is_empty();

    // Count sections per segment
    let text_nsects = 1 + if has_const { 1 } else { 0 }; // __text [+ __const]
    let data_nsects = (if has_data { 1 } else { 0 })
        + (if has_got { 1 } else { 0 })
        + (if has_bss { 1 } else { 0 });
    let data_nsects = data_nsects.max(if has_got { 1 } else { 0 }); // ensure at least __got if needed

    // Compute load command sizes (preliminary — recalculated below with actual section counts)
    let dylib_name = "/usr/lib/libSystem.B.dylib\0";
    let dylib_cmd_size = align_up((24 + dylib_name.len()) as u64, 8) as u32;
    let dylinker_name = "/usr/lib/dyld\0";
    let dylinker_cmd_size = align_up((12 + dylinker_name.len()) as u64, 8) as u32;
    let preliminary_sizeofcmds: u32 = 72   // __PAGEZERO
        + 72 + 80 * text_nsects as u32     // __TEXT
        + 72 + 80 * data_nsects.max(1) as u32 // __DATA
        + 72                                // __LINKEDIT
        + dylinker_cmd_size                 // LC_LOAD_DYLINKER
        + 24                                // LC_MAIN
        + dylib_cmd_size                    // LC_LOAD_DYLIB
        + 48                                // LC_DYLD_INFO_ONLY
        + 24                                // LC_SYMTAB
        + 80                                // LC_DYSYMTAB
        + 32                                // LC_BUILD_VERSION
        + 16;                               // LC_CODE_SIGNATURE
    let header_size = 32 + preliminary_sizeofcmds as u64;

    // Phase 2: Layout sections with actual addresses
    let text_vmaddr = PAGEZERO_SIZE;

    // __text section: immediately after headers, aligned
    let mut cursor = text_vmaddr + align_up(header_size, 16);
    let text_sec_vmaddr = cursor;
    let text_sec_offset = cursor - text_vmaddr; // file offset within __TEXT
    // Place all code sections (RX except .const-like) into __text
    for &idx in &buckets.rx {
        let sec = &mut state.sections[idx.0];
        if sec.name.starts_with(".const") || sec.name.starts_with(".rodata") {
            continue; // These go in __const
        }
        cursor = align_up(cursor, sec.align);
        sec.vaddr = Some(cursor);
        cursor += sec.size;
    }
    let text_sec_size = cursor - text_sec_vmaddr;

    // __const section
    let const_sec_vmaddr = align_up(cursor, 8);
    let const_sec_offset = const_sec_vmaddr - text_vmaddr;
    let mut actual_has_const = false;
    cursor = const_sec_vmaddr;
    for &idx in &buckets.rx {
        let sec = &mut state.sections[idx.0];
        if sec.name.starts_with(".const") || sec.name.starts_with(".rodata") {
            cursor = align_up(cursor, sec.align);
            sec.vaddr = Some(cursor);
            cursor += sec.size;
            actual_has_const = true;
        }
    }
    let const_sec_size = cursor - const_sec_vmaddr;

    let text_vmsize = align_up(cursor - text_vmaddr, PAGE_SIZE);
    let text_filesize = text_vmsize;

    // __DATA segment
    let data_vmaddr = text_vmaddr + text_vmsize;
    let data_fileoff = text_filesize;
    let mut data_cursor = data_vmaddr;

    // __data section
    let data_sec_vmaddr = data_cursor;
    let data_sec_offset = data_fileoff;
    for &idx in &buckets.rw {
        let sec = &mut state.sections[idx.0];
        if sec.nobits { continue; }
        data_cursor = align_up(data_cursor, sec.align);
        sec.vaddr = Some(data_cursor);
        data_cursor += sec.size;
    }
    let data_sec_size = data_cursor - data_sec_vmaddr;

    // __got section
    data_cursor = align_up(data_cursor, 8);
    let got_sec_vmaddr = data_cursor;
    let got_sec_offset = data_fileoff + (got_sec_vmaddr - data_vmaddr);

    // Determine which GOT symbols are external (undefined)
    let mut got_entries = Vec::new();
    let mut got = HashMap::new();
    for sym in &got_symbols {
        let is_external = match state.globals.get(sym) {
            Some(SymbolDef::Dynamic) => true,
            Some(SymbolDef::Defined { .. }) => false,
            None => !state.locals.keys().any(|(_, n)| n == sym),
        };
        got.insert(sym.clone(), data_cursor);
        got_entries.push((sym.clone(), is_external));
        data_cursor += 8;
    }
    let got_sec_size = data_cursor - got_sec_vmaddr;

    // __bss section
    let bss_sec_vmaddr = align_up(data_cursor, 8);
    let bss_start = bss_sec_vmaddr;
    let mut bss_cursor = bss_sec_vmaddr;
    for &idx in &buckets.rw {
        let sec = &mut state.sections[idx.0];
        if !sec.nobits { continue; }
        bss_cursor = align_up(bss_cursor, sec.align);
        sec.vaddr = Some(bss_cursor);
        bss_cursor += sec.size;
    }
    let bss_sec_size = bss_cursor - bss_start;

    // Data segment: file size is everything before BSS
    let data_filesz = if got_sec_size > 0 {
        got_sec_vmaddr + got_sec_size - data_vmaddr
    } else {
        data_sec_size
    };
    let data_filesize = data_filesz;
    let data_memsz = bss_cursor - data_vmaddr;
    let data_vmsize = align_up(data_memsz, PAGE_SIZE);

    // __LINKEDIT segment
    let linkedit_vmaddr = data_vmaddr + data_vmsize;
    let linkedit_fileoff = data_fileoff + align_up(data_filesize, PAGE_SIZE);

    // Recount data sections
    let actual_data_nsects = (if data_sec_size > 0 { 1 } else { 0 })
        + (if got_sec_size > 0 { 1 } else { 0 })
        + (if bss_sec_size > 0 { 1 } else { 0 });

    // Recalculate sizeofcmds with actual section counts
    let actual_text_nsects = 1 + if actual_has_const && const_sec_size > 0 { 1 } else { 0 };
    let mut ncmds = 0u32;
    let mut sizeofcmds = 0u32;
    ncmds += 1; sizeofcmds += 72; // __PAGEZERO
    ncmds += 1; sizeofcmds += 72 + 80 * actual_text_nsects as u32; // __TEXT
    if actual_data_nsects > 0 {
        ncmds += 1; sizeofcmds += 72 + 80 * actual_data_nsects as u32; // __DATA
    }
    ncmds += 1; sizeofcmds += 72; // __LINKEDIT
    ncmds += 1; sizeofcmds += dylinker_cmd_size; // LC_LOAD_DYLINKER
    ncmds += 1; sizeofcmds += 24; // LC_MAIN
    ncmds += 1; sizeofcmds += dylib_cmd_size; // LC_LOAD_DYLIB
    ncmds += 1; sizeofcmds += 48; // LC_DYLD_INFO_ONLY
    ncmds += 1; sizeofcmds += 24; // LC_SYMTAB
    ncmds += 1; sizeofcmds += 80; // LC_DYSYMTAB
    ncmds += 1; sizeofcmds += 32; // LC_BUILD_VERSION
    ncmds += 1; sizeofcmds += 16; // LC_CODE_SIGNATURE

    // Check that sections don't overlap with headers
    let final_header_size = 32 + sizeofcmds as u64;
    assert!(
        text_sec_offset >= align_up(final_header_size, 16) || text_sec_size == 0,
        "sections overlap with headers: header={final_header_size} text_offset={}",
        text_sec_offset
    );

    MachOLayout {
        text_vmaddr,
        text_vmsize,
        text_filesize,
        text_sec_offset,
        text_sec_vmaddr,
        text_sec_size,
        const_sec_offset,
        const_sec_vmaddr,
        const_sec_size,
        data_vmaddr,
        data_vmsize,
        data_fileoff,
        data_filesize,
        data_sec_offset,
        data_sec_vmaddr,
        data_sec_size,
        got_sec_offset,
        got_sec_vmaddr,
        got_sec_size,
        bss_sec_vmaddr,
        bss_sec_size,
        linkedit_vmaddr,
        linkedit_fileoff,
        got,
        got_entries,
        sizeofcmds,
        ncmds,
    }
}

// ── Emission ────────────────────────────────────────────────────────────

pub(crate) fn emit_macho_bytes(
    state: &LinkState,
    layout: &MachOLayout,
    entry_name: &str,
    rebase_entries: &[(u64, i64)],  // (vaddr, value) for internal pointers
    bind_entries: &[(String, u64)], // (symbol_name, got_slot_vaddr) for external
) -> Result<Vec<u8>, LinkError> {
    let entry_addr = state
        .globals
        .get(entry_name)
        .map(|def| match def {
            SymbolDef::Defined { section, value } => {
                state.sections[section.0].vaddr.unwrap() + value
            }
            SymbolDef::Dynamic => panic!("entry point cannot be a dynamic symbol"),
        })
        .ok_or_else(|| LinkError::MissingEntry(entry_name.to_string()))?;

    let entryoff = entry_addr - layout.text_vmaddr;

    // Build __LINKEDIT content first to know sizes
    let rebase_data = build_rebase_opcodes(layout, rebase_entries);
    let bind_data = build_bind_opcodes(layout, bind_entries);
    let export_data = build_export_trie(entry_name, entryoff);

    // Build symbol table
    let (symtab_data, strtab_data, nlocalsym, nextdefsym, nundefsym) =
        build_symbol_table(state, layout, bind_entries);

    // __LINKEDIT layout
    let mut linkedit_cursor = layout.linkedit_fileoff;

    let rebase_off = linkedit_cursor;
    let rebase_size = rebase_data.len() as u32;
    linkedit_cursor += align_up(rebase_size as u64, 8);

    let bind_off = linkedit_cursor;
    let bind_size = bind_data.len() as u32;
    linkedit_cursor += align_up(bind_size as u64, 8);

    let export_off = linkedit_cursor;
    let export_size = export_data.len() as u32;
    linkedit_cursor += align_up(export_size as u64, 8);

    let symtab_off = linkedit_cursor;
    let nsyms = (symtab_data.len() / 16) as u32;
    linkedit_cursor += symtab_data.len() as u64;
    linkedit_cursor = align_up(linkedit_cursor, 8);

    let strtab_off = linkedit_cursor;
    let strtab_size = strtab_data.len() as u32;
    linkedit_cursor += align_up(strtab_size as u64, 8);

    // Code signature: must be 16-byte aligned
    linkedit_cursor = align_up(linkedit_cursor, 16);
    let codesig_off = linkedit_cursor;
    // Estimate signature size: SuperBlob(12) + 1 BlobIndex(8) + CodeDirectory(88)
    //   + identifier + nCodeSlots*32
    let code_limit = codesig_off as u32;
    let cs_page_size: u32 = 4096;
    let n_code_slots = (code_limit + cs_page_size - 1) / cs_page_size;
    let ident = "_main\0"; // identifier string (null-terminated)
    let cd_size = 88 + ident.len() as u32 + n_code_slots * 32;
    let codesig_size = 12 + 8 + cd_size; // SuperBlob + 1 BlobIndex + CodeDirectory
    let codesig_size_aligned = align_up(codesig_size as u64, 16) as u32;
    linkedit_cursor += codesig_size_aligned as u64;

    let linkedit_filesize = linkedit_cursor - layout.linkedit_fileoff;
    let linkedit_vmsize = align_up(linkedit_filesize, PAGE_SIZE);

    // Build the output buffer
    let total_size = linkedit_cursor as usize;
    let mut buf = vec![0u8; total_size];

    // ── Mach-O header ──
    let flags = MH_PIE | MH_TWOLEVEL | MH_DYLDLINK;
    write32(&mut buf, 0, MH_MAGIC_64);
    write32(&mut buf, 4, CPU_TYPE_ARM64);
    write32(&mut buf, 8, CPU_SUBTYPE_ARM64_ALL);
    write32(&mut buf, 12, MH_EXECUTE);
    write32(&mut buf, 16, layout.ncmds);
    write32(&mut buf, 20, layout.sizeofcmds);
    write32(&mut buf, 24, flags);
    write32(&mut buf, 28, 0); // reserved

    // ── Load commands ──
    let mut cmd_off = 32usize;

    // LC_SEGMENT_64 __PAGEZERO
    cmd_off = write_segment_cmd(&mut buf, cmd_off, b"__PAGEZERO\0\0\0\0\0\0",
        0, PAGEZERO_SIZE, 0, 0, 0, 0, 0, 0);

    // LC_SEGMENT_64 __TEXT
    let text_nsects = 1 + if layout.const_sec_size > 0 { 1u32 } else { 0 };
    let text_cmd_size = 72 + 80 * text_nsects;
    write32(&mut buf, cmd_off, LC_SEGMENT_64);
    write32(&mut buf, cmd_off + 4, text_cmd_size);
    write_segname(&mut buf, cmd_off + 8, b"__TEXT");
    write64(&mut buf, cmd_off + 24, layout.text_vmaddr);
    write64(&mut buf, cmd_off + 32, layout.text_vmsize);
    write64(&mut buf, cmd_off + 40, 0); // fileoff
    write64(&mut buf, cmd_off + 48, layout.text_filesize);
    write32(&mut buf, cmd_off + 56, 5); // maxprot: r-x
    write32(&mut buf, cmd_off + 60, 5); // initprot: r-x
    write32(&mut buf, cmd_off + 64, text_nsects);
    write32(&mut buf, cmd_off + 68, 0); // flags
    let mut sec_off = cmd_off + 72;

    // __text section header
    sec_off = write_section_header(&mut buf, sec_off, b"__text", b"__TEXT",
        layout.text_sec_vmaddr, layout.text_sec_size,
        layout.text_sec_offset as u32, 2, // align 2^2 = 4
        0, 0,
        S_REGULAR | S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS, 0, 0, 0);

    // __const section header (if present)
    if layout.const_sec_size > 0 {
        sec_off = write_section_header(&mut buf, sec_off, b"__const", b"__TEXT",
            layout.const_sec_vmaddr, layout.const_sec_size,
            layout.const_sec_offset as u32, 0,
            0, 0,
            S_REGULAR, 0, 0, 0);
    }
    cmd_off = sec_off;

    // LC_SEGMENT_64 __DATA
    let data_nsects = (if layout.data_sec_size > 0 { 1u32 } else { 0 })
        + (if layout.got_sec_size > 0 { 1 } else { 0 })
        + (if layout.bss_sec_size > 0 { 1 } else { 0 });
    if data_nsects > 0 {
        let data_cmd_size = 72 + 80 * data_nsects;
        write32(&mut buf, cmd_off, LC_SEGMENT_64);
        write32(&mut buf, cmd_off + 4, data_cmd_size);
        write_segname(&mut buf, cmd_off + 8, b"__DATA");
        write64(&mut buf, cmd_off + 24, layout.data_vmaddr);
        write64(&mut buf, cmd_off + 32, layout.data_vmsize);
        write64(&mut buf, cmd_off + 40, layout.data_fileoff);
        write64(&mut buf, cmd_off + 48, layout.data_filesize);
        write32(&mut buf, cmd_off + 56, 3); // maxprot: rw-
        write32(&mut buf, cmd_off + 60, 3); // initprot: rw-
        write32(&mut buf, cmd_off + 64, data_nsects);
        write32(&mut buf, cmd_off + 68, 0); // flags
        let mut sec_off = cmd_off + 72;

        if layout.data_sec_size > 0 {
            sec_off = write_section_header(&mut buf, sec_off, b"__data", b"__DATA",
                layout.data_sec_vmaddr, layout.data_sec_size,
                layout.data_sec_offset as u32, 3, // align 2^3 = 8
                0, 0,
                S_REGULAR, 0, 0, 0);
        }
        if layout.got_sec_size > 0 {
            sec_off = write_section_header(&mut buf, sec_off, b"__got", b"__DATA",
                layout.got_sec_vmaddr, layout.got_sec_size,
                layout.got_sec_offset as u32, 3,
                0, 0,
                S_NON_LAZY_SYMBOL_POINTERS, 0, 0, 0);
        }
        if layout.bss_sec_size > 0 {
            sec_off = write_section_header(&mut buf, sec_off, b"__bss", b"__DATA",
                layout.bss_sec_vmaddr, layout.bss_sec_size,
                0, 3,
                0, 0,
                S_ZEROFILL, 0, 0, 0);
        }
        cmd_off = sec_off;
    }

    // LC_SEGMENT_64 __LINKEDIT
    cmd_off = write_segment_cmd(&mut buf, cmd_off, b"__LINKEDIT\0\0\0\0\0\0",
        layout.linkedit_vmaddr, linkedit_vmsize,
        layout.linkedit_fileoff, linkedit_filesize,
        7, 1, // maxprot=rwx, initprot=r--
        0, 0);

    // LC_LOAD_DYLINKER
    let dylinker_name_bytes = b"/usr/lib/dyld\0";
    let dylinker_cmd_size = align_up((12 + dylinker_name_bytes.len()) as u64, 8) as u32;
    write32(&mut buf, cmd_off, LC_LOAD_DYLINKER);
    write32(&mut buf, cmd_off + 4, dylinker_cmd_size);
    write32(&mut buf, cmd_off + 8, 12); // name offset
    buf[cmd_off + 12..cmd_off + 12 + dylinker_name_bytes.len()].copy_from_slice(dylinker_name_bytes);
    cmd_off += dylinker_cmd_size as usize;

    // LC_MAIN
    write32(&mut buf, cmd_off, LC_MAIN);
    write32(&mut buf, cmd_off + 4, 24);
    write64(&mut buf, cmd_off + 8, entryoff);
    write64(&mut buf, cmd_off + 16, 0); // stacksize
    cmd_off += 24;

    // LC_LOAD_DYLIB
    let dylib_name_bytes = b"/usr/lib/libSystem.B.dylib\0";
    let dylib_cmd_size = align_up((24 + dylib_name_bytes.len()) as u64, 8) as u32;
    write32(&mut buf, cmd_off, LC_LOAD_DYLIB);
    write32(&mut buf, cmd_off + 4, dylib_cmd_size);
    write32(&mut buf, cmd_off + 8, 24); // name offset
    write32(&mut buf, cmd_off + 12, 0); // timestamp
    write32(&mut buf, cmd_off + 16, 0x010000); // current version
    write32(&mut buf, cmd_off + 20, 0x010000); // compat version
    buf[cmd_off + 24..cmd_off + 24 + dylib_name_bytes.len()].copy_from_slice(dylib_name_bytes);
    cmd_off += dylib_cmd_size as usize;

    // LC_DYLD_INFO_ONLY
    write32(&mut buf, cmd_off, LC_DYLD_INFO_ONLY);
    write32(&mut buf, cmd_off + 4, 48);
    write32(&mut buf, cmd_off + 8, rebase_off as u32);  // rebase_off
    write32(&mut buf, cmd_off + 12, rebase_size);        // rebase_size
    write32(&mut buf, cmd_off + 16, bind_off as u32);   // bind_off
    write32(&mut buf, cmd_off + 20, bind_size);          // bind_size
    write32(&mut buf, cmd_off + 24, 0);                  // weak_bind_off
    write32(&mut buf, cmd_off + 28, 0);                  // weak_bind_size
    write32(&mut buf, cmd_off + 32, 0);                  // lazy_bind_off
    write32(&mut buf, cmd_off + 36, 0);                  // lazy_bind_size
    write32(&mut buf, cmd_off + 40, export_off as u32);  // export_off
    write32(&mut buf, cmd_off + 44, export_size);        // export_size
    cmd_off += 48;

    // LC_SYMTAB
    write32(&mut buf, cmd_off, LC_SYMTAB);
    write32(&mut buf, cmd_off + 4, 24);
    write32(&mut buf, cmd_off + 8, symtab_off as u32);
    write32(&mut buf, cmd_off + 12, nsyms);
    write32(&mut buf, cmd_off + 16, strtab_off as u32);
    write32(&mut buf, cmd_off + 20, strtab_size);
    cmd_off += 24;

    // LC_DYSYMTAB
    write32(&mut buf, cmd_off, LC_DYSYMTAB);
    write32(&mut buf, cmd_off + 4, 80);
    write32(&mut buf, cmd_off + 8, 0);           // ilocalsym
    write32(&mut buf, cmd_off + 12, nlocalsym);  // nlocalsym
    write32(&mut buf, cmd_off + 16, nlocalsym);  // iextdefsym
    write32(&mut buf, cmd_off + 20, nextdefsym); // nextdefsym
    write32(&mut buf, cmd_off + 24, nlocalsym + nextdefsym); // iundefsym
    write32(&mut buf, cmd_off + 28, nundefsym);  // nundefsym
    // All remaining fields are 0 (indirect symbol table etc.)
    cmd_off += 80;

    // LC_BUILD_VERSION (platform = macOS, minos = 14.0)
    write32(&mut buf, cmd_off, LC_BUILD_VERSION);
    write32(&mut buf, cmd_off + 4, 32);      // cmdsize
    write32(&mut buf, cmd_off + 8, 1);       // platform = MACOS
    write32(&mut buf, cmd_off + 12, 0x000E0000); // minos = 14.0.0
    write32(&mut buf, cmd_off + 16, 0x000E0000); // sdk = 14.0.0
    write32(&mut buf, cmd_off + 20, 1);      // ntools = 1
    write32(&mut buf, cmd_off + 24, 3);      // tool = LD
    write32(&mut buf, cmd_off + 28, 0x010000);   // version = 1.0
    cmd_off += 32;

    // LC_CODE_SIGNATURE
    write32(&mut buf, cmd_off, LC_CODE_SIGNATURE);
    write32(&mut buf, cmd_off + 4, 16);
    write32(&mut buf, cmd_off + 8, code_limit);
    write32(&mut buf, cmd_off + 12, codesig_size);
    cmd_off += 16;

    let _ = cmd_off;

    // ── Write section data ──

    // __TEXT,__text: code sections
    for sec in &state.sections {
        let Some(vaddr) = sec.vaddr else { continue; };
        if sec.data.is_empty() { continue; }
        if vaddr >= layout.text_sec_vmaddr
            && vaddr < layout.text_sec_vmaddr + layout.text_sec_size
        {
            let off = (vaddr - layout.text_vmaddr) as usize;
            buf[off..off + sec.data.len()].copy_from_slice(&sec.data);
        }
    }

    // __TEXT,__const: read-only data sections
    if layout.const_sec_size > 0 {
        for sec in &state.sections {
            let Some(vaddr) = sec.vaddr else { continue; };
            if sec.data.is_empty() { continue; }
            if vaddr >= layout.const_sec_vmaddr
                && vaddr < layout.const_sec_vmaddr + layout.const_sec_size
            {
                let off = (vaddr - layout.text_vmaddr) as usize;
                buf[off..off + sec.data.len()].copy_from_slice(&sec.data);
            }
        }
    }

    // __DATA,__data: writable data sections
    for sec in &state.sections {
        let Some(vaddr) = sec.vaddr else { continue; };
        if sec.data.is_empty() || sec.nobits { continue; }
        if vaddr >= layout.data_sec_vmaddr
            && vaddr < layout.data_sec_vmaddr + layout.data_sec_size
        {
            let off = (layout.data_fileoff + (vaddr - layout.data_vmaddr)) as usize;
            if off + sec.data.len() <= buf.len() {
                buf[off..off + sec.data.len()].copy_from_slice(&sec.data);
            }
        }
    }

    // __DATA,__got: fill internal GOT entries
    for (sym_name, &got_vaddr) in &layout.got {
        let is_external = layout.got_entries.iter()
            .any(|(n, ext)| n == sym_name && *ext);
        if is_external { continue; } // filled by dyld
        let sym_addr = resolve_symbol(state, sym_name, SectionIdx(0), None)
            .ok_or_else(|| LinkError::UndefinedSymbols(vec![sym_name.clone()]))?;
        let off = (layout.data_fileoff + (got_vaddr - layout.data_vmaddr)) as usize;
        buf[off..off + 8].copy_from_slice(&sym_addr.to_le_bytes());
    }

    // __LINKEDIT content
    write_at(&mut buf, rebase_off as usize, &rebase_data);
    write_at(&mut buf, bind_off as usize, &bind_data);
    write_at(&mut buf, export_off as usize, &export_data);
    write_at(&mut buf, symtab_off as usize, &symtab_data);
    write_at(&mut buf, strtab_off as usize, &strtab_data);

    // Build and write ad-hoc code signature
    let codesig = build_code_signature(&buf, code_limit, n_code_slots, layout);
    write_at(&mut buf, codesig_off as usize, &codesig);

    Ok(buf)
}

// ── Rebase opcodes ──────────────────────────────────────────────────────

fn build_rebase_opcodes(layout: &MachOLayout, entries: &[(u64, i64)]) -> Vec<u8> {
    let mut ops = Vec::new();
    if entries.is_empty() {
        ops.push(REBASE_OPCODE_DONE);
        return ops;
    }

    ops.push(REBASE_OPCODE_SET_TYPE_IMM | REBASE_TYPE_POINTER);

    // Group rebase entries by segment
    let mut data_entries: Vec<u64> = entries.iter()
        .filter(|(vaddr, _)| *vaddr >= layout.data_vmaddr && *vaddr < layout.data_vmaddr + layout.data_vmsize)
        .map(|(vaddr, _)| *vaddr)
        .collect();
    data_entries.sort();

    if !data_entries.is_empty() {
        // __DATA segment ordinal: __PAGEZERO=0, __TEXT=1, __DATA=2
        let seg_ordinal = 2u8;
        for &vaddr in &data_entries {
            let offset = vaddr - layout.data_vmaddr;
            ops.push(REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB | seg_ordinal);
            encode_uleb128(&mut ops, offset);
            ops.push(REBASE_OPCODE_DO_REBASE_IMM_TIMES | 1);
        }
    }

    // Also rebase entries in __TEXT (if any absolute data pointers in const)
    let mut text_entries: Vec<u64> = entries.iter()
        .filter(|(vaddr, _)| *vaddr >= layout.text_vmaddr && *vaddr < layout.text_vmaddr + layout.text_vmsize)
        .map(|(vaddr, _)| *vaddr)
        .collect();
    text_entries.sort();

    if !text_entries.is_empty() {
        let seg_ordinal = 1u8; // __TEXT
        for &vaddr in &text_entries {
            let offset = vaddr - layout.text_vmaddr;
            ops.push(REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB | seg_ordinal);
            encode_uleb128(&mut ops, offset);
            ops.push(REBASE_OPCODE_DO_REBASE_IMM_TIMES | 1);
        }
    }

    ops.push(REBASE_OPCODE_DONE);
    ops
}

// ── Bind opcodes ────────────────────────────────────────────────────────

fn build_bind_opcodes(layout: &MachOLayout, entries: &[(String, u64)]) -> Vec<u8> {
    let mut ops = Vec::new();
    if entries.is_empty() {
        ops.push(BIND_OPCODE_DONE);
        return ops;
    }

    for (sym_name, got_vaddr) in entries {
        // Set dylib ordinal (1 = first LC_LOAD_DYLIB = libSystem)
        ops.push(BIND_OPCODE_SET_DYLIB_ORDINAL_IMM | 1);
        // Set symbol name
        ops.push(BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM | 0);
        ops.extend_from_slice(sym_name.as_bytes());
        ops.push(0); // null terminator
        // Set type
        ops.push(BIND_OPCODE_SET_TYPE_IMM | BIND_TYPE_POINTER);
        // Set segment and offset (__DATA segment = ordinal 2)
        let seg_ordinal = 2u8;
        let offset = got_vaddr - layout.data_vmaddr;
        ops.push(BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB | seg_ordinal);
        encode_uleb128(&mut ops, offset);
        // Bind
        ops.push(BIND_OPCODE_DO_BIND);
    }

    ops.push(BIND_OPCODE_DONE);
    ops
}

// ── Export trie ─────────────────────────────────────────────────────────

fn build_export_trie(entry_name: &str, entry_offset: u64) -> Vec<u8> {
    // Minimal export trie with just the entry symbol.
    // Trie format: each node has terminal_size + [info] + num_children + [edges]
    // Root node with one child edge for the entry symbol.
    let mut trie = Vec::new();

    // Root node
    trie.push(0); // terminal size = 0 (root has no export)
    trie.push(1); // 1 child

    // Edge: entry symbol name → child node
    trie.extend_from_slice(entry_name.as_bytes());
    trie.push(0); // null terminator

    // Child node offset (will be right after this ULEB)
    let child_offset = trie.len() + 1; // +1 for the ULEB we're about to write
    // We need to know the size, so let's compute it
    let mut info = Vec::new();
    info.push(0); // flags: EXPORT_SYMBOL_FLAGS_KIND_REGULAR
    encode_uleb128(&mut info, entry_offset);
    let terminal_size = info.len();

    // ULEB128 for child node offset
    let child_node_offset = trie.len() + uleb_size(child_offset as u64);
    encode_uleb128(&mut trie, child_node_offset as u64);

    // Child node (terminal)
    let mut child = Vec::new();
    encode_uleb128(&mut child, terminal_size as u64); // terminal size
    child.extend_from_slice(&info);
    child.push(0); // 0 children

    trie.extend_from_slice(&child);

    // Pad to 8-byte alignment
    while trie.len() % 8 != 0 { trie.push(0); }

    trie
}

// ── Symbol table ────────────────────────────────────────────────────────

fn build_symbol_table(
    state: &LinkState,
    layout: &MachOLayout,
    bind_entries: &[(String, u64)],
) -> (Vec<u8>, Vec<u8>, u32, u32, u32) {
    // nlist_64: { n_strx: u32, n_type: u8, n_sect: u8, n_desc: u16, n_value: u64 } = 16 bytes
    let mut symtab = Vec::new();
    let mut strtab = vec![0u8]; // index 0 = empty string

    // Section numbering (1-based): __text=1, __const=2 (if exists), __data=3, __got=4, __bss=5
    let mut sect_num = 1u8;
    let text_sect = sect_num; sect_num += 1;
    let const_sect = if layout.const_sec_size > 0 { let s = sect_num; sect_num += 1; s } else { 0 };
    let data_sect = if layout.data_sec_size > 0 { let s = sect_num; sect_num += 1; s } else { 0 };
    if layout.got_sec_size > 0 { sect_num += 1; }
    if layout.bss_sec_size > 0 { sect_num += 1; }
    let _ = sect_num;

    fn add_string(strtab: &mut Vec<u8>, s: &str) -> u32 {
        let offset = strtab.len() as u32;
        strtab.extend_from_slice(s.as_bytes());
        strtab.push(0);
        offset
    }

    // Local symbols (none for now)
    let nlocalsym = 0u32;

    // Defined external symbols
    let mut extdef_syms: Vec<(String, u64, u8)> = Vec::new(); // (name, value, section)
    for (name, def) in &state.globals {
        let SymbolDef::Defined { section, value } = def else { continue; };
        let sec = &state.sections[section.0];
        let value = sec.vaddr.unwrap() + value;
        let sect = if value >= layout.text_sec_vmaddr && value < layout.text_sec_vmaddr + layout.text_sec_size {
            text_sect
        } else if layout.const_sec_size > 0 && value >= layout.const_sec_vmaddr && value < layout.const_sec_vmaddr + layout.const_sec_size {
            const_sect
        } else if layout.data_sec_size > 0 && value >= layout.data_sec_vmaddr && value < layout.data_sec_vmaddr + layout.data_sec_size {
            data_sect
        } else {
            1 // fallback
        };
        extdef_syms.push((name.clone(), value, sect));
    }
    extdef_syms.sort_by(|a, b| a.0.cmp(&b.0));
    let nextdefsym = extdef_syms.len() as u32;

    for (name, value, sect) in &extdef_syms {
        let n_strx = add_string(&mut strtab, name);
        let n_type: u8 = 0x0F; // N_SECT | N_EXT
        let n_sect = *sect;
        let n_desc: u16 = 0;
        let n_value = *value;
        symtab.extend_from_slice(&n_strx.to_le_bytes());
        symtab.push(n_type);
        symtab.push(n_sect);
        symtab.extend_from_slice(&n_desc.to_le_bytes());
        symtab.extend_from_slice(&n_value.to_le_bytes());
    }

    // Undefined symbols (external dylib references)
    let mut undef_syms: Vec<String> = bind_entries.iter().map(|(n, _)| n.clone()).collect();
    undef_syms.sort();
    undef_syms.dedup();
    let nundefsym = undef_syms.len() as u32;

    for name in &undef_syms {
        let n_strx = add_string(&mut strtab, name);
        let n_type: u8 = 0x01; // N_EXT (undefined)
        let n_sect: u8 = 0;    // NO_SECT
        // REFERENCE_FLAG_UNDEFINED_NON_LAZY (0) | SET_LIBRARY_ORDINAL(1)
        let n_desc: u16 = 0x0100; // library ordinal 1 in high byte
        let n_value: u64 = 0;
        symtab.extend_from_slice(&n_strx.to_le_bytes());
        symtab.push(n_type);
        symtab.push(n_sect);
        symtab.extend_from_slice(&n_desc.to_le_bytes());
        symtab.extend_from_slice(&n_value.to_le_bytes());
    }

    (symtab, strtab, nlocalsym, nextdefsym, nundefsym)
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn encode_uleb128(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            break;
        }
        buf.push(byte | 0x80);
    }
}

fn uleb_size(mut value: u64) -> usize {
    let mut size = 0;
    loop {
        value >>= 7;
        size += 1;
        if value == 0 { break; }
    }
    size
}

fn write32(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

fn write64(buf: &mut [u8], off: usize, val: u64) {
    buf[off..off + 8].copy_from_slice(&val.to_le_bytes());
}

fn write_at(buf: &mut [u8], off: usize, data: &[u8]) {
    buf[off..off + data.len()].copy_from_slice(data);
}

fn write_segname(buf: &mut [u8], off: usize, name: &[u8]) {
    let mut segname = [0u8; 16];
    let len = name.len().min(16);
    segname[..len].copy_from_slice(&name[..len]);
    buf[off..off + 16].copy_from_slice(&segname);
}

fn write_segment_cmd(
    buf: &mut [u8], off: usize, name: &[u8],
    vmaddr: u64, vmsize: u64, fileoff: u64, filesize: u64,
    maxprot: u32, initprot: u32, nsects: u32, flags: u32,
) -> usize {
    write32(buf, off, LC_SEGMENT_64);
    write32(buf, off + 4, 72 + 80 * nsects);
    write_segname(buf, off + 8, name);
    write64(buf, off + 24, vmaddr);
    write64(buf, off + 32, vmsize);
    write64(buf, off + 40, fileoff);
    write64(buf, off + 48, filesize);
    write32(buf, off + 56, maxprot);
    write32(buf, off + 60, initprot);
    write32(buf, off + 64, nsects);
    write32(buf, off + 68, flags);
    off + 72 + 80 * nsects as usize
}

fn write_section_header(
    buf: &mut [u8], off: usize, sectname: &[u8], segname: &[u8],
    addr: u64, size: u64, offset: u32, align: u32,
    reloff: u32, nreloc: u32,
    flags: u32, reserved1: u32, reserved2: u32, reserved3: u32,
) -> usize {
    let mut name = [0u8; 16];
    let len = sectname.len().min(16);
    name[..len].copy_from_slice(&sectname[..len]);
    buf[off..off + 16].copy_from_slice(&name);
    write_segname(buf, off + 16, segname);
    write64(buf, off + 32, addr);
    write64(buf, off + 40, size);
    write32(buf, off + 48, offset);
    write32(buf, off + 52, align);
    write32(buf, off + 56, reloff);
    write32(buf, off + 60, nreloc);
    write32(buf, off + 64, flags);
    write32(buf, off + 68, reserved1);
    write32(buf, off + 72, reserved2);
    write32(buf, off + 76, reserved3);
    off + 80
}

// ── Ad-hoc code signature ────────────────────────────────────────────

/// Build a minimal ad-hoc code signature (SuperBlob with CodeDirectory).
fn build_code_signature(
    file_bytes: &[u8],
    code_limit: u32,
    n_code_slots: u32,
    layout: &MachOLayout,
) -> Vec<u8> {
    const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xFADE_0CC0;
    const CSMAGIC_CODEDIRECTORY: u32 = 0xFADE_0C02;
    const CS_SUPPORTSEXECSEG: u32 = 0x0002_0400;
    const CS_ADHOC: u32 = 0x0000_0002;
    const CS_LINKER_SIGNED: u32 = 0x0002_0000;
    const CS_HASHTYPE_SHA256: u8 = 2;
    const CS_SHA256_LEN: u8 = 32;
    const CS_EXECSEG_MAIN_BINARY: u64 = 1;
    const CS_PAGE_SIZE_LOG2: u8 = 12;
    const CS_PAGE_SIZE: u32 = 4096;

    let ident = b"_main\0";

    // CodeDirectory layout: 88 bytes header + identifier + page hashes
    let hash_offset = 88 + ident.len() as u32;
    let cd_length = hash_offset + n_code_slots * CS_SHA256_LEN as u32;

    // SuperBlob: header(12) + 1 BlobIndex(8) + CodeDirectory
    let blob_offset = 12 + 8; // offset from SuperBlob start to CodeDirectory
    let super_blob_length = blob_offset as u32 + cd_length;

    let mut sig = vec![0u8; super_blob_length as usize];

    // SuperBlob header (big-endian)
    write32_be(&mut sig, 0, CSMAGIC_EMBEDDED_SIGNATURE);
    write32_be(&mut sig, 4, super_blob_length);
    write32_be(&mut sig, 8, 1); // count = 1 blob

    // BlobIndex[0]: CodeDirectory
    write32_be(&mut sig, 12, 0); // CSSLOT_CODEDIRECTORY
    write32_be(&mut sig, 16, blob_offset as u32);

    // CodeDirectory (at offset blob_offset, big-endian)
    let cd = blob_offset as usize;
    write32_be(&mut sig, cd + 0, CSMAGIC_CODEDIRECTORY);
    write32_be(&mut sig, cd + 4, cd_length);
    write32_be(&mut sig, cd + 8, CS_SUPPORTSEXECSEG); // version
    write32_be(&mut sig, cd + 12, CS_ADHOC | CS_LINKER_SIGNED); // flags
    write32_be(&mut sig, cd + 16, hash_offset); // hashOffset
    write32_be(&mut sig, cd + 20, 88); // identOffset (always after fixed header)
    write32_be(&mut sig, cd + 24, 0); // nSpecialSlots
    write32_be(&mut sig, cd + 28, n_code_slots);
    write32_be(&mut sig, cd + 32, code_limit);
    sig[cd + 36] = CS_SHA256_LEN; // hashSize
    sig[cd + 37] = CS_HASHTYPE_SHA256; // hashType
    sig[cd + 38] = 0; // platform
    sig[cd + 39] = CS_PAGE_SIZE_LOG2; // pageSize
    write32_be(&mut sig, cd + 40, 0); // spare2
    // v0x20100: scatterOffset
    write32_be(&mut sig, cd + 44, 0);
    // v0x20200: teamOffset
    write32_be(&mut sig, cd + 48, 0);
    // v0x20300: spare3
    write32_be(&mut sig, cd + 52, 0);
    // v0x20300: codeLimit64
    write64_be(&mut sig, cd + 56, 0);
    // v0x20400: execSegBase, execSegLimit, execSegFlags
    write64_be(&mut sig, cd + 64, 0); // __TEXT fileoff = 0
    write64_be(&mut sig, cd + 72, layout.text_filesize);
    write64_be(&mut sig, cd + 80, CS_EXECSEG_MAIN_BINARY);

    // Identifier string
    sig[cd + 88..cd + 88 + ident.len()].copy_from_slice(ident);

    // Page hashes: SHA-256 of each 4KB page of the file up to code_limit
    let hash_start = cd + hash_offset as usize;
    for i in 0..n_code_slots {
        let page_start = (i * CS_PAGE_SIZE) as usize;
        let page_end = ((i + 1) * CS_PAGE_SIZE).min(code_limit) as usize;
        let hash = Sha256::digest(&file_bytes[page_start..page_end]);
        let off = hash_start + (i as usize) * 32;
        sig[off..off + 32].copy_from_slice(&hash);
    }

    sig
}

fn write32_be(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_be_bytes());
}

fn write64_be(buf: &mut [u8], off: usize, val: u64) {
    buf[off..off + 8].copy_from_slice(&val.to_be_bytes());
}

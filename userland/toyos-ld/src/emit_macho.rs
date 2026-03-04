use crate::collect::{collect_unique_symbols, LinkState, RelocType, SectionKind, SymbolDef, SymbolRef};
use crate::reloc::resolve_symbol;
use crate::{align_up, classify_sections, LinkError};
use sha2::{Sha256, Digest};
use std::collections::HashMap;
use std::mem::size_of;
use zerocopy::{FromZeros, Immutable, IntoBytes};
use zerocopy::little_endian::{U16 as U16Le, U32 as U32Le, U64 as U64Le};
use zerocopy::big_endian::{U32 as U32Be, U64 as U64Be};

// ── Mach-O constants ────────────────────────────────────────────────────

const MH_MAGIC_64: u32 = 0xFEEDFACF;
const CPU_TYPE_ARM64: u32 = 0x0100000C;
const CPU_SUBTYPE_ARM64_ALL: u32 = 0;
const CPU_TYPE_X86_64: u32 = 0x01000007;
const CPU_SUBTYPE_X86_64_ALL: u32 = 3;
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

const S_REGULAR: u32 = 0x0;
const S_NON_LAZY_SYMBOL_POINTERS: u32 = 0x6;
const S_ZEROFILL: u32 = 0x1;
const S_ATTR_PURE_INSTRUCTIONS: u32 = 0x8000_0000;
const S_ATTR_SOME_INSTRUCTIONS: u32 = 0x0000_0400;

const VM_PROT_READ: u32 = 1;
const VM_PROT_WRITE: u32 = 2;
const VM_PROT_EXECUTE: u32 = 4;
const VM_PROT_READ_WRITE: u32 = VM_PROT_READ | VM_PROT_WRITE;
const VM_PROT_READ_EXECUTE: u32 = VM_PROT_READ | VM_PROT_EXECUTE;
const VM_PROT_ALL: u32 = VM_PROT_READ | VM_PROT_WRITE | VM_PROT_EXECUTE;

const N_EXT: u8 = 0x01;
const N_SECT: u8 = 0x0E;

const PLATFORM_MACOS: u32 = 1;
const TOOL_LD: u32 = 3;
const MACOS_14_0: u32 = 0x000E0000;

const PAGE_SIZE_ARM64: u64 = 0x4000; // 16KB
const PAGE_SIZE_X86_64: u64 = 0x1000; // 4KB
const PAGEZERO_SIZE: u64 = 0x1_0000_0000; // 4GB

const SEGMENT_CMD_SIZE: u32 = size_of::<SegmentCommand64>() as u32;
const SECTION_HEADER_SIZE: u32 = size_of::<Section64>() as u32;
const MACHO_HEADER_SIZE: u32 = size_of::<MachHeader64>() as u32;

// ── Mach-O binary structs ───────────────────────────────────────────────

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct MachHeader64 {
    magic: U32Le,
    cputype: U32Le,
    cpusubtype: U32Le,
    filetype: U32Le,
    ncmds: U32Le,
    sizeofcmds: U32Le,
    flags: U32Le,
    reserved: U32Le,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct SegmentCommand64 {
    cmd: U32Le,
    cmdsize: U32Le,
    segname: [u8; 16],
    vmaddr: U64Le,
    vmsize: U64Le,
    fileoff: U64Le,
    filesize: U64Le,
    maxprot: U32Le,
    initprot: U32Le,
    nsects: U32Le,
    flags: U32Le,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct Section64 {
    sectname: [u8; 16],
    segname: [u8; 16],
    addr: U64Le,
    size: U64Le,
    offset: U32Le,
    align: U32Le,
    reloff: U32Le,
    nreloc: U32Le,
    flags: U32Le,
    reserved1: U32Le,
    reserved2: U32Le,
    reserved3: U32Le,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct Nlist64 {
    n_strx: U32Le,
    n_type: u8,
    n_sect: u8,
    n_desc: U16Le,
    n_value: U64Le,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct LoadDylinkerCmd {
    cmd: U32Le,
    cmdsize: U32Le,
    name_offset: U32Le,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct EntryPointCmd {
    cmd: U32Le,
    cmdsize: U32Le,
    entryoff: U64Le,
    stacksize: U64Le,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct DylibCmd {
    cmd: U32Le,
    cmdsize: U32Le,
    name_offset: U32Le,
    timestamp: U32Le,
    current_version: U32Le,
    compat_version: U32Le,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct DyldInfoCmd {
    cmd: U32Le,
    cmdsize: U32Le,
    rebase_off: U32Le,
    rebase_size: U32Le,
    bind_off: U32Le,
    bind_size: U32Le,
    weak_bind_off: U32Le,
    weak_bind_size: U32Le,
    lazy_bind_off: U32Le,
    lazy_bind_size: U32Le,
    export_off: U32Le,
    export_size: U32Le,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct SymtabCmd {
    cmd: U32Le,
    cmdsize: U32Le,
    symoff: U32Le,
    nsyms: U32Le,
    stroff: U32Le,
    strsize: U32Le,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, FromZeros)]
struct DysymtabCmd {
    cmd: U32Le,
    cmdsize: U32Le,
    ilocalsym: U32Le,
    nlocalsym: U32Le,
    iextdefsym: U32Le,
    nextdefsym: U32Le,
    iundefsym: U32Le,
    nundefsym: U32Le,
    tocoff: U32Le,
    ntoc: U32Le,
    modtaboff: U32Le,
    nmodtab: U32Le,
    extrefsymoff: U32Le,
    nextrefsyms: U32Le,
    indirectsymoff: U32Le,
    nindirectsyms: U32Le,
    extreloff: U32Le,
    nextrel: U32Le,
    locreloff: U32Le,
    nlocrel: U32Le,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct BuildVersionCmd {
    cmd: U32Le,
    cmdsize: U32Le,
    platform: U32Le,
    minos: U32Le,
    sdk: U32Le,
    ntools: U32Le,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct BuildToolVersion {
    tool: U32Le,
    version: U32Le,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct LinkeditDataCmd {
    cmd: U32Le,
    cmdsize: U32Le,
    dataoff: U32Le,
    datasize: U32Le,
}

// Code signature structs (big-endian)

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct CsSuperBlob {
    magic: U32Be,
    length: U32Be,
    count: U32Be,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct CsBlobIndex {
    typ: U32Be,
    offset: U32Be,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct CsCodeDirectory {
    magic: U32Be,
    length: U32Be,
    version: U32Be,
    flags: U32Be,
    hash_offset: U32Be,
    ident_offset: U32Be,
    n_special_slots: U32Be,
    n_code_slots: U32Be,
    code_limit: U32Be,
    hash_size: u8,
    hash_type: u8,
    platform: u8,
    page_size: u8,
    spare2: U32Be,
    scatter_offset: U32Be,
    team_offset: U32Be,
    spare3: U32Be,
    code_limit_64: U64Be,
    exec_seg_base: U64Be,
    exec_seg_limit: U64Be,
    exec_seg_flags: U64Be,
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn pad16(name: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let len = name.len().min(16);
    out[..len].copy_from_slice(&name[..len]);
    out
}

fn write_struct<T: IntoBytes + Immutable>(buf: &mut [u8], off: usize, val: &T) -> usize {
    let bytes = val.as_bytes();
    buf[off..off + bytes.len()].copy_from_slice(bytes);
    off + bytes.len()
}

fn write_at(buf: &mut [u8], off: usize, data: &[u8]) {
    buf[off..off + data.len()].copy_from_slice(data);
}

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

// ── Layout ──────────────────────────────────────────────────────────────

pub(crate) struct MachOLayout {
    pub(crate) text_vmaddr: u64,
    pub(crate) text_vmsize: u64,
    pub(crate) text_filesize: u64,
    pub(crate) text_sec_offset: u64,
    pub(crate) text_sec_vmaddr: u64,
    pub(crate) text_sec_size: u64,
    pub(crate) const_sec_offset: u64,
    pub(crate) const_sec_vmaddr: u64,
    pub(crate) const_sec_size: u64,

    pub(crate) data_vmaddr: u64,
    pub(crate) data_vmsize: u64,
    pub(crate) data_fileoff: u64,
    pub(crate) data_filesize: u64,
    pub(crate) data_sec_offset: u64,
    pub(crate) data_sec_vmaddr: u64,
    pub(crate) data_sec_size: u64,
    pub(crate) got_sec_offset: u64,
    pub(crate) got_sec_vmaddr: u64,
    pub(crate) got_sec_size: u64,
    pub(crate) bss_sec_vmaddr: u64,
    pub(crate) bss_sec_size: u64,

    pub(crate) linkedit_vmaddr: u64,
    pub(crate) linkedit_fileoff: u64,

    pub(crate) got: HashMap<SymbolRef, u64>,
    pub(crate) got_entries: Vec<(SymbolRef, bool)>,

    pub(crate) sizeofcmds: u32,
    pub(crate) ncmds: u32,
    pub(crate) is_x86_64: bool,
    pub(crate) page_size: u64,
}

/// Compute the Mach-O layout: segment placement, GOT allocation.
pub(crate) fn layout_macho(state: &mut LinkState) -> MachOLayout {
    // Detect architecture from relocation types present
    let is_x86_64 = state.relocs.iter().any(|r| matches!(r.r_type,
        RelocType::X86_64 | RelocType::X86Pc32 | RelocType::X86Plt32
        | RelocType::X86Gotpcrel | RelocType::X86Gotpcrelx
        | RelocType::X86RexGotpcrelx | RelocType::X86_32 | RelocType::X86_32S));
    let page_size = if is_x86_64 { PAGE_SIZE_X86_64 } else { PAGE_SIZE_ARM64 };
    let buckets = classify_sections(state);

    let got_symbols = collect_unique_symbols(state.relocs.iter(), |r| {
        // Standard GOT relocation types
        if matches!(r.r_type,
            RelocType::Aarch64AdrGotPage | RelocType::Aarch64Ld64GotLo12Nc
            | RelocType::X86Gotpcrel | RelocType::X86Gotpcrelx
            | RelocType::X86RexGotpcrelx)
        {
            return true;
        }
        // MOVW relocations targeting dynamic symbols need GOT slots so we can
        // rewrite MOVZ/MOVK → ADRP+LDR at relocation time.
        if matches!(r.r_type,
            RelocType::Aarch64MovwUabsG0Nc | RelocType::Aarch64MovwUabsG1Nc
            | RelocType::Aarch64MovwUabsG2Nc | RelocType::Aarch64MovwUabsG3)
        {
            let is_dynamic = match &r.target {
                SymbolRef::Global(name) => matches!(
                    state.globals.get(name),
                    Some(SymbolDef::Dynamic) | None
                ),
                _ => false,
            };
            return is_dynamic;
        }
        false
    });

    let has_const = buckets.rx.iter().any(|&i| state.sections[i].kind == SectionKind::ReadOnly);
    let has_data = !buckets.rw.is_empty();
    let has_bss = buckets.rw.iter().any(|&i| state.sections[i].kind.is_nobits());
    let has_got = !got_symbols.is_empty();

    let text_nsects = 1 + if has_const { 1 } else { 0 };
    let data_nsects = (if has_data { 1 } else { 0 })
        + (if has_got { 1 } else { 0 })
        + (if has_bss { 1 } else { 0 });

    let dylib_name = "/usr/lib/libSystem.B.dylib\0";
    let dylib_cmd_size = align_up((size_of::<DylibCmd>() + dylib_name.len()) as u64, 8) as u32;
    let dylinker_name = "/usr/lib/dyld\0";
    let dylinker_cmd_size = align_up((size_of::<LoadDylinkerCmd>() + dylinker_name.len()) as u64, 8) as u32;
    let build_version_size = size_of::<BuildVersionCmd>() as u32 + size_of::<BuildToolVersion>() as u32;

    let preliminary_sizeofcmds: u32 =
        SEGMENT_CMD_SIZE                                           // __PAGEZERO
        + SEGMENT_CMD_SIZE + SECTION_HEADER_SIZE * text_nsects as u32  // __TEXT
        + SEGMENT_CMD_SIZE + SECTION_HEADER_SIZE * data_nsects.max(1) as u32 // __DATA
        + SEGMENT_CMD_SIZE                                         // __LINKEDIT
        + dylinker_cmd_size
        + size_of::<EntryPointCmd>() as u32
        + dylib_cmd_size
        + size_of::<DyldInfoCmd>() as u32
        + size_of::<SymtabCmd>() as u32
        + size_of::<DysymtabCmd>() as u32
        + build_version_size
        + size_of::<LinkeditDataCmd>() as u32;
    let header_size = MACHO_HEADER_SIZE as u64 + preliminary_sizeofcmds as u64;

    // Phase 2: Layout sections with actual addresses
    let text_vmaddr = PAGEZERO_SIZE;

    let mut cursor = text_vmaddr + align_up(header_size, 16);
    let text_sec_vmaddr = cursor;
    let text_sec_offset = cursor - text_vmaddr;
    for &idx in &buckets.rx {
        let sec = &mut state.sections[idx];
        if sec.kind == SectionKind::ReadOnly {
            continue;
        }
        cursor = align_up(cursor, sec.align);
        sec.vaddr = Some(cursor);
        cursor += sec.size;
    }
    let text_sec_size = cursor - text_sec_vmaddr;

    let const_sec_vmaddr = align_up(cursor, 8);
    let const_sec_offset = const_sec_vmaddr - text_vmaddr;
    let mut actual_has_const = false;
    cursor = const_sec_vmaddr;
    for &idx in &buckets.rx {
        let sec = &mut state.sections[idx];
        if sec.kind == SectionKind::ReadOnly {
            cursor = align_up(cursor, sec.align);
            sec.vaddr = Some(cursor);
            cursor += sec.size;
            actual_has_const = true;
        }
    }
    let const_sec_size = cursor - const_sec_vmaddr;

    let text_vmsize = align_up(cursor - text_vmaddr, page_size);
    let text_filesize = text_vmsize;

    let data_vmaddr = text_vmaddr + text_vmsize;
    let data_fileoff = text_filesize;
    let mut data_cursor = data_vmaddr;

    let data_sec_vmaddr = data_cursor;
    let data_sec_offset = data_fileoff;
    for &idx in &buckets.rw {
        let sec = &mut state.sections[idx];
        if sec.kind.is_nobits() { continue; }
        data_cursor = align_up(data_cursor, sec.align);
        sec.vaddr = Some(data_cursor);
        data_cursor += sec.size;
    }
    let data_sec_size = data_cursor - data_sec_vmaddr;

    data_cursor = align_up(data_cursor, 8);
    let got_sec_vmaddr = data_cursor;
    let got_sec_offset = data_fileoff + (got_sec_vmaddr - data_vmaddr);

    let mut got_entries = Vec::new();
    let mut got = HashMap::new();
    for sym in &got_symbols {
        let is_external = match sym {
            SymbolRef::Global(name) => match state.globals.get(name) {
                Some(SymbolDef::Dynamic) => true,
                Some(SymbolDef::Defined { .. }) => false,
                None => true,
            },
            SymbolRef::Local(_, _) => false,
        };
        got.insert(sym.clone(), data_cursor);
        got_entries.push((sym.clone(), is_external));
        data_cursor += 8;
    }
    let got_sec_size = data_cursor - got_sec_vmaddr;

    let bss_sec_vmaddr = align_up(data_cursor, 8);
    let bss_start = bss_sec_vmaddr;
    let mut bss_cursor = bss_sec_vmaddr;
    for &idx in &buckets.rw {
        let sec = &mut state.sections[idx];
        if !sec.kind.is_nobits() { continue; }
        bss_cursor = align_up(bss_cursor, sec.align);
        sec.vaddr = Some(bss_cursor);
        bss_cursor += sec.size;
    }
    let bss_sec_size = bss_cursor - bss_start;

    let data_filesz = if got_sec_size > 0 {
        got_sec_vmaddr + got_sec_size - data_vmaddr
    } else {
        data_sec_size
    };
    let data_filesize = data_filesz;
    let data_memsz = bss_cursor - data_vmaddr;
    let data_vmsize = align_up(data_memsz, page_size);

    let linkedit_vmaddr = data_vmaddr + data_vmsize;
    let linkedit_fileoff = data_fileoff + align_up(data_filesize, page_size);

    let actual_data_nsects = (if data_sec_size > 0 { 1 } else { 0 })
        + (if got_sec_size > 0 { 1 } else { 0 })
        + (if bss_sec_size > 0 { 1 } else { 0 });

    let actual_text_nsects = 1 + if actual_has_const && const_sec_size > 0 { 1 } else { 0 };
    let mut ncmds = 0u32;
    let mut sizeofcmds = 0u32;
    ncmds += 1; sizeofcmds += SEGMENT_CMD_SIZE; // __PAGEZERO
    ncmds += 1; sizeofcmds += SEGMENT_CMD_SIZE + SECTION_HEADER_SIZE * actual_text_nsects as u32;
    if actual_data_nsects > 0 {
        ncmds += 1; sizeofcmds += SEGMENT_CMD_SIZE + SECTION_HEADER_SIZE * actual_data_nsects as u32;
    }
    ncmds += 1; sizeofcmds += SEGMENT_CMD_SIZE; // __LINKEDIT
    ncmds += 1; sizeofcmds += dylinker_cmd_size;
    ncmds += 1; sizeofcmds += size_of::<EntryPointCmd>() as u32;
    ncmds += 1; sizeofcmds += dylib_cmd_size;
    ncmds += 1; sizeofcmds += size_of::<DyldInfoCmd>() as u32;
    ncmds += 1; sizeofcmds += size_of::<SymtabCmd>() as u32;
    ncmds += 1; sizeofcmds += size_of::<DysymtabCmd>() as u32;
    ncmds += 1; sizeofcmds += build_version_size;
    ncmds += 1; sizeofcmds += size_of::<LinkeditDataCmd>() as u32;

    let final_header_size = MACHO_HEADER_SIZE as u64 + sizeofcmds as u64;
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
        is_x86_64,
        page_size,
    }
}

// ── Emission ────────────────────────────────────────────────────────────

pub(crate) fn emit_macho_bytes(
    state: &LinkState,
    layout: &MachOLayout,
    entry_name: &str,
    rebase_entries: &[(u64, i64)],
    bind_entries: &[(String, u64)],
) -> Result<Vec<u8>, LinkError> {
    let entry_addr = state
        .globals
        .get(entry_name)
        .map(|def| match def {
            SymbolDef::Defined { section, value } => {
                state.sections[*section].vaddr.unwrap() + value
            }
            SymbolDef::Dynamic => panic!("entry point cannot be a dynamic symbol"),
        })
        .ok_or_else(|| LinkError::MissingEntry(entry_name.to_string()))?;

    let entryoff = entry_addr - layout.text_vmaddr;

    // Build __LINKEDIT content first to know sizes
    let rebase_data = build_rebase_opcodes(layout, rebase_entries);
    let bind_data = build_bind_opcodes(layout, bind_entries);
    let export_data = build_export_trie(entry_name, entryoff);

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
    let nsyms = (symtab_data.len() / size_of::<Nlist64>()) as u32;
    linkedit_cursor += symtab_data.len() as u64;
    linkedit_cursor = align_up(linkedit_cursor, 8);

    let strtab_off = linkedit_cursor;
    let strtab_size = strtab_data.len() as u32;
    linkedit_cursor += align_up(strtab_size as u64, 8);

    // Code signature: must be 16-byte aligned
    linkedit_cursor = align_up(linkedit_cursor, 16);
    let codesig_off = linkedit_cursor;
    let code_limit = codesig_off as u32;
    let cs_page_size: u32 = 4096;
    let n_code_slots = (code_limit + cs_page_size - 1) / cs_page_size;
    let ident = "_main\0";
    let cd_size = size_of::<CsCodeDirectory>() as u32 + ident.len() as u32 + n_code_slots * 32;
    let codesig_size = size_of::<CsSuperBlob>() as u32
        + size_of::<CsBlobIndex>() as u32
        + cd_size;
    let codesig_size_aligned = align_up(codesig_size as u64, 16) as u32;
    linkedit_cursor += codesig_size_aligned as u64;

    let linkedit_filesize = linkedit_cursor - layout.linkedit_fileoff;
    let linkedit_vmsize = align_up(linkedit_filesize, layout.page_size);

    // Build the output buffer
    let total_size = linkedit_cursor as usize;
    let mut buf = vec![0u8; total_size];

    // ── Mach-O header ──
    let mut off = write_struct(&mut buf, 0, &MachHeader64 {
        magic: U32Le::new(MH_MAGIC_64),
        cputype: U32Le::new(if layout.is_x86_64 { CPU_TYPE_X86_64 } else { CPU_TYPE_ARM64 }),
        cpusubtype: U32Le::new(if layout.is_x86_64 { CPU_SUBTYPE_X86_64_ALL } else { CPU_SUBTYPE_ARM64_ALL }),
        filetype: U32Le::new(MH_EXECUTE),
        ncmds: U32Le::new(layout.ncmds),
        sizeofcmds: U32Le::new(layout.sizeofcmds),
        flags: U32Le::new(MH_PIE | MH_TWOLEVEL | MH_DYLDLINK),
        reserved: U32Le::new(0),
    });

    // ── Load commands ──

    // LC_SEGMENT_64 __PAGEZERO
    off = write_struct(&mut buf, off, &SegmentCommand64 {
        cmd: U32Le::new(LC_SEGMENT_64),
        cmdsize: U32Le::new(SEGMENT_CMD_SIZE),
        segname: pad16(b"__PAGEZERO"),
        vmaddr: U64Le::new(0),
        vmsize: U64Le::new(PAGEZERO_SIZE),
        fileoff: U64Le::new(0),
        filesize: U64Le::new(0),
        maxprot: U32Le::new(0),
        initprot: U32Le::new(0),
        nsects: U32Le::new(0),
        flags: U32Le::new(0),
    });

    // LC_SEGMENT_64 __TEXT
    let text_nsects = 1 + if layout.const_sec_size > 0 { 1u32 } else { 0 };
    off = write_struct(&mut buf, off, &SegmentCommand64 {
        cmd: U32Le::new(LC_SEGMENT_64),
        cmdsize: U32Le::new(SEGMENT_CMD_SIZE + SECTION_HEADER_SIZE * text_nsects),
        segname: pad16(b"__TEXT"),
        vmaddr: U64Le::new(layout.text_vmaddr),
        vmsize: U64Le::new(layout.text_vmsize),
        fileoff: U64Le::new(0),
        filesize: U64Le::new(layout.text_filesize),
        maxprot: U32Le::new(VM_PROT_READ_EXECUTE),
        initprot: U32Le::new(VM_PROT_READ_EXECUTE),
        nsects: U32Le::new(text_nsects),
        flags: U32Le::new(0),
    });

    // __text section header
    off = write_struct(&mut buf, off, &Section64 {
        sectname: pad16(b"__text"),
        segname: pad16(b"__TEXT"),
        addr: U64Le::new(layout.text_sec_vmaddr),
        size: U64Le::new(layout.text_sec_size),
        offset: U32Le::new(layout.text_sec_offset as u32),
        align: U32Le::new(if layout.is_x86_64 { 4 } else { 2 }), // 2^4=16 or 2^2=4
        reloff: U32Le::new(0),
        nreloc: U32Le::new(0),
        flags: U32Le::new(S_REGULAR | S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS),
        reserved1: U32Le::new(0),
        reserved2: U32Le::new(0),
        reserved3: U32Le::new(0),
    });

    // __const section header (if present)
    if layout.const_sec_size > 0 {
        off = write_struct(&mut buf, off, &Section64 {
            sectname: pad16(b"__const"),
            segname: pad16(b"__TEXT"),
            addr: U64Le::new(layout.const_sec_vmaddr),
            size: U64Le::new(layout.const_sec_size),
            offset: U32Le::new(layout.const_sec_offset as u32),
            align: U32Le::new(0),
            reloff: U32Le::new(0),
            nreloc: U32Le::new(0),
            flags: U32Le::new(S_REGULAR),
            reserved1: U32Le::new(0),
            reserved2: U32Le::new(0),
            reserved3: U32Le::new(0),
        });
    }

    // LC_SEGMENT_64 __DATA
    let data_nsects = (if layout.data_sec_size > 0 { 1u32 } else { 0 })
        + (if layout.got_sec_size > 0 { 1 } else { 0 })
        + (if layout.bss_sec_size > 0 { 1 } else { 0 });
    if data_nsects > 0 {
        off = write_struct(&mut buf, off, &SegmentCommand64 {
            cmd: U32Le::new(LC_SEGMENT_64),
            cmdsize: U32Le::new(SEGMENT_CMD_SIZE + SECTION_HEADER_SIZE * data_nsects),
            segname: pad16(b"__DATA"),
            vmaddr: U64Le::new(layout.data_vmaddr),
            vmsize: U64Le::new(layout.data_vmsize),
            fileoff: U64Le::new(layout.data_fileoff),
            filesize: U64Le::new(layout.data_filesize),
            maxprot: U32Le::new(VM_PROT_READ_WRITE),
            initprot: U32Le::new(VM_PROT_READ_WRITE),
            nsects: U32Le::new(data_nsects),
            flags: U32Le::new(0),
        });

        if layout.data_sec_size > 0 {
            off = write_struct(&mut buf, off, &Section64 {
                sectname: pad16(b"__data"),
                segname: pad16(b"__DATA"),
                addr: U64Le::new(layout.data_sec_vmaddr),
                size: U64Le::new(layout.data_sec_size),
                offset: U32Le::new(layout.data_sec_offset as u32),
                align: U32Le::new(3), // 2^3 = 8
                reloff: U32Le::new(0),
                nreloc: U32Le::new(0),
                flags: U32Le::new(S_REGULAR),
                reserved1: U32Le::new(0),
                reserved2: U32Le::new(0),
                reserved3: U32Le::new(0),
            });
        }
        if layout.got_sec_size > 0 {
            off = write_struct(&mut buf, off, &Section64 {
                sectname: pad16(b"__got"),
                segname: pad16(b"__DATA"),
                addr: U64Le::new(layout.got_sec_vmaddr),
                size: U64Le::new(layout.got_sec_size),
                offset: U32Le::new(layout.got_sec_offset as u32),
                align: U32Le::new(3),
                reloff: U32Le::new(0),
                nreloc: U32Le::new(0),
                flags: U32Le::new(S_NON_LAZY_SYMBOL_POINTERS),
                reserved1: U32Le::new(0),
                reserved2: U32Le::new(0),
                reserved3: U32Le::new(0),
            });
        }
        if layout.bss_sec_size > 0 {
            off = write_struct(&mut buf, off, &Section64 {
                sectname: pad16(b"__bss"),
                segname: pad16(b"__DATA"),
                addr: U64Le::new(layout.bss_sec_vmaddr),
                size: U64Le::new(layout.bss_sec_size),
                offset: U32Le::new(0),
                align: U32Le::new(3),
                reloff: U32Le::new(0),
                nreloc: U32Le::new(0),
                flags: U32Le::new(S_ZEROFILL),
                reserved1: U32Le::new(0),
                reserved2: U32Le::new(0),
                reserved3: U32Le::new(0),
            });
        }
    }

    // LC_SEGMENT_64 __LINKEDIT
    off = write_struct(&mut buf, off, &SegmentCommand64 {
        cmd: U32Le::new(LC_SEGMENT_64),
        cmdsize: U32Le::new(SEGMENT_CMD_SIZE),
        segname: pad16(b"__LINKEDIT"),
        vmaddr: U64Le::new(layout.linkedit_vmaddr),
        vmsize: U64Le::new(linkedit_vmsize),
        fileoff: U64Le::new(layout.linkedit_fileoff),
        filesize: U64Le::new(linkedit_filesize),
        maxprot: U32Le::new(VM_PROT_ALL),
        initprot: U32Le::new(VM_PROT_READ),
        nsects: U32Le::new(0),
        flags: U32Le::new(0),
    });

    // LC_LOAD_DYLINKER
    let dylinker_name_bytes = b"/usr/lib/dyld\0";
    let dylinker_cmd_size = align_up(
        (size_of::<LoadDylinkerCmd>() + dylinker_name_bytes.len()) as u64, 8,
    ) as u32;
    off = write_struct(&mut buf, off, &LoadDylinkerCmd {
        cmd: U32Le::new(LC_LOAD_DYLINKER),
        cmdsize: U32Le::new(dylinker_cmd_size),
        name_offset: U32Le::new(size_of::<LoadDylinkerCmd>() as u32),
    });
    buf[off..off + dylinker_name_bytes.len()].copy_from_slice(dylinker_name_bytes);
    off = (off - size_of::<LoadDylinkerCmd>()) + dylinker_cmd_size as usize;

    // LC_MAIN
    off = write_struct(&mut buf, off, &EntryPointCmd {
        cmd: U32Le::new(LC_MAIN),
        cmdsize: U32Le::new(size_of::<EntryPointCmd>() as u32),
        entryoff: U64Le::new(entryoff),
        stacksize: U64Le::new(0),
    });

    // LC_LOAD_DYLIB
    let dylib_name_bytes = b"/usr/lib/libSystem.B.dylib\0";
    let dylib_cmd_size = align_up(
        (size_of::<DylibCmd>() + dylib_name_bytes.len()) as u64, 8,
    ) as u32;
    off = write_struct(&mut buf, off, &DylibCmd {
        cmd: U32Le::new(LC_LOAD_DYLIB),
        cmdsize: U32Le::new(dylib_cmd_size),
        name_offset: U32Le::new(size_of::<DylibCmd>() as u32),
        timestamp: U32Le::new(0),
        current_version: U32Le::new(0x010000),
        compat_version: U32Le::new(0x010000),
    });
    buf[off..off + dylib_name_bytes.len()].copy_from_slice(dylib_name_bytes);
    off = (off - size_of::<DylibCmd>()) + dylib_cmd_size as usize;

    // LC_DYLD_INFO_ONLY
    off = write_struct(&mut buf, off, &DyldInfoCmd {
        cmd: U32Le::new(LC_DYLD_INFO_ONLY),
        cmdsize: U32Le::new(size_of::<DyldInfoCmd>() as u32),
        rebase_off: U32Le::new(rebase_off as u32),
        rebase_size: U32Le::new(rebase_size),
        bind_off: U32Le::new(bind_off as u32),
        bind_size: U32Le::new(bind_size),
        weak_bind_off: U32Le::new(0),
        weak_bind_size: U32Le::new(0),
        lazy_bind_off: U32Le::new(0),
        lazy_bind_size: U32Le::new(0),
        export_off: U32Le::new(export_off as u32),
        export_size: U32Le::new(export_size),
    });

    // LC_SYMTAB
    off = write_struct(&mut buf, off, &SymtabCmd {
        cmd: U32Le::new(LC_SYMTAB),
        cmdsize: U32Le::new(size_of::<SymtabCmd>() as u32),
        symoff: U32Le::new(symtab_off as u32),
        nsyms: U32Le::new(nsyms),
        stroff: U32Le::new(strtab_off as u32),
        strsize: U32Le::new(strtab_size),
    });

    // LC_DYSYMTAB
    let mut dysymtab = DysymtabCmd::new_zeroed();
    dysymtab.cmd = U32Le::new(LC_DYSYMTAB);
    dysymtab.cmdsize = U32Le::new(size_of::<DysymtabCmd>() as u32);
    dysymtab.nlocalsym = U32Le::new(nlocalsym);
    dysymtab.iextdefsym = U32Le::new(nlocalsym);
    dysymtab.nextdefsym = U32Le::new(nextdefsym);
    dysymtab.iundefsym = U32Le::new(nlocalsym + nextdefsym);
    dysymtab.nundefsym = U32Le::new(nundefsym);
    off = write_struct(&mut buf, off, &dysymtab);

    // LC_BUILD_VERSION (platform = macOS, minos = 14.0)
    off = write_struct(&mut buf, off, &BuildVersionCmd {
        cmd: U32Le::new(LC_BUILD_VERSION),
        cmdsize: U32Le::new(
            size_of::<BuildVersionCmd>() as u32 + size_of::<BuildToolVersion>() as u32,
        ),
        platform: U32Le::new(PLATFORM_MACOS),
        minos: U32Le::new(MACOS_14_0),
        sdk: U32Le::new(MACOS_14_0),
        ntools: U32Le::new(1),
    });
    off = write_struct(&mut buf, off, &BuildToolVersion {
        tool: U32Le::new(TOOL_LD),
        version: U32Le::new(0x010000),
    });

    // LC_CODE_SIGNATURE
    off = write_struct(&mut buf, off, &LinkeditDataCmd {
        cmd: U32Le::new(LC_CODE_SIGNATURE),
        cmdsize: U32Le::new(size_of::<LinkeditDataCmd>() as u32),
        dataoff: U32Le::new(code_limit),
        datasize: U32Le::new(codesig_size),
    });

    let _ = off;

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
        if sec.data.is_empty() || sec.kind.is_nobits() { continue; }
        if vaddr >= layout.data_sec_vmaddr
            && vaddr < layout.data_sec_vmaddr + layout.data_sec_size
        {
            let off = (layout.data_fileoff + (vaddr - layout.data_vmaddr)) as usize;
            buf[off..off + sec.data.len()].copy_from_slice(&sec.data);
        }
    }

    // __DATA,__got: fill internal GOT entries
    for (sym_ref, &got_vaddr) in &layout.got {
        let is_external = layout.got_entries.iter()
            .any(|(n, ext)| n == sym_ref && *ext);
        if is_external { continue; } // filled by dyld
        let sym_addr = resolve_symbol(state, sym_ref, None)
            .ok_or_else(|| LinkError::UndefinedSymbols(vec![sym_ref.name().to_string()]))?;
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

/// Mach-O C symbol names have a leading `_` prefix. ELF names don't.
/// Prepend `_` unless the name already starts with `_` (from Mach-O input).
fn macho_mangle(name: &str) -> String {
    format!("_{name}")
}

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
        let macho_name = macho_mangle(sym_name);
        // Set dylib ordinal (1 = first LC_LOAD_DYLIB = libSystem)
        ops.push(BIND_OPCODE_SET_DYLIB_ORDINAL_IMM | 1);
        // Set symbol name
        ops.push(BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM | 0);
        ops.extend_from_slice(macho_name.as_bytes());
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
    let macho_entry = macho_mangle(entry_name);
    trie.extend_from_slice(macho_entry.as_bytes());
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
    let mut symtab = Vec::new();
    let mut strtab = vec![0u8]; // index 0 = empty string

    // Section numbering (1-based): __text=1, __const=2 (if exists), __data=3, __got=4, __bss=5
    let mut sect_num = 1u8;
    let text_sect = sect_num; sect_num += 1;
    let const_sect = if layout.const_sec_size > 0 { let s = sect_num; sect_num += 1; s } else { 0 };
    let data_sect = if layout.data_sec_size > 0 { let s = sect_num; sect_num += 1; s } else { 0 };
    if layout.got_sec_size > 0 { sect_num += 1; }
    let bss_sect = if layout.bss_sec_size > 0 { let s = sect_num; sect_num += 1; s } else { 0 };
    let _ = sect_num;

    fn add_string(strtab: &mut Vec<u8>, s: &str) -> u32 {
        let offset = strtab.len() as u32;
        strtab.extend_from_slice(s.as_bytes());
        strtab.push(0);
        offset
    }

    let nlocalsym = 0u32;

    // Defined external symbols
    let mut extdef_syms: Vec<(String, u64, u8)> = Vec::new();
    for (name, def) in &state.globals {
        let SymbolDef::Defined { section, value } = def else { continue; };
        let sec = &state.sections[*section];
        let value = sec.vaddr.unwrap() + value;
        let sect = if value >= layout.text_sec_vmaddr && value < layout.text_sec_vmaddr + layout.text_sec_size {
            text_sect
        } else if layout.const_sec_size > 0 && value >= layout.const_sec_vmaddr && value < layout.const_sec_vmaddr + layout.const_sec_size {
            const_sect
        } else if layout.data_sec_size > 0 && value >= layout.data_sec_vmaddr && value < layout.data_sec_vmaddr + layout.data_sec_size {
            data_sect
        } else if layout.bss_sec_size > 0 && value >= layout.bss_sec_vmaddr && value < layout.bss_sec_vmaddr + layout.bss_sec_size {
            bss_sect
        } else {
            panic!(
                "symbol {name:?} at {value:#x} does not fall in any known Mach-O section \
                 (text={:#x}..{:#x}, const={:#x}..{:#x}, data={:#x}..{:#x}, bss={:#x}..{:#x})",
                layout.text_sec_vmaddr, layout.text_sec_vmaddr + layout.text_sec_size,
                layout.const_sec_vmaddr, layout.const_sec_vmaddr + layout.const_sec_size,
                layout.data_sec_vmaddr, layout.data_sec_vmaddr + layout.data_sec_size,
                layout.bss_sec_vmaddr, layout.bss_sec_vmaddr + layout.bss_sec_size,
            )
        };
        extdef_syms.push((name.clone(), value, sect));
    }
    extdef_syms.sort_by(|a, b| a.0.cmp(&b.0));
    let nextdefsym = extdef_syms.len() as u32;

    for (name, value, sect) in &extdef_syms {
        let macho_name = macho_mangle(name);
        let nlist = Nlist64 {
            n_strx: U32Le::new(add_string(&mut strtab, &macho_name)),
            n_type: N_SECT | N_EXT,
            n_sect: *sect,
            n_desc: U16Le::new(0),
            n_value: U64Le::new(*value),
        };
        symtab.extend_from_slice(nlist.as_bytes());
    }

    // Undefined symbols (external dylib references)
    let mut undef_syms: Vec<String> = bind_entries.iter().map(|(n, _)| n.clone()).collect();
    undef_syms.sort();
    undef_syms.dedup();
    let nundefsym = undef_syms.len() as u32;

    for name in &undef_syms {
        let macho_name = macho_mangle(name);
        let nlist = Nlist64 {
            n_strx: U32Le::new(add_string(&mut strtab, &macho_name)),
            n_type: N_EXT, // undefined
            n_sect: 0,     // NO_SECT
            // REFERENCE_FLAG_UNDEFINED_NON_LAZY (0) | SET_LIBRARY_ORDINAL(1)
            n_desc: U16Le::new(0x0100), // library ordinal 1 in high byte
            n_value: U64Le::new(0),
        };
        symtab.extend_from_slice(nlist.as_bytes());
    }

    (symtab, strtab, nlocalsym, nextdefsym, nundefsym)
}

// ── Ad-hoc code signature ────────────────────────────────────────────

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

fn build_code_signature(
    file_bytes: &[u8],
    code_limit: u32,
    n_code_slots: u32,
    layout: &MachOLayout,
) -> Vec<u8> {
    let ident = b"_main\0";

    let hash_offset = size_of::<CsCodeDirectory>() as u32 + ident.len() as u32;
    let cd_length = hash_offset + n_code_slots * CS_SHA256_LEN as u32;

    let blob_offset = size_of::<CsSuperBlob>() as u32 + size_of::<CsBlobIndex>() as u32;
    let super_blob_length = blob_offset + cd_length;

    let mut sig = vec![0u8; super_blob_length as usize];

    // SuperBlob header
    let mut off = write_struct(&mut sig, 0, &CsSuperBlob {
        magic: U32Be::new(CSMAGIC_EMBEDDED_SIGNATURE),
        length: U32Be::new(super_blob_length),
        count: U32Be::new(1),
    });

    // BlobIndex[0]: CodeDirectory
    off = write_struct(&mut sig, off, &CsBlobIndex {
        typ: U32Be::new(0), // CSSLOT_CODEDIRECTORY
        offset: U32Be::new(blob_offset),
    });

    // CodeDirectory
    let cd_off = off;
    off = write_struct(&mut sig, off, &CsCodeDirectory {
        magic: U32Be::new(CSMAGIC_CODEDIRECTORY),
        length: U32Be::new(cd_length),
        version: U32Be::new(CS_SUPPORTSEXECSEG),
        flags: U32Be::new(CS_ADHOC | CS_LINKER_SIGNED),
        hash_offset: U32Be::new(hash_offset),
        ident_offset: U32Be::new(size_of::<CsCodeDirectory>() as u32),
        n_special_slots: U32Be::new(0),
        n_code_slots: U32Be::new(n_code_slots),
        code_limit: U32Be::new(code_limit),
        hash_size: CS_SHA256_LEN,
        hash_type: CS_HASHTYPE_SHA256,
        platform: 0,
        page_size: CS_PAGE_SIZE_LOG2,
        spare2: U32Be::new(0),
        scatter_offset: U32Be::new(0),
        team_offset: U32Be::new(0),
        spare3: U32Be::new(0),
        code_limit_64: U64Be::new(0),
        exec_seg_base: U64Be::new(0), // __TEXT fileoff = 0
        exec_seg_limit: U64Be::new(layout.text_filesize),
        exec_seg_flags: U64Be::new(CS_EXECSEG_MAIN_BINARY),
    });

    // Identifier string
    sig[off..off + ident.len()].copy_from_slice(ident);

    // Page hashes: SHA-256 of each 4KB page of the file up to code_limit
    let hash_start = cd_off + hash_offset as usize;
    for i in 0..n_code_slots {
        let page_start = (i * CS_PAGE_SIZE) as usize;
        let page_end = ((i + 1) * CS_PAGE_SIZE).min(code_limit) as usize;
        let hash = Sha256::digest(&file_bytes[page_start..page_end]);
        let off = hash_start + (i as usize) * 32;
        sig[off..off + 32].copy_from_slice(&hash);
    }

    sig
}

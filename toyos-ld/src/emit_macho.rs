use crate::collect::{Arch, collect_unique_symbols, LinkState, RelocType, SectionKind, SymbolDef, SymbolRef};
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
const MH_HAS_TLV_DESCRIPTORS: u32 = 0x0080_0000;
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
const S_ZEROFILL: u32 = 0x1;
const S_NON_LAZY_SYMBOL_POINTERS: u32 = 0x6;
const S_THREAD_LOCAL_REGULAR: u32 = 0x11;
const S_THREAD_LOCAL_ZEROFILL: u32 = 0x12;
const S_THREAD_LOCAL_VARIABLES: u32 = 0x13;
const S_ATTR_PURE_INSTRUCTIONS: u32 = 0x8000_0000;
const S_ATTR_SOME_INSTRUCTIONS: u32 = 0x0000_0400;

const VM_PROT_READ: u32 = 1;
const VM_PROT_WRITE: u32 = 2;
const VM_PROT_EXECUTE: u32 = 4;
const VM_PROT_READ_WRITE: u32 = VM_PROT_READ | VM_PROT_WRITE;
const VM_PROT_READ_EXECUTE: u32 = VM_PROT_READ | VM_PROT_EXECUTE;

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

// Code signature (big-endian, matching lld's minimal approach)

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

/// A Mach-O output section within a segment.
#[derive(Clone)]
pub(crate) struct MachOSection {
    pub(crate) sectname: [u8; 16],
    pub(crate) segname: [u8; 16],
    pub(crate) vmaddr: u64,
    pub(crate) size: u64,
    /// File offset (0 for zerofill/nobits sections).
    pub(crate) offset: u32,
    pub(crate) align: u32,
    pub(crate) flags: u32,
    /// Index into the indirect symbol table (for S_NON_LAZY_SYMBOL_POINTERS etc.).
    pub(crate) reserved1: u32,
}

pub(crate) struct MachOLayout {
    pub(crate) text_vmaddr: u64,
    pub(crate) text_vmsize: u64,
    pub(crate) text_filesize: u64,
    /// Sections in __TEXT, sorted by vmaddr.
    pub(crate) text_sections: Vec<MachOSection>,

    pub(crate) data_vmaddr: u64,
    pub(crate) data_vmsize: u64,
    pub(crate) data_fileoff: u64,
    pub(crate) data_filesize: u64,
    /// Sections in __DATA, sorted by vmaddr.
    pub(crate) data_sections: Vec<MachOSection>,

    pub(crate) linkedit_vmaddr: u64,
    pub(crate) linkedit_fileoff: u64,

    pub(crate) got: HashMap<SymbolRef, u64>,
    pub(crate) got_entries: Vec<(SymbolRef, bool)>,

    pub(crate) sizeofcmds: u32,
    pub(crate) ncmds: u32,
    pub(crate) arch: Arch,
    pub(crate) page_size: u64,

    /// Start of the TLS template (__thread_data vmaddr) for TLV offset computation.
    pub(crate) tls_template_start: u64,
    /// Whether the binary has TLV descriptors (sets MH_HAS_TLV_DESCRIPTORS).
    pub(crate) has_tlv: bool,
}

impl MachOLayout {
    /// Find the 1-based section number for a symbol at `vaddr`.
    /// Searches both __TEXT and __DATA sections.
    pub(crate) fn section_for_addr(&self, vaddr: u64) -> Option<u8> {
        let mut sect_num = 1u8;
        for sec in &self.text_sections {
            if sec.size > 0 && vaddr >= sec.vmaddr && vaddr < sec.vmaddr + sec.size {
                return Some(sect_num);
            }
            sect_num += 1;
        }
        for sec in &self.data_sections {
            if sec.size > 0 && vaddr >= sec.vmaddr && vaddr < sec.vmaddr + sec.size {
                return Some(sect_num);
            }
            sect_num += 1;
        }
        None
    }
}

/// Compute the Mach-O layout: segment placement, GOT allocation.
pub(crate) fn layout_macho(state: &mut LinkState) -> MachOLayout {
    let arch = state.arch;
    let page_size = match arch { Arch::X86_64 => PAGE_SIZE_X86_64, Arch::Aarch64 => PAGE_SIZE_ARM64 };
    let buckets = classify_sections(state);

    let got_symbols = collect_unique_symbols(state.relocs.iter(), |r| {
        // Standard GOT relocation types
        if matches!(r.r_type,
            RelocType::Aarch64AdrGotPage | RelocType::Aarch64Ld64GotLo12Nc
            | RelocType::Aarch64GotPcrel32
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
                    Some(SymbolDef::Dynamic { .. }) | None
                ),
                _ => false,
            };
            return is_dynamic;
        }
        false
    });

    // Determine which output sections will exist by checking for nonzero input content.
    // These flags are the single source of truth for section counts — used both for
    // sizeofcmds computation and section vector construction.
    let has_const = buckets.rx.iter().any(|&i| state.sections[i].kind == SectionKind::ReadOnly && state.sections[i].size > 0);
    let has_data = buckets.rw.iter().any(|&i| !state.sections[i].kind.is_nobits() && state.sections[i].size > 0);
    let has_bss = buckets.rw.iter().any(|&i| state.sections[i].kind.is_nobits() && state.sections[i].size > 0);
    let has_got = !got_symbols.is_empty();
    let has_thread_vars = buckets.tls.iter().any(|&i| state.sections[i].kind == SectionKind::TlsVariables && state.sections[i].size > 0);
    let has_thread_data = buckets.tls.iter().any(|&i| state.sections[i].kind == SectionKind::Tls && state.sections[i].size > 0);
    let has_thread_bss = buckets.tls.iter().any(|&i| state.sections[i].kind == SectionKind::TlsBss && state.sections[i].size > 0);

    let has_data_segment = has_data || has_got || has_bss
        || has_thread_vars || has_thread_data || has_thread_bss;
    let text_nsects = 1 + has_const as u32;
    let data_nsects = has_data as u32 + has_got as u32 + has_bss as u32
        + has_thread_vars as u32 + has_thread_data as u32 + has_thread_bss as u32;

    let dylib_name = "/usr/lib/libSystem.B.dylib\0";
    let dylib_cmd_size = align_up((size_of::<DylibCmd>() + dylib_name.len()) as u64, 8) as u32;
    let dylinker_name = "/usr/lib/dyld\0";
    let dylinker_cmd_size = align_up((size_of::<LoadDylinkerCmd>() + dylinker_name.len()) as u64, 8) as u32;
    let build_version_size = size_of::<BuildVersionCmd>() as u32 + size_of::<BuildToolVersion>() as u32;

    let sizeofcmds: u32 =
        SEGMENT_CMD_SIZE                                           // __PAGEZERO
        + SEGMENT_CMD_SIZE + SECTION_HEADER_SIZE * text_nsects     // __TEXT
        + if has_data_segment { SEGMENT_CMD_SIZE + SECTION_HEADER_SIZE * data_nsects } else { 0 }
        + SEGMENT_CMD_SIZE                                         // __LINKEDIT
        + dylinker_cmd_size
        + size_of::<EntryPointCmd>() as u32
        + dylib_cmd_size
        + size_of::<DyldInfoCmd>() as u32
        + size_of::<SymtabCmd>() as u32
        + size_of::<DysymtabCmd>() as u32
        + build_version_size
        + size_of::<LinkeditDataCmd>() as u32;
    let ncmds: u32 = 11 + has_data_segment as u32;
    let header_size = MACHO_HEADER_SIZE as u64 + sizeofcmds as u64;

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
    cursor = const_sec_vmaddr;
    for &idx in &buckets.rx {
        let sec = &mut state.sections[idx];
        if sec.kind == SectionKind::ReadOnly {
            cursor = align_up(cursor, sec.align);
            sec.vaddr = Some(cursor);
            cursor += sec.size;
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
                Some(SymbolDef::Dynamic { .. }) => true,
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

    // __thread_vars: TLV descriptors (file-backed)
    let thread_vars_sec_vmaddr = align_up(data_cursor, 8);
    let thread_vars_sec_offset = data_fileoff + (thread_vars_sec_vmaddr - data_vmaddr);
    let mut tv_cursor = thread_vars_sec_vmaddr;
    for &idx in &buckets.tls {
        let sec = &mut state.sections[idx];
        if sec.kind != SectionKind::TlsVariables { continue; }
        tv_cursor = align_up(tv_cursor, sec.align);
        sec.vaddr = Some(tv_cursor);
        tv_cursor += sec.size;
    }
    let thread_vars_sec_size = tv_cursor - thread_vars_sec_vmaddr;
    data_cursor = tv_cursor;

    // __thread_data: initialized TLS data (file-backed)
    let thread_data_sec_vmaddr = align_up(data_cursor, 8);
    let thread_data_sec_offset = data_fileoff + (thread_data_sec_vmaddr - data_vmaddr);
    let mut td_cursor = thread_data_sec_vmaddr;
    for &idx in &buckets.tls {
        let sec = &mut state.sections[idx];
        if sec.kind != SectionKind::Tls { continue; }
        td_cursor = align_up(td_cursor, sec.align);
        sec.vaddr = Some(td_cursor);
        td_cursor += sec.size;
    }
    let thread_data_sec_size = td_cursor - thread_data_sec_vmaddr;
    // Don't advance data_cursor yet — thread_bss must follow thread_data
    // to keep the TLS template contiguous (dyld computes template as
    // [thread_data_start .. thread_bss_end]).

    // __thread_bss: zero-init TLS (nobits) — must be adjacent to __thread_data
    let thread_bss_sec_vmaddr = align_up(td_cursor, 8);
    let mut tb_cursor = thread_bss_sec_vmaddr;
    for &idx in &buckets.tls {
        let sec = &mut state.sections[idx];
        if sec.kind != SectionKind::TlsBss { continue; }
        tb_cursor = align_up(tb_cursor, sec.align);
        sec.vaddr = Some(tb_cursor);
        tb_cursor += sec.size;
    }
    let thread_bss_sec_size = tb_cursor - thread_bss_sec_vmaddr;

    // BSS (regular) — after all TLS sections
    let bss_sec_vmaddr = align_up(tb_cursor, 8);
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

    // Build sorted section lists for __TEXT and __DATA segments
    let mut text_sections = vec![MachOSection {
        sectname: pad16(b"__text"), segname: pad16(b"__TEXT"),
        vmaddr: text_sec_vmaddr, size: text_sec_size,
        offset: text_sec_offset as u32,
        align: match arch { Arch::X86_64 => 4, Arch::Aarch64 => 2 },
        flags: S_REGULAR | S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS, reserved1: 0,
    }];
    if has_const {
        text_sections.push(MachOSection {
            sectname: pad16(b"__const"), segname: pad16(b"__TEXT"),
            vmaddr: const_sec_vmaddr, size: const_sec_size,
            offset: const_sec_offset as u32, align: 0, flags: S_REGULAR, reserved1: 0,
        });
    }
    text_sections.sort_by_key(|s| s.vmaddr);

    let mut data_sections = Vec::new();
    let data_seg = pad16(b"__DATA");
    if has_data {
        data_sections.push(MachOSection {
            sectname: pad16(b"__data"), segname: data_seg,
            vmaddr: data_sec_vmaddr, size: data_sec_size,
            offset: data_sec_offset as u32, align: 3, flags: S_REGULAR, reserved1: 0,
        });
    }
    if has_got {
        data_sections.push(MachOSection {
            sectname: pad16(b"__got"), segname: data_seg,
            vmaddr: got_sec_vmaddr, size: got_sec_size,
            offset: got_sec_offset as u32, align: 3, flags: S_NON_LAZY_SYMBOL_POINTERS, reserved1: 0,
        });
    }
    if has_thread_vars {
        data_sections.push(MachOSection {
            sectname: pad16(b"__thread_vars"), segname: data_seg,
            vmaddr: thread_vars_sec_vmaddr, size: thread_vars_sec_size,
            offset: thread_vars_sec_offset as u32, align: 3, flags: S_THREAD_LOCAL_VARIABLES, reserved1: 0,
        });
    }
    if has_thread_data {
        data_sections.push(MachOSection {
            sectname: pad16(b"__thread_data"), segname: data_seg,
            vmaddr: thread_data_sec_vmaddr, size: thread_data_sec_size,
            offset: thread_data_sec_offset as u32, align: 3, flags: S_THREAD_LOCAL_REGULAR, reserved1: 0,
        });
    }
    if has_bss {
        data_sections.push(MachOSection {
            sectname: pad16(b"__bss"), segname: data_seg,
            vmaddr: bss_sec_vmaddr, size: bss_sec_size,
            offset: 0, align: 3, flags: S_ZEROFILL, reserved1: 0,
        });
    }
    if has_thread_bss {
        data_sections.push(MachOSection {
            sectname: pad16(b"__thread_bss"), segname: data_seg,
            vmaddr: thread_bss_sec_vmaddr, size: thread_bss_sec_size,
            offset: 0, align: 3, flags: S_THREAD_LOCAL_ZEROFILL, reserved1: 0,
        });
    }
    data_sections.sort_by_key(|s| s.vmaddr);

    // File-backed size: everything up to the end of the last non-nobits section,
    // rounded up to page size. macOS dyld hangs on TLS segments with non-page-aligned filesize.
    let data_filesize = data_sections.iter()
        .filter(|s| s.offset > 0)
        .map(|s| (s.vmaddr + s.size) - data_vmaddr)
        .max()
        .map(|sz| align_up(sz, page_size))
        .unwrap_or(0);
    let data_memsz = data_sections.iter()
        .map(|s| (s.vmaddr + s.size) - data_vmaddr)
        .max()
        .unwrap_or(0);
    let data_vmsize = align_up(data_memsz, page_size);

    let linkedit_vmaddr = data_vmaddr + data_vmsize;
    let linkedit_fileoff = data_fileoff + align_up(data_filesize, page_size);

    // Defense-in-depth: verify predicted section counts match actual vectors.
    assert_eq!(text_sections.len() as u32, text_nsects,
        "text section count: predicted {text_nsects}, actual {}", text_sections.len());
    assert_eq!(data_sections.len() as u32, data_nsects,
        "data section count: predicted {data_nsects}, actual {}", data_sections.len());

    MachOLayout {
        text_vmaddr,
        text_vmsize,
        text_filesize,
        text_sections,
        data_vmaddr,
        data_vmsize,
        data_fileoff,
        data_filesize,
        data_sections,
        linkedit_vmaddr,
        linkedit_fileoff,
        got,
        got_entries,
        sizeofcmds,
        ncmds,
        arch,
        page_size,
        tls_template_start: thread_data_sec_vmaddr,
        has_tlv: has_thread_vars,
    }
}

// ── Emission ────────────────────────────────────────────────────────────

pub(crate) fn emit_macho_bytes(
    state: &LinkState,
    layout: &MachOLayout,
    entry_name: &str,
    rebase_entries: &[u64],
    bind_entries: &[(String, u64)],
) -> Result<Vec<u8>, LinkError> {
    let entry_addr = state
        .globals
        .get(entry_name)
        .map(|def| match def {
            SymbolDef::Defined { section, value, .. } => {
                let sec = &state.sections[*section];
                sec.vaddr.unwrap_or_else(|| panic!(
                    "entry point in section {:?} ({:?}) has no vaddr",
                    sec.name, sec.kind,
                )) + value
            }
            SymbolDef::Dynamic { .. } => panic!("entry point cannot be a dynamic symbol"),
        })
        .ok_or_else(|| LinkError::MissingEntry(entry_name.to_string()))?;

    let entryoff = entry_addr - layout.text_vmaddr;

    // Build __LINKEDIT content first to know sizes
    let rebase_data = build_rebase_opcodes(layout, rebase_entries);
    let bind_data = build_bind_opcodes(layout, bind_entries);
    let export_data = build_export_trie(entry_name, entryoff);

    let (symtab_data, strtab_data, nlocalsym, nextdefsym, nundefsym, undef_syms) =
        build_symbol_table(state, layout, bind_entries);

    // Build indirect symbol table: one u32 entry per GOT slot
    const INDIRECT_SYMBOL_LOCAL: u32 = 0x8000_0000;
    let undef_base = nlocalsym + nextdefsym;
    let mut indirect_symtab = Vec::new();
    for (sym_ref, is_external) in &layout.got_entries {
        if *is_external {
            let name = sym_ref.name();
            let idx = undef_syms.iter().position(|s| s == name)
                .unwrap_or_else(|| panic!("GOT symbol {name:?} not in undef_syms"));
            indirect_symtab.extend_from_slice(&(undef_base + idx as u32).to_le_bytes());
        } else {
            indirect_symtab.extend_from_slice(&INDIRECT_SYMBOL_LOCAL.to_le_bytes());
        }
    }

    // __LINKEDIT layout
    let mut linkedit_cursor = layout.linkedit_fileoff;

    let rebase_size = rebase_data.len() as u32;
    let rebase_off = if rebase_size > 0 { linkedit_cursor } else { 0 };
    linkedit_cursor += align_up(rebase_size as u64, 8);

    let bind_size = bind_data.len() as u32;
    let bind_off = if bind_size > 0 { linkedit_cursor } else { 0 };
    linkedit_cursor += align_up(bind_size as u64, 8);

    let export_size = export_data.len() as u32;
    let export_off = if export_size > 0 { linkedit_cursor } else { 0 };
    linkedit_cursor += align_up(export_size as u64, 8);

    let symtab_off = linkedit_cursor;
    let nsyms = (symtab_data.len() / size_of::<Nlist64>()) as u32;
    linkedit_cursor += symtab_data.len() as u64;
    linkedit_cursor = align_up(linkedit_cursor, 8);

    let strtab_off = linkedit_cursor;
    let strtab_size = strtab_data.len() as u32;
    linkedit_cursor += align_up(strtab_size as u64, 8);

    let nindirectsyms = (indirect_symtab.len() / 4) as u32;
    let indirect_symtab_off = if nindirectsyms > 0 { linkedit_cursor } else { 0 };
    linkedit_cursor += align_up(indirect_symtab.len() as u64, 8);

    // Code signature (16-byte aligned, must be last in __LINKEDIT)
    linkedit_cursor = align_up(linkedit_cursor, 16);
    let codesig_off = linkedit_cursor;
    let code_limit = codesig_off as u32;
    let cs_block_size: u32 = 4096;
    let n_code_slots = (code_limit + cs_block_size - 1) / cs_block_size;
    let ident = b"_main\0";
    let all_headers = (size_of::<CsCodeDirectory>() + ident.len() + 15) & !15;
    let hash_offset = all_headers as u32;
    let cd_size = hash_offset + n_code_slots * 32;
    // SuperBlob(12) + BlobIndex(8) + CodeDirectory
    let codesig_size = 12 + 8 + cd_size;
    linkedit_cursor += codesig_size as u64;

    let linkedit_filesize = linkedit_cursor - layout.linkedit_fileoff;
    let linkedit_vmsize = align_up(linkedit_filesize, layout.page_size);

    // Build the output buffer
    let total_size = linkedit_cursor as usize;
    let mut buf = vec![0u8; total_size];

    // ── Mach-O header ──
    let mut off = write_struct(&mut buf, 0, &MachHeader64 {
        magic: U32Le::new(MH_MAGIC_64),
        cputype: U32Le::new(match layout.arch { Arch::X86_64 => CPU_TYPE_X86_64, Arch::Aarch64 => CPU_TYPE_ARM64 }),
        cpusubtype: U32Le::new(match layout.arch { Arch::X86_64 => CPU_SUBTYPE_X86_64_ALL, Arch::Aarch64 => CPU_SUBTYPE_ARM64_ALL }),
        filetype: U32Le::new(MH_EXECUTE),
        ncmds: U32Le::new(layout.ncmds),
        sizeofcmds: U32Le::new(layout.sizeofcmds),
        flags: U32Le::new(MH_PIE | MH_TWOLEVEL | MH_DYLDLINK
            | if layout.has_tlv { MH_HAS_TLV_DESCRIPTORS } else { 0 }),
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
    off = write_struct(&mut buf, off, &SegmentCommand64 {
        cmd: U32Le::new(LC_SEGMENT_64),
        cmdsize: U32Le::new(SEGMENT_CMD_SIZE + SECTION_HEADER_SIZE * layout.text_sections.len() as u32),
        segname: pad16(b"__TEXT"),
        vmaddr: U64Le::new(layout.text_vmaddr),
        vmsize: U64Le::new(layout.text_vmsize),
        fileoff: U64Le::new(0),
        filesize: U64Le::new(layout.text_filesize),
        maxprot: U32Le::new(VM_PROT_READ_EXECUTE),
        initprot: U32Le::new(VM_PROT_READ_EXECUTE),
        nsects: U32Le::new(layout.text_sections.len() as u32),
        flags: U32Le::new(0),
    });
    for sec in &layout.text_sections {
        off = write_struct(&mut buf, off, &Section64 {
            sectname: sec.sectname, segname: sec.segname,
            addr: U64Le::new(sec.vmaddr), size: U64Le::new(sec.size),
            offset: U32Le::new(sec.offset), align: U32Le::new(sec.align),
            reloff: U32Le::new(0), nreloc: U32Le::new(0),
            flags: U32Le::new(sec.flags),
            reserved1: U32Le::new(sec.reserved1), reserved2: U32Le::new(0), reserved3: U32Le::new(0),
        });
    }

    // LC_SEGMENT_64 __DATA
    if !layout.data_sections.is_empty() {
        off = write_struct(&mut buf, off, &SegmentCommand64 {
            cmd: U32Le::new(LC_SEGMENT_64),
            cmdsize: U32Le::new(SEGMENT_CMD_SIZE + SECTION_HEADER_SIZE * layout.data_sections.len() as u32),
            segname: pad16(b"__DATA"),
            vmaddr: U64Le::new(layout.data_vmaddr),
            vmsize: U64Le::new(layout.data_vmsize),
            fileoff: U64Le::new(layout.data_fileoff),
            filesize: U64Le::new(layout.data_filesize),
            maxprot: U32Le::new(VM_PROT_READ_WRITE),
            initprot: U32Le::new(VM_PROT_READ_WRITE),
            nsects: U32Le::new(layout.data_sections.len() as u32),
            flags: U32Le::new(0),
        });
        for sec in &layout.data_sections {
            off = write_struct(&mut buf, off, &Section64 {
                sectname: sec.sectname, segname: sec.segname,
                addr: U64Le::new(sec.vmaddr), size: U64Le::new(sec.size),
                offset: U32Le::new(sec.offset), align: U32Le::new(sec.align),
                reloff: U32Le::new(0), nreloc: U32Le::new(0),
                flags: U32Le::new(sec.flags),
                reserved1: U32Le::new(sec.reserved1), reserved2: U32Le::new(0), reserved3: U32Le::new(0),
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
        maxprot: U32Le::new(VM_PROT_READ),
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
    dysymtab.indirectsymoff = U32Le::new(indirect_symtab_off as u32);
    dysymtab.nindirectsyms = U32Le::new(nindirectsyms);
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
    // Every input section with a vaddr gets written at its file offset.
    // __TEXT sections: file_offset = vaddr - text_vmaddr
    // __DATA sections: file_offset = data_fileoff + (vaddr - data_vmaddr)
    for sec in &state.sections {
        let Some(vaddr) = sec.vaddr else { continue };
        if sec.data.is_empty() || sec.kind.is_nobits() { continue }
        let file_off = if vaddr >= layout.data_vmaddr {
            (layout.data_fileoff + (vaddr - layout.data_vmaddr)) as usize
        } else {
            (vaddr - layout.text_vmaddr) as usize
        };
        buf[file_off..file_off + sec.data.len()].copy_from_slice(&sec.data);
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
    write_at(&mut buf, indirect_symtab_off as usize, &indirect_symtab);

    // Build and write ad-hoc code signature (matching lld's minimal approach:
    // single CodeDirectory blob, no special slots, CS_ADHOC | CS_LINKER_SIGNED)
    build_code_signature(&mut buf, codesig_off as usize, code_limit, n_code_slots,
        ident, hash_offset, cd_size, codesig_size, layout);

    Ok(buf)
}

// ── Rebase opcodes ──────────────────────────────────────────────────────

/// Mach-O C symbol names have a leading `_` prefix. ELF names don't.
/// Prepend `_` unless the name already starts with `_` (from Mach-O input).
fn macho_mangle(name: &str) -> String {
    format!("_{name}")
}

fn build_rebase_opcodes(layout: &MachOLayout, entries: &[u64]) -> Vec<u8> {
    let mut ops = Vec::new();
    if entries.is_empty() {
        return ops;
    }

    ops.push(REBASE_OPCODE_SET_TYPE_IMM | REBASE_TYPE_POINTER);

    // Group rebase entries by segment
    let mut data_entries: Vec<u64> = entries.iter()
        .filter(|&&vaddr| vaddr >= layout.data_vmaddr && vaddr < layout.data_vmaddr + layout.data_vmsize)
        .copied()
        .collect();
    data_entries.sort();

    // __DATA segment ordinal: __PAGEZERO=0, __TEXT=1, __DATA=2.
    // All absolute reloc sections are moved to __DATA by mark_abs_reloc_sections_writable,
    // so rebase entries are always in __DATA.
    let seg_ordinal = 2u8;
    for &vaddr in &data_entries {
        let offset = vaddr - layout.data_vmaddr;
        ops.push(REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB | seg_ordinal);
        encode_uleb128(&mut ops, offset);
        ops.push(REBASE_OPCODE_DO_REBASE_IMM_TIMES | 1);
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

    // Child node info (needed to know terminal_size)
    let mut info = Vec::new();
    info.push(0); // flags: EXPORT_SYMBOL_FLAGS_KIND_REGULAR
    encode_uleb128(&mut info, entry_offset);
    let terminal_size = info.len();

    // Child node offset: iteratively solve offset = pos + uleb_size(offset)
    // since the ULEB encoding of the offset may itself change the offset.
    let pos = trie.len();
    let mut child_node_offset = pos + 1;
    loop {
        let new = pos + uleb_size(child_node_offset as u64);
        if new == child_node_offset { break; }
        child_node_offset = new;
    }
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
) -> (Vec<u8>, Vec<u8>, u32, u32, u32, Vec<String>) {
    let mut symtab = Vec::new();
    let mut strtab = vec![0u8]; // index 0 = empty string

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
        let SymbolDef::Defined { section, value, .. } = def else { continue; };
        let sec = &state.sections[*section];
        let value = sec.vaddr.unwrap_or_else(|| panic!(
            "symbol {name:?} in section {:?} ({:?}) has no vaddr",
            sec.name, sec.kind,
        )) + value;
        let sect = layout.section_for_addr(value).unwrap_or_else(|| panic!(
            "symbol {name:?} at {value:#x} does not fall in any Mach-O section",
        ));
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

    (symtab, strtab, nlocalsym, nextdefsym, nundefsym, undef_syms)
}

// ── Ad-hoc code signature (matching lld's minimal approach) ──────────

const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xFADE_0CC0;
const CSMAGIC_CODEDIRECTORY: u32 = 0xFADE_0C02;
const CS_SUPPORTSEXECSEG: u32 = 0x0002_0400;
const CS_ADHOC: u32 = 0x0000_0002;
const CS_LINKER_SIGNED: u32 = 0x0002_0000;
const CS_HASHTYPE_SHA256: u8 = 2;
const CS_EXECSEG_MAIN_BINARY: u64 = 1;

#[allow(clippy::too_many_arguments)]
fn build_code_signature(
    buf: &mut [u8],
    codesig_off: usize,
    code_limit: u32,
    n_code_slots: u32,
    ident: &[u8],
    hash_offset: u32,
    cd_size: u32,
    codesig_size: u32,
    layout: &MachOLayout,
) {
    let blob_headers_size: u32 = 12 + 8; // SuperBlob + 1 BlobIndex

    // SuperBlob header
    let mut off = codesig_off;
    write_be32(buf, off, CSMAGIC_EMBEDDED_SIGNATURE);
    write_be32(buf, off + 4, codesig_size);
    write_be32(buf, off + 8, 1); // count = 1 blob

    // BlobIndex[0]: CodeDirectory
    write_be32(buf, off + 12, 0); // CSSLOT_CODEDIRECTORY
    write_be32(buf, off + 16, blob_headers_size);

    // CodeDirectory
    let cd_off = codesig_off + blob_headers_size as usize;
    off = write_struct(buf, cd_off, &CsCodeDirectory {
        magic: U32Be::new(CSMAGIC_CODEDIRECTORY),
        length: U32Be::new(cd_size),
        version: U32Be::new(CS_SUPPORTSEXECSEG),
        flags: U32Be::new(CS_ADHOC | CS_LINKER_SIGNED),
        hash_offset: U32Be::new(hash_offset),
        ident_offset: U32Be::new(size_of::<CsCodeDirectory>() as u32),
        n_special_slots: U32Be::new(0),
        n_code_slots: U32Be::new(n_code_slots),
        code_limit: U32Be::new(code_limit),
        hash_size: 32,
        hash_type: CS_HASHTYPE_SHA256,
        platform: 0,
        page_size: 12, // log2(4096)
        spare2: U32Be::new(0),
        scatter_offset: U32Be::new(0),
        team_offset: U32Be::new(0),
        spare3: U32Be::new(0),
        code_limit_64: U64Be::new(0),
        exec_seg_base: U64Be::new(0),
        exec_seg_limit: U64Be::new(layout.text_filesize),
        exec_seg_flags: U64Be::new(CS_EXECSEG_MAIN_BINARY),
    });

    // Identifier string + padding
    buf[off..off + ident.len()].copy_from_slice(ident);
    // padding bytes are already zero

    // Code page hashes (SHA-256 of each 4K block)
    let hash_start = cd_off + hash_offset as usize;
    for i in 0..n_code_slots {
        let page_start = (i * 4096) as usize;
        let page_end = ((i + 1) * 4096).min(code_limit) as usize;
        let hash = Sha256::digest(&buf[page_start..page_end]);
        let off = hash_start + (i as usize) * 32;
        buf[off..off + 32].copy_from_slice(&hash);
    }
}

fn write_be32(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_be_bytes());
}



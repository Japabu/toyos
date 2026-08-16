//! ELF64 decoding, for the questions ToyOS asks a program image: **what does
//! this file want mapped, where does everything it names live inside it, and
//! which of the numbers in it may the kernel act on?**
//!
//! An ELF is untrusted input — `spawn` opens whatever path a process names and
//! `dlopen` opens whatever path a process names — so the kernel's fail-fast
//! rule does not apply to anything decoded here. Every length, count, offset
//! and vaddr is checked before it is used, nothing is indexed without a bound,
//! and no path panics. What cannot be satisfied is refused by name through
//! [`Error`]; nothing is silently truncated to fit.
//!
//! `no_std`, no allocation, no `unsafe`. The bounded pieces are bounded by
//! constants in this crate ([`MAX_LOAD_SEGMENTS`], [`MAX_TLS_ALIGN`]); the
//! unbounded ones — a relocation table, a symbol table, a section header table
//! — are *views* over bytes the caller already holds, so nothing here is sized
//! by a number the file chose.
//!
//! # What this crate is not
//!
//! It maps nothing, allocates nothing and writes nothing. Turning a
//! [`Layout`] into an address space, applying a relocation and copying a TLS
//! template are the kernel's, and the split is the point: everything in here
//! is a pure function of bytes and is tested on the host in milliseconds.
//!
//! # Scope
//!
//! ELF64, little-endian, `ET_DYN`, `EM_X86_64`. Everything else is refused by
//! name rather than tolerated: ToyOS emits PIE binaries only, has no 32-bit
//! mode and no big-endian target. A second architecture adds a machine to
//! [`header::Machine`] and a relocation set to [`rela`], not a class or an
//! endianness.

#![no_std]
#![forbid(unsafe_code)]

pub mod dynamic;
pub mod gnu_hash;
pub mod header;
pub mod layout;
pub mod rela;
pub mod section;
pub mod sym;
pub mod tls;

pub use dynamic::{Dynamic, Table};
pub use gnu_hash::GnuHash;
pub use header::FileHeader;
pub use layout::{Layout, Segment, SegmentFlags, SectionTableRef, TlsSegment};
pub use rela::{Rela, RelaCounts, RelaTable, RelocError, RelocKind};
pub use section::{SectionHeader, SectionTable};
pub use sym::{Sym, SymTab};

/// The most `PT_LOAD` segments an image may declare.
///
/// A policy bound, and the answer to it is [`Error::TooManyLoadSegments`] —
/// the file is refused, never trimmed to fit. Every linker in use writes three
/// (text, data, rodata) and no toolchain in the wild exceeds six; the number is
/// deliberately far above that because the cost of raising it is one array and
/// the cost of a wrong refusal is a program that will not start.
///
/// It exists so that a [`Layout`] needs no allocator: `e_phnum` is a `u16` a
/// file chooses, and a `Vec` sized from it is a heap allocation an attacker
/// picks the size of.
pub const MAX_LOAD_SEGMENTS: usize = 16;

/// The largest `PT_TLS` `p_align` a file may declare.
///
/// `p_align` reaches the TLS block's size computation as an addend and its
/// placement as the mask `!(align - 1)`. Neither survives an arbitrary `u64`:
/// the addition overflows, and a non-power-of-two turns the mask into noise
/// that can place TLS data on top of the DTV. The ceiling is the largest page
/// the kernel maps, since a stricter alignment than one page cannot be honoured
/// by an allocator that hands out pages.
pub const MAX_TLS_ALIGN: u64 = 2 * 1024 * 1024;

/// Why a file was refused.
///
/// Every variant names a specific property of the bytes. Callers log the
/// message and answer their own caller with a refusal; none of them is a
/// condition the kernel can recover into a partly-built process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Fewer bytes than an ELF64 file header.
    TooSmall,
    /// `e_ident` does not begin `\x7fELF`.
    BadMagic,
    /// Not `ELFCLASS64`.
    NotElf64,
    /// Not `ELFDATA2LSB`.
    NotLittleEndian,
    /// `e_ident[EI_VERSION]` is not `EV_CURRENT`.
    BadVersion,
    /// Not `ET_DYN`. ToyOS loads position-independent executables only.
    NotPie,
    /// Not `EM_X86_64`.
    WrongMachine,
    /// `e_phnum` is zero: nothing to map.
    NoProgramHeaders,
    /// `e_phentsize` is not 56, the only size an ELF64 program header has.
    BadProgramHeaderSize,
    /// `e_phoff` is less than [`header::FILE_HEADER_SIZE`]: the program
    /// header table would begin inside the file header itself, which no
    /// linker emits.
    ProgramHeadersInsideFileHeader,
    /// The program header table lies outside the buffer that was read, or
    /// `e_phoff` plus its length overflows.
    ProgramHeadersOutsideBuffer,
    /// More `PT_LOAD` segments than [`MAX_LOAD_SEGMENTS`].
    TooManyLoadSegments,
    /// No `PT_LOAD` segment at all.
    NoLoadSegments,
    /// `p_filesz > p_memsz`. Downstream these are a (copy length, destination
    /// size) pair, so this is a memory-safety property and not an ELF
    /// formality.
    FileszAboveMemsz,
    /// `p_vaddr + p_memsz` does not fit a `u64`.
    SegmentExtentOverflows,
    /// `p_offset + p_filesz` does not fit a `u64`.
    FileExtentOverflows,
    /// `e_entry` is outside `[vaddr_min, vaddr_max)`, so the process would
    /// start executing at an address no segment covers.
    EntryOutsideImage,
    /// The file-backed part of `PT_TLS` is outside the loadable segments.
    TlsOutsideImage,
    /// `PT_DYNAMIC` is outside the loadable segments.
    DynamicOutsideImage,
    /// `PT_GNU_EH_FRAME` is outside the loadable segments.
    EhFrameOutsideImage,
    /// `PT_TLS` `p_align` is neither zero nor a power of two no larger than
    /// [`MAX_TLS_ALIGN`].
    BadTlsAlign,
}

impl Error {
    pub const fn as_str(self) -> &'static str {
        match self {
            Error::TooSmall => "ELF: fewer bytes than a file header",
            Error::BadMagic => "ELF: bad magic",
            Error::NotElf64 => "ELF: not ELFCLASS64",
            Error::NotLittleEndian => "ELF: not ELFDATA2LSB",
            Error::BadVersion => "ELF: e_ident version is not EV_CURRENT",
            Error::NotPie => "ELF: not PIE (expected ET_DYN)",
            Error::WrongMachine => "ELF: not x86_64",
            Error::NoProgramHeaders => "ELF: no program headers",
            Error::BadProgramHeaderSize => "ELF: e_phentsize is not 56",
            Error::ProgramHeadersInsideFileHeader => "ELF: e_phoff points inside the file header",
            Error::ProgramHeadersOutsideBuffer => "ELF: program headers outside the header buffer",
            Error::TooManyLoadSegments => "ELF: more PT_LOAD segments than the loader will map",
            Error::NoLoadSegments => "ELF: no loadable segments",
            Error::FileszAboveMemsz => "ELF: p_filesz > p_memsz",
            Error::SegmentExtentOverflows => "ELF: p_vaddr + p_memsz overflows",
            Error::FileExtentOverflows => "ELF: p_offset + p_filesz overflows",
            Error::EntryOutsideImage => "ELF: e_entry outside the loadable segments",
            Error::TlsOutsideImage => "ELF: PT_TLS file image outside the loadable segments",
            Error::DynamicOutsideImage => "ELF: PT_DYNAMIC outside the loadable segments",
            Error::EhFrameOutsideImage => "ELF: PT_GNU_EH_FRAME outside the loadable segments",
            Error::BadTlsAlign => "ELF: PT_TLS p_align is not a power of two within a page",
        }
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A NUL-terminated name out of a string table, or `""` when the offset names
/// nothing readable.
///
/// The offset is a `u64` because every one of them — `st_name`, a `DT_NEEDED`
/// value — came out of a file. Past the end of the table there is no name, and
/// no name is the right answer: a name is only ever compared against another
/// name, so an unreadable one matches nothing.
pub fn cstr(strings: &[u8], offset: u64) -> &str {
    match usize::try_from(offset) {
        Ok(off) => read::cstr(strings, off),
        Err(_) => "",
    }
}

/// Little-endian scalar reads that answer `None` rather than panicking when the
/// field is not wholly inside the buffer.
///
/// Every offset passed to these came out of a file, so "past the end" is an
/// ordinary answer about the input and not a bug in the caller.
pub(crate) mod read {
    pub fn u16_at(data: &[u8], off: usize) -> Option<u16> {
        Some(u16::from_le_bytes(bytes::<2>(data, off)?))
    }

    pub fn u32_at(data: &[u8], off: usize) -> Option<u32> {
        Some(u32::from_le_bytes(bytes::<4>(data, off)?))
    }

    pub fn u64_at(data: &[u8], off: usize) -> Option<u64> {
        Some(u64::from_le_bytes(bytes::<8>(data, off)?))
    }

    pub fn i64_at(data: &[u8], off: usize) -> Option<i64> {
        Some(i64::from_le_bytes(bytes::<8>(data, off)?))
    }

    fn bytes<const N: usize>(data: &[u8], off: usize) -> Option<[u8; N]> {
        let end = off.checked_add(N)?;
        data.get(off..end)?.try_into().ok()
    }

    /// The NUL-terminated string starting at `off`, or `""` for an offset past
    /// the end or bytes that are not UTF-8.
    ///
    /// A run with no NUL before the table ends is that table's last name,
    /// truncated by its own declared length — the string table bounds it, so
    /// there is nothing to refuse. A name is only ever compared against another
    /// name, so an unreadable one matching nothing is the right answer.
    pub fn cstr(data: &[u8], off: usize) -> &str {
        let Some(rest) = data.get(off..) else { return "" };
        let len = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        core::str::from_utf8(&rest[..len]).unwrap_or("")
    }
}

//! The ELF64 file header, decoded field by field from bytes.
//!
//! `e_phentsize` is honoured rather than assumed. Deriving the stride from the
//! class instead lets a file declare 0 and be read at 56 — the header says one
//! thing and the loader does another, which is the class of defect the whole
//! crate exists to remove.

use crate::read;
use crate::Error;

/// Bytes in `e_ident`.
pub const EI_NIDENT: usize = 16;
/// Bytes in an ELF64 file header, `e_ident` included.
pub const FILE_HEADER_SIZE: usize = 64;
/// Bytes in an ELF64 program header. The only value `e_phentsize` may hold.
pub const PROGRAM_HEADER_SIZE: usize = 56;
/// Bytes in an ELF64 section header. The only value `e_shentsize` may hold.
pub const SECTION_HEADER_SIZE: usize = 64;

const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;

/// The instruction set an image is built for.
///
/// One variant, because one architecture boots. ARM64 adds a variant here and
/// a relocation set to [`crate::rela`]; nothing else in this crate is
/// per-architecture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Machine {
    X86_64,
}

#[derive(Clone, Copy, Debug)]
pub struct FileHeader {
    pub machine: Machine,
    pub entry: u64,
    pub phoff: u64,
    pub phnum: u16,
    pub shoff: u64,
    pub shnum: u16,
    pub shentsize: u16,
}

impl FileHeader {
    /// Decode the header out of the first bytes of a file.
    ///
    /// `data` may be a prefix — the loader reads 4 KiB and nothing here needs
    /// more than 64 bytes.
    pub fn parse(data: &[u8]) -> Result<FileHeader, Error> {
        if data.len() < FILE_HEADER_SIZE {
            return Err(Error::TooSmall);
        }
        if data[..4] != [0x7f, b'E', b'L', b'F'] {
            return Err(Error::BadMagic);
        }
        if data[4] != ELFCLASS64 {
            return Err(Error::NotElf64);
        }
        if data[5] != ELFDATA2LSB {
            return Err(Error::NotLittleEndian);
        }
        if data[6] != EV_CURRENT {
            return Err(Error::BadVersion);
        }

        // Every read below is inside the 64 bytes the length check established,
        // so the `?` arms are unreachable and cost nothing to state.
        let e_type = read::u16_at(data, 16).ok_or(Error::TooSmall)?;
        if e_type != ET_DYN {
            return Err(Error::NotPie);
        }
        if read::u16_at(data, 18).ok_or(Error::TooSmall)? != EM_X86_64 {
            return Err(Error::WrongMachine);
        }
        let phnum = read::u16_at(data, 56).ok_or(Error::TooSmall)?;
        if phnum == 0 {
            return Err(Error::NoProgramHeaders);
        }
        if read::u16_at(data, 54).ok_or(Error::TooSmall)? as usize != PROGRAM_HEADER_SIZE {
            return Err(Error::BadProgramHeaderSize);
        }
        let phoff = read::u64_at(data, 32).ok_or(Error::TooSmall)?;
        // The header occupies exactly the first `FILE_HEADER_SIZE` bytes, so a
        // table a linker wrote never starts inside them. Left unchecked this
        // is not caught here: `phoff` is small, so `program_headers` finds it
        // inside the buffer, and the "table" it reads back is the file
        // header's own bytes reinterpreted as `ProgramHeader`s. None of their
        // bit patterns lands on a known `p_type`, so the file is refused
        // anyway — as `NoLoadSegments`, true of the resulting (garbage) table
        // but silent about what is actually wrong: an `e_phoff` no linker
        // would write.
        if phoff < FILE_HEADER_SIZE as u64 {
            return Err(Error::ProgramHeadersInsideFileHeader);
        }

        Ok(FileHeader {
            machine: Machine::X86_64,
            entry: read::u64_at(data, 24).ok_or(Error::TooSmall)?,
            phoff,
            phnum,
            shoff: read::u64_at(data, 40).ok_or(Error::TooSmall)?,
            shnum: read::u16_at(data, 60).ok_or(Error::TooSmall)?,
            shentsize: read::u16_at(data, 58).ok_or(Error::TooSmall)?,
        })
    }

    /// The program header table's bytes, or the reason they cannot be read.
    ///
    /// `e_phoff` is a `u64` a file chooses, so the end of the table is computed
    /// with `checked_add` on `usize` and compared against the buffer. Written
    /// as `phoff + phnum * 56` it wraps, and the slice that follows either
    /// panics on an inverted range or reads a table that is not there.
    pub fn program_headers<'a>(&self, data: &'a [u8]) -> Result<&'a [u8], Error> {
        let start = usize::try_from(self.phoff).map_err(|_| Error::ProgramHeadersOutsideBuffer)?;
        let len = (self.phnum as usize)
            .checked_mul(PROGRAM_HEADER_SIZE)
            .ok_or(Error::ProgramHeadersOutsideBuffer)?;
        let end = start.checked_add(len).ok_or(Error::ProgramHeadersOutsideBuffer)?;
        data.get(start..end).ok_or(Error::ProgramHeadersOutsideBuffer)
    }
}

/// One program header, decoded.
#[derive(Clone, Copy, Debug)]
pub struct ProgramHeader {
    pub kind: u32,
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;
pub const PT_TLS: u32 = 7;
pub const PT_GNU_EH_FRAME: u32 = 0x6474_e550;

impl ProgramHeader {
    /// Decode the `i`th entry of a program header table, or `None` past its
    /// end.
    pub fn parse(table: &[u8], i: usize) -> Option<ProgramHeader> {
        let off = i.checked_mul(PROGRAM_HEADER_SIZE)?;
        // Below `table.len()` every `off + k` for a field of this record is
        // inside `usize` — a slice is at most `isize::MAX` bytes — so the field
        // reads need no second overflow check of their own.
        if off >= table.len() {
            return None;
        }
        Some(ProgramHeader {
            kind: read::u32_at(table, off)?,
            flags: read::u32_at(table, off + 4)?,
            offset: read::u64_at(table, off + 8)?,
            vaddr: read::u64_at(table, off + 16)?,
            filesz: read::u64_at(table, off + 32)?,
            memsz: read::u64_at(table, off + 40)?,
            align: read::u64_at(table, off + 48)?,
        })
    }
}

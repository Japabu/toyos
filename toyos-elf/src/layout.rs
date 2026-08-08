//! What an image wants mapped, derived from program headers alone.
//!
//! [`Layout`] is the crate's central value and the only thing downstream of it
//! is effects. Its invariants are established once, in [`Layout::parse`], so
//! that no consumer re-checks them and none of them can be forgotten at a call
//! site.

use crate::header::{
    FileHeader, ProgramHeader, PT_DYNAMIC, PT_GNU_EH_FRAME, PT_LOAD, PT_TLS, SECTION_HEADER_SIZE,
};
use crate::{Error, MAX_LOAD_SEGMENTS, MAX_TLS_ALIGN};

/// `PF_X`, `PF_W`, `PF_R` as the file declared them.
///
/// Kept whole rather than reduced to `writable` at parse time: protection is a
/// three-way property and a loader that only records one bit cannot express
/// W^X.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentFlags(pub u32);

impl SegmentFlags {
    pub const fn executable(self) -> bool {
        self.0 & 1 != 0
    }
    pub const fn writable(self) -> bool {
        self.0 & 2 != 0
    }
    pub const fn readable(self) -> bool {
        self.0 & 4 != 0
    }
}

/// One `PT_LOAD` segment.
///
/// `filesz <= memsz`, `vaddr + memsz` and `file_offset + filesz` both fit a
/// `u64`, and `[vaddr, vaddr + memsz)` is inside the layout's own
/// `[vaddr_min, vaddr_max)`. [`Layout::parse`] is the only constructor.
#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub vaddr: u64,
    pub memsz: u64,
    pub filesz: u64,
    pub file_offset: u64,
    pub flags: SegmentFlags,
}

impl Segment {
    pub const fn writable(&self) -> bool {
        self.flags.writable()
    }

    /// `[vaddr, vaddr + memsz)` rounded out to whole pages, expressed relative
    /// to `origin`.
    ///
    /// Relative because the loader rebases the image: with a page-aligned base,
    /// rounding an image-relative offset and rounding the rebased address give
    /// the same answer, and only the relative form is free of the base's own
    /// arithmetic.
    /// A round-up that would leave the last page is `u64::MAX` instead: the
    /// only consumer is an overlap test, and a range that is too long can
    /// report an overlap that is not there but never miss one that is.
    pub fn page_range(&self, origin: u64, page_size: u64) -> (u64, u64) {
        debug_assert!(page_size.is_power_of_two());
        let mask = page_size - 1;
        let start = self.vaddr - origin;
        let end = start + self.memsz;
        let end_page = match end.checked_add(mask) {
            Some(rounded) => rounded & !mask,
            None => u64::MAX,
        };
        (start & !mask, end_page)
    }
}

/// A `PT_TLS` segment.
///
/// `align` is zero or a power of two no larger than [`MAX_TLS_ALIGN`], and
/// `filesz <= memsz`. Absent TLS is `None`, never a zero `memsz`: a module with
/// a `PT_TLS` of zero size still gets a DTV slot, and the two cases are not the
/// same question.
#[derive(Clone, Copy, Debug)]
pub struct TlsSegment {
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

/// Where the section header table is, when the file has a usable one.
///
/// Section headers are optional metadata — symbol names for backtraces, and
/// the `.rela.dyn` fallback for a file with no `PT_DYNAMIC`. A table the
/// loader cannot index is dropped rather than refused, because refusing would
/// turn "no symbol names" into "this program does not run".
#[derive(Clone, Copy, Debug)]
pub struct SectionTableRef {
    pub file_offset: u64,
    pub count: u16,
    pub entry_size: u16,
}

impl SectionTableRef {
    /// Bytes the whole table occupies. Cannot overflow: both factors are
    /// `u16`.
    pub const fn byte_len(&self) -> usize {
        self.count as usize * self.entry_size as usize
    }
}

/// Everything the program headers say, validated.
///
/// # Invariants
///
/// [`Layout::parse`] is the only constructor and refuses anything that breaks
/// the following, so every `Layout` that exists already satisfies them:
///
/// - one to [`MAX_LOAD_SEGMENTS`] `PT_LOAD` segments, each with
///   `filesz <= memsz` and neither `vaddr + memsz` nor `file_offset + filesz`
///   overflowing;
/// - `vaddr_min` is the smallest `p_vaddr` and `vaddr_max` the largest
///   `p_vaddr + p_memsz` over those segments, so `[vaddr_min, vaddr_max)`
///   covers every one of them and `vaddr_max - vaddr_min` cannot underflow;
/// - `entry`, the file-backed extent of `PT_TLS`, and all of `PT_DYNAMIC` and
///   `PT_GNU_EH_FRAME`, lie inside `[vaddr_min, vaddr_max)`;
/// - `tls.align` is zero or a power of two no larger than [`MAX_TLS_ALIGN`], so
///   that `!(align - 1)` is a mask and the TLS block's size cannot be dominated
///   by a number the file chose.
///
/// Downstream every size pair is a (copy length, destination size) pair — an
/// allocation of `memsz` then a copy of `filesz` — so `filesz <= memsz` is a
/// memory-safety invariant here and not an ELF formality. Likewise the vaddr
/// ranges: a module's image is addressed as `vaddr - vaddr_min`, which is a
/// wrapping subtraction on anything outside them.
#[derive(Clone, Debug)]
pub struct Layout {
    pub entry: u64,
    pub vaddr_min: u64,
    pub vaddr_max: u64,
    segments: [Segment; MAX_LOAD_SEGMENTS],
    segment_count: usize,
    pub tls: Option<TlsSegment>,
    /// `PT_DYNAMIC` as (file offset, vaddr, size).
    pub dynamic: Option<(u64, u64, u64)>,
    pub section_headers: Option<SectionTableRef>,
    /// `PT_GNU_EH_FRAME` as (vaddr, memsz), for DWARF unwinding.
    pub eh_frame_hdr: Option<(u64, u64)>,
}

impl Layout {
    /// Parse program headers out of the first bytes of a file.
    ///
    /// `data` need only reach the end of the program header table; the loader
    /// hands it 4 KiB and never reads a segment's contents to get here.
    pub fn parse(data: &[u8]) -> Result<Layout, Error> {
        let ehdr = FileHeader::parse(data)?;
        let phdrs = ehdr.program_headers(data)?;

        let mut segments = [Segment {
            vaddr: 0,
            memsz: 0,
            filesz: 0,
            file_offset: 0,
            flags: SegmentFlags(0),
        }; MAX_LOAD_SEGMENTS];
        let mut segment_count = 0usize;
        let mut vaddr_min = u64::MAX;
        let mut vaddr_max = 0u64;
        let mut tls = None;
        let mut dynamic = None;
        let mut eh_frame_hdr = None;

        for i in 0..ehdr.phnum as usize {
            let Some(phdr) = ProgramHeader::parse(phdrs, i) else {
                return Err(Error::ProgramHeadersOutsideBuffer);
            };
            if matches!(phdr.kind, PT_LOAD | PT_TLS) && phdr.filesz > phdr.memsz {
                return Err(Error::FileszAboveMemsz);
            }
            match phdr.kind {
                PT_LOAD => {
                    let seg_end = phdr
                        .vaddr
                        .checked_add(phdr.memsz)
                        .ok_or(Error::SegmentExtentOverflows)?;
                    if phdr.offset.checked_add(phdr.filesz).is_none() {
                        return Err(Error::FileExtentOverflows);
                    }
                    if segment_count == MAX_LOAD_SEGMENTS {
                        return Err(Error::TooManyLoadSegments);
                    }
                    vaddr_min = vaddr_min.min(phdr.vaddr);
                    vaddr_max = vaddr_max.max(seg_end);
                    segments[segment_count] = Segment {
                        vaddr: phdr.vaddr,
                        memsz: phdr.memsz,
                        filesz: phdr.filesz,
                        file_offset: phdr.offset,
                        flags: SegmentFlags(phdr.flags),
                    };
                    segment_count += 1;
                }
                // Last one wins, as it does for every other singleton header:
                // a file with two of these is malformed and the loader has no
                // better answer than a consistent one.
                PT_TLS => {
                    tls = Some(TlsSegment {
                        vaddr: phdr.vaddr,
                        filesz: phdr.filesz,
                        memsz: phdr.memsz,
                        align: phdr.align,
                    })
                }
                PT_DYNAMIC => dynamic = Some((phdr.offset, phdr.vaddr, phdr.filesz)),
                PT_GNU_EH_FRAME => eh_frame_hdr = Some((phdr.vaddr, phdr.memsz)),
                _ => {}
            }
        }

        if segment_count == 0 {
            return Err(Error::NoLoadSegments);
        }

        let layout = Layout {
            entry: ehdr.entry,
            vaddr_min,
            vaddr_max,
            segments,
            segment_count,
            tls,
            dynamic,
            section_headers: section_table(&ehdr),
            eh_frame_hdr,
        };

        // Every other program header names a vaddr the loader turns into an
        // offset into the image as `vaddr - vaddr_min`. Outside
        // `[vaddr_min, vaddr_max)` that is a wrapping subtraction into an
        // out-of-bounds pointer, so bound them here rather than at each use
        // site. Only the file-backed part of `PT_TLS` is checked: `.tbss`
        // occupies address space the containing `PT_LOAD` need not cover, and
        // it is never read from, only zeroed in a buffer of its own.
        if !layout.contains(layout.entry, 1) {
            return Err(Error::EntryOutsideImage);
        }
        if let Some(tls) = layout.tls {
            if !layout.contains(tls.vaddr, tls.filesz) {
                return Err(Error::TlsOutsideImage);
            }
            // `p_align` reaches the TLS block as both an addend to its size and
            // the mask `!(align - 1)`. Neither survives an arbitrary u64: the
            // addition overflows, and a non-power-of-two turns the mask into
            // noise that can place the TLS data on top of the DTV. Zero and one
            // mean "no alignment constraint".
            if tls.align > MAX_TLS_ALIGN || !(tls.align == 0 || tls.align.is_power_of_two()) {
                return Err(Error::BadTlsAlign);
            }
        }
        if let Some((_, vaddr, size)) = layout.dynamic {
            if !layout.contains(vaddr, size) {
                return Err(Error::DynamicOutsideImage);
            }
        }
        if let Some((vaddr, size)) = layout.eh_frame_hdr {
            if !layout.contains(vaddr, size) {
                return Err(Error::EhFrameOutsideImage);
            }
        }

        Ok(layout)
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments[..self.segment_count]
    }

    /// Bytes between the lowest and highest address any segment claims.
    pub const fn span(&self) -> u64 {
        self.vaddr_max - self.vaddr_min
    }

    /// Whether `[vaddr, vaddr + size)` is inside the loadable image.
    pub fn contains(&self, vaddr: u64, size: u64) -> bool {
        vaddr >= self.vaddr_min
            && vaddr.checked_add(size).is_some_and(|end| end <= self.vaddr_max)
    }

    /// The writable window `[lo, hi)` in image-relative coordinates, or `None`
    /// when no segment is writable.
    ///
    /// This is the extent a relocation may write into once the module's
    /// read-only pages are shared between processes: past it the write would
    /// land in another process's copy.
    pub fn writable_window(&self) -> Option<(u64, u64)> {
        let mut window: Option<(u64, u64)> = None;
        for seg in self.segments() {
            if !seg.writable() {
                continue;
            }
            let lo = seg.vaddr - self.vaddr_min;
            let hi = lo + seg.memsz;
            window = Some(match window {
                Some((w_lo, w_hi)) => (w_lo.min(lo), w_hi.max(hi)),
                None => (lo, hi),
            });
        }
        window
    }

    /// The first pair of `PT_LOAD` segments whose page-rounded ranges overlap.
    ///
    /// Each segment becomes a demand-paged region, and a region map holds one
    /// region per address: two segments that merely *share* a page are two
    /// regions at one address. The loader asks before it inserts anything, so a
    /// refusal leaves the address space as it found it.
    pub fn overlapping_load_pages(&self, page_size: u64) -> Option<(usize, usize)> {
        let segs = self.segments();
        for (i, a) in segs.iter().enumerate() {
            let (a_start, a_end) = a.page_range(self.vaddr_min, page_size);
            for (j, b) in segs.iter().enumerate().skip(i + 1) {
                let (b_start, b_end) = b.page_range(self.vaddr_min, page_size);
                if a_start < b_end && b_start < a_end {
                    return Some((i, j));
                }
            }
        }
        None
    }

    /// The file offset a virtual address maps to.
    ///
    /// Falls back to extrapolating from the nearest segment at or below
    /// `vaddr`, which is what `.rela.dyn` and friends need when the linker
    /// places them past a segment's file-backed extent.
    ///
    /// `None` when there is no segment at or below `vaddr` to extrapolate
    /// from, or when the extrapolation overflows. Every `vaddr` asked here is a
    /// `DT_*` tag or a program-header field, so the answer to "this address is
    /// in no segment" is that the binary is malformed, not that the kernel
    /// dies.
    pub fn vaddr_to_file_offset(&self, vaddr: u64) -> Option<u64> {
        for seg in self.segments() {
            if vaddr >= seg.vaddr && vaddr - seg.vaddr < seg.filesz {
                return seg.file_offset.checked_add(vaddr - seg.vaddr);
            }
        }
        let mut best: Option<&Segment> = None;
        for seg in self.segments() {
            if seg.vaddr <= vaddr && best.is_none_or(|b| seg.vaddr > b.vaddr) {
                best = Some(seg);
            }
        }
        let seg = best?;
        seg.file_offset.checked_add(vaddr - seg.vaddr)
    }

    /// How many bytes of file back `vaddr` before the segment holding it runs
    /// out.
    ///
    /// `.gnu.hash` declares no length anywhere — its extent is the section it
    /// lives in and no `DT_*` tag names one — so the honest bound is the
    /// containing segment's own file image. `None` when no segment's file
    /// image covers `vaddr`.
    pub fn file_bytes_from(&self, vaddr: u64) -> Option<u64> {
        for seg in self.segments() {
            if vaddr >= seg.vaddr && vaddr - seg.vaddr < seg.filesz {
                return Some(seg.filesz - (vaddr - seg.vaddr));
            }
        }
        None
    }
}

/// A section header table the loader can index, or `None`.
///
/// `e_shentsize` must be exactly 64: consumers divide a byte count by it and
/// read 64-byte fields out of each entry, so a smaller stride is a short read
/// and a larger one is a table this crate does not know the shape of.
fn section_table(ehdr: &FileHeader) -> Option<SectionTableRef> {
    if ehdr.shoff == 0 || ehdr.shnum == 0 || ehdr.shentsize as usize != SECTION_HEADER_SIZE {
        return None;
    }
    Some(SectionTableRef {
        file_offset: ehdr.shoff,
        count: ehdr.shnum,
        entry_size: ehdr.shentsize,
    })
}

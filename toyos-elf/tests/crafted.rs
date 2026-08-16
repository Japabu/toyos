//! Crafted-input corpus: an ELF that no linker would emit must be refused by
//! name, and nothing here may panic.
//!
//! Every case is a file a process can write and hand to `spawn` or `dlopen`.
//! The kernel's fail-fast rule does not reach any of them — a panic here is a
//! userland-triggered kernel panic, which is exactly what several of these
//! used to be.

mod common;

use common::*;
use toyos_elf::header::PROGRAM_HEADER_SIZE;
use toyos_elf::{Error, Layout, MAX_LOAD_SEGMENTS};

fn refused(bytes: Vec<u8>) -> Error {
    match Layout::parse(&bytes) {
        Ok(_) => panic!("accepted a file that must be refused"),
        Err(e) => e,
    }
}

fn accepted(bytes: Vec<u8>) -> Layout {
    Layout::parse(&bytes).expect("refused a file that must be accepted")
}

// ── The bytes before the program headers ────────────────────────────────

#[test]
fn an_empty_file_is_too_small() {
    assert_eq!(Layout::parse(&[]).unwrap_err(), Error::TooSmall);
}

#[test]
fn a_header_one_byte_short_is_too_small() {
    let bytes = Elf::honest(0x1000).build();
    assert_eq!(Layout::parse(&bytes[..63]).unwrap_err(), Error::TooSmall);
}

#[test]
fn the_magic_is_checked() {
    let mut bytes = Elf::honest(0x1000).build();
    bytes[1] = b'e';
    assert_eq!(Layout::parse(&bytes).unwrap_err(), Error::BadMagic);
}

#[test]
fn class_endianness_version_type_and_machine_are_each_refused_by_name() {
    assert_eq!(refused(Elf::honest(0x1000).class(1).build()), Error::NotElf64);
    assert_eq!(refused(Elf::honest(0x1000).endian(2).build()), Error::NotLittleEndian);
    assert_eq!(refused(Elf::honest(0x1000).version(0).build()), Error::BadVersion);
    assert_eq!(refused(Elf::honest(0x1000).kind(ET_EXEC).build()), Error::NotPie);
    assert_eq!(refused(Elf::honest(0x1000).machine(EM_AARCH64).build()), Error::WrongMachine);
}

#[test]
fn a_program_header_size_that_is_not_56_is_refused() {
    // Not merely tolerated: deriving the stride from the class instead lets a
    // file declare 0 and be read at 56.
    for size in [0u16, 32, 55, 57, 64, u16::MAX] {
        assert_eq!(
            refused(Elf::honest(0x1000).phentsize(size).build()),
            Error::BadProgramHeaderSize,
            "e_phentsize {size}",
        );
    }
}

#[test]
fn no_program_headers_is_refused() {
    assert_eq!(refused(Elf::new(0x1000).phnum(0).build()), Error::NoProgramHeaders);
}

/// A real file's program header table starts at or after `e_phoff ==
/// FILE_HEADER_SIZE` (64): the header occupies exactly the bytes before it.
/// An `e_phoff` inside that range is not a buffer-bounds question — every
/// case here is well inside a one-page file — so it must be refused by its
/// own name rather than fall through to `program_headers` reading header
/// bytes back as a (garbage) program header table and failing later as
/// `NoLoadSegments`.
#[test]
fn a_program_header_offset_inside_the_file_header_is_refused_by_name() {
    for phoff in [0u64, 1, 32, 63] {
        assert_eq!(
            refused(Elf::honest(0x1000).phoff(phoff).phnum(1).build()),
            Error::ProgramHeadersInsideFileHeader,
            "e_phoff {phoff:#x}",
        );
    }
    // 64 itself is the boundary and is not refused by this check; an honest
    // file's real table starts there.
    assert_eq!(accepted(Elf::honest(0x1000).build()).segments().len(), 1);
}

/// `e_phoff + e_phnum * 56` is `usize` arithmetic on a `u64` a file chose. The
/// shape this replaces computed it unchecked and then sliced with it: with
/// overflow checks on that is a kernel panic, and with them off the slice's own
/// inverted range is a kernel panic.
#[test]
fn a_program_header_offset_that_overflows_is_refused() {
    let cases = [
        u64::MAX,
        u64::MAX - PROGRAM_HEADER_SIZE as u64 + 1,
        u64::MAX - PROGRAM_HEADER_SIZE as u64,
        usize::MAX as u64,
        0x1000,
        0x8000_0000_0000_0000,
    ];
    for phoff in cases {
        assert_eq!(
            refused(Elf::honest(0x1000).phoff(phoff).phnum(1).build()),
            Error::ProgramHeadersOutsideBuffer,
            "e_phoff {phoff:#x}",
        );
    }
}

/// `e_phnum * 56` on its own, with `e_phoff` honest.
#[test]
fn a_program_header_count_past_the_buffer_is_refused() {
    assert_eq!(
        refused(Elf::honest(0x1000).phnum(u16::MAX).build()),
        Error::ProgramHeadersOutsideBuffer,
    );
}

// ── Segments ────────────────────────────────────────────────────────────

#[test]
fn a_file_with_no_load_segment_is_refused() {
    let bytes = Elf::new(0x1000).ph(Phdr::tls(0, 0, 8, 8)).build();
    assert_eq!(refused(bytes), Error::NoLoadSegments);
}

#[test]
fn the_load_segment_count_is_bounded_and_the_bound_is_generous() {
    let mut ok = Elf::new(0x4000);
    for i in 0..MAX_LOAD_SEGMENTS {
        ok = ok.ph(Phdr::load(0, i as u64 * 0x1000, 0, 0x1000, PF_R));
    }
    assert_eq!(accepted(ok.build()).segments().len(), MAX_LOAD_SEGMENTS);

    let mut too_many = Elf::new(0x4000);
    for i in 0..=MAX_LOAD_SEGMENTS {
        too_many = too_many.ph(Phdr::load(0, i as u64 * 0x1000, 0, 0x1000, PF_R));
    }
    assert_eq!(refused(too_many.build()), Error::TooManyLoadSegments);
}

#[test]
fn filesz_above_memsz_is_refused_for_load_and_for_tls() {
    let load = Elf::new(0x1000)
        .ph(Phdr::load(0, 0, 0x2000, 0x1000, PF_R | PF_W))
        .build();
    assert_eq!(refused(load), Error::FileszAboveMemsz);

    let tls = Elf::honest(0x1000).ph(Phdr::tls(0, 0x100, 8, 8)).build();
    assert_eq!(refused(tls), Error::FileszAboveMemsz);
}

#[test]
fn a_segment_extent_that_overflows_is_refused() {
    let bytes = Elf::new(0x1000)
        .ph(Phdr::load(0, u64::MAX - 0x100, 0, 0x1000, PF_R))
        .build();
    assert_eq!(refused(bytes), Error::SegmentExtentOverflows);
}

#[test]
fn a_file_extent_that_overflows_is_refused() {
    let bytes = Elf::new(0x1000)
        .ph(Phdr::load(u64::MAX - 0x100, 0, 0x1000, 0x1000, PF_R))
        .build();
    assert_eq!(refused(bytes), Error::FileExtentOverflows);
}

/// `base + e_entry` is where the process starts, and `base` is chosen so the
/// image lands in the user half — an entry outside the image lands wherever the
/// file wants.
#[test]
fn an_entry_point_outside_the_image_is_refused() {
    let below = Elf::new(0x2000)
        .ph(Phdr::load(0, 0x1000, 0x1000, 0x1000, PF_R | PF_X))
        .entry(0)
        .build();
    assert_eq!(refused(below), Error::EntryOutsideImage);

    let one_past = Elf::honest(0x1000).entry(0x1000).build();
    assert_eq!(refused(one_past), Error::EntryOutsideImage);

    let wild = Elf::honest(0x1000).entry(u64::MAX).build();
    assert_eq!(refused(wild), Error::EntryOutsideImage);

    let last_byte = Elf::honest(0x1000).entry(0xFFF).build();
    assert_eq!(accepted(last_byte).entry, 0xFFF);
}

#[test]
fn a_header_naming_a_vaddr_outside_the_image_is_refused_by_name() {
    let tls = Elf::honest(0x1000).ph(Phdr::tls(0x8000, 8, 8, 8)).build();
    assert_eq!(refused(tls), Error::TlsOutsideImage);

    let dynamic = Elf::honest(0x1000)
        .ph(Phdr { kind: PT_DYNAMIC, flags: PF_R, offset: 0, vaddr: 0x800, filesz: 0x1000, memsz: 0x1000, align: 8 })
        .build();
    assert_eq!(refused(dynamic), Error::DynamicOutsideImage);

    let eh = Elf::honest(0x1000)
        .ph(Phdr { kind: PT_GNU_EH_FRAME, flags: PF_R, offset: 0, vaddr: 0, filesz: 0, memsz: u64::MAX, align: 4 })
        .build();
    assert_eq!(refused(eh), Error::EhFrameOutsideImage);
}

/// `.tbss` is address space the containing `PT_LOAD` need not cover: only the
/// file-backed part of `PT_TLS` has to be inside the image.
#[test]
fn tls_bss_may_extend_past_the_image() {
    let bytes = Elf::honest(0x1000).ph(Phdr::tls(0xF00, 0x100, 0x9000, 8)).build();
    assert_eq!(accepted(bytes).tls.unwrap().memsz, 0x9000);
}

#[test]
fn tls_alignment_is_a_power_of_two_within_a_page_or_it_is_refused() {
    for align in [3u64, 5, 0x8000_0000_0000_0003, u64::MAX, 4 * 1024 * 1024] {
        let bytes = Elf::honest(0x1000).ph(Phdr::tls(0, 0, 8, align)).build();
        assert_eq!(refused(bytes), Error::BadTlsAlign, "p_align {align:#x}");
    }
    for align in [0u64, 1, 8, 64, 2 * 1024 * 1024] {
        let bytes = Elf::honest(0x1000).ph(Phdr::tls(0, 0, 8, align)).build();
        assert_eq!(accepted(bytes).tls.unwrap().align, align, "p_align {align:#x}");
    }
}

/// Absent TLS is `None`, never a zero `memsz`: a module with a `PT_TLS` of zero
/// size still gets a DTV slot, and telling the two apart is what the sentinel
/// could not do.
#[test]
fn a_zero_size_tls_segment_is_present_not_absent() {
    let with = Elf::honest(0x1000).ph(Phdr::tls(0, 0, 0, 8)).build();
    assert!(accepted(with).tls.is_some());
    assert!(accepted(Elf::honest(0x1000).build()).tls.is_none());
}

// ── The section header table, which is optional ─────────────────────────

#[test]
fn a_section_table_the_loader_cannot_index_is_dropped_not_refused() {
    for entsize in [0u16, 32, 63, 65, 128] {
        let layout = accepted(Elf::honest(0x1000).sections(0x100, 4, entsize).build());
        assert!(layout.section_headers.is_none(), "e_shentsize {entsize}");
    }
    assert!(accepted(Elf::honest(0x1000).sections(0, 4, 64).build()).section_headers.is_none());
    assert!(accepted(Elf::honest(0x1000).sections(0x100, 0, 64).build()).section_headers.is_none());

    let good = accepted(Elf::honest(0x1000).sections(0x100, 4, 64).build());
    assert_eq!(good.section_headers.unwrap().byte_len(), 256);
}

// ── Derived answers about a valid layout ────────────────────────────────

#[test]
fn segments_that_share_a_page_are_an_overlap() {
    let shared = accepted(
        Elf::honest(0x4000)
            .ph(Phdr::load(0x1000, 0x800, 0x1000, 0x1000, PF_R | PF_W))
            .build(),
    );
    assert!(shared.overlapping_load_pages(4096).is_some());

    let adjacent = accepted(
        Elf::new(0x4000)
            .ph(Phdr::load(0, 0, 0x1000, 0x1000, PF_R | PF_X))
            .ph(Phdr::load(0x1000, 0x1000, 0x1000, 0x1000, PF_R | PF_W))
            .build(),
    );
    assert_eq!(adjacent.overlapping_load_pages(4096), None);

    // A segment covering the top of the address space rounds its last page up
    // past `u64::MAX`; the answer is still an answer.
    let huge = accepted(
        Elf::new(0x1000)
            .ph(Phdr::load(0, 0, 0x1000, 0x1000, PF_R))
            .ph(Phdr::load(0, 0x1000, 0, u64::MAX - 0x1000, PF_R | PF_W))
            .build(),
    );
    assert_eq!(huge.overlapping_load_pages(4096), None);
}

#[test]
fn a_vaddr_below_every_segment_has_no_file_offset() {
    let layout = accepted(
        Elf::new(0x4000)
            .ph(Phdr::load(0x1000, 0x1000, 0x3000, 0x3000, PF_R | PF_W))
            .entry(0x1000)
            .build(),
    );
    assert_eq!(layout.vaddr_to_file_offset(0), None);
    assert_eq!(layout.vaddr_to_file_offset(0xFFF), None);
    assert_eq!(layout.vaddr_to_file_offset(0x1000), Some(0x1000));
    assert_eq!(layout.vaddr_to_file_offset(0x2000), Some(0x2000));
    // Past the file image of the only segment, extrapolated — what `.rela.dyn`
    // needs when the linker places it outside any `PT_LOAD`.
    assert_eq!(layout.vaddr_to_file_offset(0x9000), Some(0x9000));
}

#[test]
fn an_extrapolated_file_offset_that_overflows_has_no_answer() {
    let layout = accepted(
        Elf::new(0x1000)
            .ph(Phdr::load(u64::MAX - 0x1000, 0, 0x1000, 0x1000, PF_R))
            .build(),
    );
    assert_eq!(layout.vaddr_to_file_offset(0), Some(u64::MAX - 0x1000));
    assert_eq!(layout.vaddr_to_file_offset(0x2000), None);
}

/// `.gnu.hash` declares its length nowhere, so its bound is the file image of
/// the segment holding it.
#[test]
fn the_file_bytes_behind_a_vaddr_are_the_containing_segments() {
    let layout = accepted(
        Elf::new(0x4000)
            .ph(Phdr::load(0, 0, 0x1000, 0x2000, PF_R | PF_X))
            .ph(Phdr::load(0x2000, 0x2000, 0x800, 0x2000, PF_R | PF_W))
            .build(),
    );
    assert_eq!(layout.file_bytes_from(0), Some(0x1000));
    assert_eq!(layout.file_bytes_from(0xF00), Some(0x100));
    // Inside the segment's memory image but past its file image: no bytes.
    assert_eq!(layout.file_bytes_from(0x1800), None);
    assert_eq!(layout.file_bytes_from(0x2400), Some(0x400));
}

#[test]
fn the_writable_window_spans_every_writable_segment() {
    let layout = accepted(
        Elf::new(0x4000)
            .ph(Phdr::load(0, 0, 0x1000, 0x1000, PF_R | PF_X))
            .ph(Phdr::load(0x1000, 0x1000, 0x1000, 0x1000, PF_R | PF_W))
            .ph(Phdr::load(0x2000, 0x3000, 0x1000, 0x1000, PF_R | PF_W))
            .build(),
    );
    assert_eq!(layout.writable_window(), Some((0x1000, 0x4000)));
    assert_eq!(accepted(Elf::honest(0x1000).build()).writable_window(), None);
}

#[test]
fn segment_flags_survive_the_parse() {
    let layout = accepted(
        Elf::new(0x2000)
            .ph(Phdr::load(0, 0, 0x1000, 0x1000, PF_R | PF_X))
            .ph(Phdr::load(0x1000, 0x1000, 0x1000, 0x1000, PF_R | PF_W))
            .build(),
    );
    let text = layout.segments()[0].flags;
    assert!(text.readable() && text.executable() && !text.writable());
    let data = layout.segments()[1].flags;
    assert!(data.readable() && data.writable() && !data.executable());
}

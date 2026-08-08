//! The positive control: the parser agrees with a real artifact.
//!
//! Every other test in this crate hands the parser bytes no linker would emit,
//! and a parser that refused everything would pass all of them. The fixture is
//! the first 4 KiB of `/bin/shell` as `toyos-ld` linked it — which is all
//! [`Layout::parse`] ever reads — so this is the shape the loader actually
//! meets at every boot.
//!
//! Refresh it with `dd if=userland/target/x86_64-unknown-toyos/debug/shell
//! of=toyos-elf/tests/fixtures/toyos-ld-headers.bin bs=1 count=4096`, and
//! expect the entry point below to move.

use toyos_elf::Layout;

const HEADERS: &[u8] = include_bytes!("fixtures/toyos-ld-headers.bin");

#[test]
fn a_toyos_ld_binary_parses_to_what_readelf_says() {
    let layout = Layout::parse(HEADERS).expect("toyos-ld's own output");

    assert_eq!(layout.entry, 0x3261c);
    assert_eq!(layout.vaddr_min, 0);
    assert_eq!(layout.vaddr_max, 0x167000);

    // text (R-X), data (RW-), rodata carrying .rela.dyn and .dynamic (R--).
    let segs = layout.segments();
    assert_eq!(segs.len(), 3);
    assert!(segs[0].flags.executable() && !segs[0].writable());
    assert!(segs[1].writable() && !segs[1].flags.executable());
    assert!(!segs[2].writable() && !segs[2].flags.executable());
    assert_eq!(layout.writable_window(), Some((0x145000, 0x155000)));

    assert_eq!(layout.dynamic, Some((0x166490, 0x166490, 0x60)));
    assert_eq!(layout.eh_frame_hdr, Some((0x13e118, 0x688c)));
    assert_eq!(layout.tls.unwrap().memsz, 0x90);
    assert_eq!(layout.tls.unwrap().align, 0x40);

    let sections = layout.section_headers.expect("a section header table");
    assert_eq!((sections.count, sections.entry_size), (9, 64));

    // Every `DT_*` vaddr in this file resolves, and no two segments contend for
    // a page — the two derived answers `spawn` refuses a binary over.
    assert_eq!(layout.overlapping_load_pages(4096), None);
    assert_eq!(layout.vaddr_to_file_offset(0x166490), Some(0x166490));
    assert_eq!(layout.vaddr_to_file_offset(0x155000), Some(0x155000));
}

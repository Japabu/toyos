//! The tables an image points at, given hostile bytes.
//!
//! Every one of them is a view over a buffer the caller already holds, so the
//! question each case asks is the same: does a length, count or index the file
//! chose ever reach past that buffer, and does anything here panic instead of
//! answering.

// `common` is one builder shared by two test binaries, and each drives a
// different half of it: this one builds the tables, `crafted.rs` builds the
// whole files that point at them. Every item is used by one of the two, so the
// unused half is dead only from here.
#[allow(dead_code)]
mod common;

use common::*;
use toyos_elf::dynamic::{self, Dynamic};
use toyos_elf::gnu_hash::{self, GnuHash};
use toyos_elf::rela::{self, RelaCounts, RelaTable, RelocError, RelocKind};
use toyos_elf::section::{SectionTable, SHT_DYNSYM, SHT_RELA, SHT_SYMTAB};
use toyos_elf::sym::SymTab;

/// `st_info` for a global `STT_FUNC`, and for the data object it is told apart
/// from.
const FUNC: u8 = (1 << 4) | 2;
const OBJECT: u8 = (1 << 4) | 1;

// ── PT_DYNAMIC ──────────────────────────────────────────────────────────

/// A tag naming zero is a tag naming zero, which is a legal address in a
/// `ET_DYN` image. The shape this replaces read absence and a zero value as the
/// same thing at every use site.
#[test]
fn a_tag_is_absent_or_present_never_zero_meaning_absent() {
    let named_zero = Dynamic::parse(&dynamic(&[(dynamic::DT_SYMTAB, 0)]));
    assert_eq!(named_zero.symtab, Some(0));

    let silent = Dynamic::parse(&dynamic(&[]));
    assert_eq!(silent.symtab, None);
    assert_eq!(silent.gnu_hash, None);
    assert_eq!(silent.strtab, None);
}

/// A table needs both of its tags. `DT_RELA` alone names a table of unknown
/// length, which nothing can read.
#[test]
fn a_table_needs_both_of_its_tags_and_a_non_zero_size() {
    assert_eq!(Dynamic::parse(&dynamic(&[(dynamic::DT_RELA, 0x1000)])).rela, None);
    assert_eq!(Dynamic::parse(&dynamic(&[(dynamic::DT_RELASZ, 24)])).rela, None);
    assert_eq!(
        Dynamic::parse(&dynamic(&[(dynamic::DT_RELA, 0x1000), (dynamic::DT_RELASZ, 0)])).rela,
        None,
    );
    assert_eq!(
        Dynamic::parse(&dynamic(&[(dynamic::DT_RELA, 0x1000), (dynamic::DT_RELASZ, 48)])).rela,
        Some(dynamic::Table { vaddr: 0x1000, size: 48 }),
    );
}

#[test]
fn a_dynamic_table_with_no_null_terminator_stops_at_the_buffer() {
    let tags: Vec<_> = (0..64).map(|i| (dynamic::DT_NEEDED, i as u64)).collect();
    let data = dynamic_unterminated(&tags);
    assert_eq!(Dynamic::needed(&data).count(), 64);

    // And a trailing partial entry is not an entry.
    let short = &data[..data.len() - 4];
    assert_eq!(Dynamic::needed(short).count(), 63);
}

#[test]
fn everything_after_dt_null_is_ignored() {
    let mut data = dynamic(&[(dynamic::DT_NEEDED, 1)]);
    data.extend_from_slice(&dynamic_unterminated(&[(dynamic::DT_NEEDED, 2)]));
    assert_eq!(Dynamic::needed(&data).collect::<Vec<_>>(), vec![1]);
}

// ── Relocation tables ───────────────────────────────────────────────────

#[test]
fn a_trailing_partial_relocation_is_not_a_relocation() {
    let mut bytes = rela(0x10, 0, 8, 4).to_vec();
    bytes.extend_from_slice(&[0u8; 23]);
    let table = RelaTable::new(&bytes);
    assert_eq!(table.len(), 1);
    assert_eq!(table.get(1), None);
    assert_eq!(table.iter().count(), 1);
}

#[test]
fn relocation_types_map_to_the_width_the_writers_use() {
    assert_eq!(RelocKind::from_raw(8).write_width(), Some(8));
    assert_eq!(RelocKind::from_raw(23).write_width(), Some(4));
    assert_eq!(RelocKind::from_raw(0).write_width(), None);
    assert_eq!(RelocKind::from_raw(42).write_width(), None);
    // RELATIVE is the one written type that resolves no symbol, so it is the
    // one whose `r_sym` needs no bound.
    assert!(!RelocKind::Relative.needs_symbol());
    for raw in [6u32, 7, 16, 17, 18, 23] {
        assert!(RelocKind::from_raw(raw).needs_symbol(), "type {raw}");
    }
}

#[test]
fn validation_refuses_a_write_outside_the_window_by_name() {
    let window = (0x1000u64, 0x2000u64);

    let overflowing = [rela(u64::MAX - 3, 0, 8, 0)].concat();
    assert_eq!(
        rela::validate(RelaTable::new(&overflowing).iter(), window, 4),
        Err(RelocError::OffsetOverflows),
    );

    let below = [rela(0xFF8, 0, 8, 0)].concat();
    assert_eq!(
        rela::validate(RelaTable::new(&below).iter(), window, 4),
        Err(RelocError::OutsideWindow),
    );

    // One byte of an eight-byte write past the end.
    let straddling = [rela(0x1FF9, 0, 8, 0)].concat();
    assert_eq!(
        rela::validate(RelaTable::new(&straddling).iter(), window, 4),
        Err(RelocError::OutsideWindow),
    );

    let fits = [rela(0x1FF8, 0, 8, 0)].concat();
    assert_eq!(rela::validate(RelaTable::new(&fits).iter(), window, 4), Ok(()));
}

/// A `DTPOFF64` with `r_sym == 0` writes its addend verbatim, so validation has
/// to cover every type the loader writes and not just the ones it applies at
/// load time.
#[test]
fn every_written_type_is_validated_and_no_other_is() {
    let window = (0u64, 0x100u64);
    for raw in [6u32, 7, 8, 16, 17, 18, 23] {
        let bytes = [rela(0x1000, 0, raw, 0)].concat();
        assert_eq!(
            rela::validate(RelaTable::new(&bytes).iter(), window, 4),
            Err(RelocError::OutsideWindow),
            "type {raw} was not validated",
        );
    }
    // A type nobody patches may name any offset at all.
    let ignored = [rela(u64::MAX, 0, 42, 0)].concat();
    assert_eq!(rela::validate(RelaTable::new(&ignored).iter(), window, 0), Ok(()));
}

#[test]
fn a_symbol_index_past_the_table_is_refused_except_for_relative() {
    let window = (0u64, 0x100u64);

    let bind = [rela(0x10, 4, 6, 0)].concat();
    assert_eq!(
        rela::validate(RelaTable::new(&bind).iter(), window, 4),
        Err(RelocError::SymbolPastTable),
    );
    assert_eq!(rela::validate(RelaTable::new(&bind).iter(), window, 5), Ok(()));

    let relative = [rela(0x10, u32::MAX, 8, 0)].concat();
    assert_eq!(rela::validate(RelaTable::new(&relative).iter(), window, 0), Ok(()));
}

#[test]
fn counts_are_per_kind_over_every_table() {
    let a = [rela(0, 0, 8, 0), rela(8, 1, 6, 0), rela(16, 2, 18, 0)].concat();
    let b = [rela(24, 3, 7, 0), rela(32, 0, 23, 0), rela(40, 0, 99, 0)].concat();
    let counts = RelaCounts::of(RelaTable::new(&a).iter().chain(RelaTable::new(&b).iter()));
    assert_eq!(
        counts,
        RelaCounts { relative: 1, bind: 2, tpoff64: 1, tpoff32: 1, dtpmod64: 0, dtpoff64: 0 },
    );
    // The ceiling is over the kinds a caller reserves for, never over every
    // kind: a bound on one nothing stores refuses a file for a collection that
    // does not exist.
    assert_eq!(counts.max_of(&[RelocKind::GlobDat, RelocKind::Tpoff32]), 2);
    assert_eq!(counts.max_of(&[RelocKind::DtpMod64]), 0);
    assert_eq!(counts.max_of(&[]), 0);
}

// ── Symbol tables ───────────────────────────────────────────────────────

/// The shape this replaces sliced from `index * 24` and read 24 bytes through a
/// raw pointer: one entry short of the end, that read past the buffer.
#[test]
fn a_symbol_index_at_the_edge_of_the_bytes_reads_nothing() {
    let mut syms = sym(0, 0, 0, 0).to_vec();
    syms.extend_from_slice(&sym(1, 0x10, 1, 0x2000)[..23]);
    let table = SymTab::new(&syms, b"\0name\0");
    assert_eq!(table.count(), 1);
    assert_eq!(table.get(1), None);
    assert_eq!(table.name(1), "");
    assert_eq!(table.name(usize::MAX), "");
}

#[test]
fn a_name_offset_past_the_string_table_is_empty() {
    let syms = [sym(0, 0, 0, 0), sym(u32::MAX, 0x10, 1, 0)].concat();
    let table = SymTab::new(&syms, b"\0abc\0");
    assert_eq!(table.name(1), "");
}

/// A run with no NUL before the table ends is that table's last name, bounded
/// by the table's own length.
#[test]
fn an_unterminated_name_is_the_rest_of_the_string_table() {
    let syms = [sym(0, 0, 0, 0), sym(1, 0x10, 1, 0)].concat();
    assert_eq!(SymTab::new(&syms, b"\0tail").name(1), "tail");
}

#[test]
fn a_name_that_is_not_utf8_matches_nothing() {
    let syms = [sym(0, 0, 0, 0), sym(1, 0x10, 1, 0)].concat();
    let table = SymTab::new(&syms, b"\0\xff\xfe\0");
    assert_eq!(table.name(1), "");
    assert_eq!(table.find("\u{fffd}\u{fffd}"), None);
}

#[test]
fn an_empty_symbol_table_answers_every_question_with_nothing() {
    let table = SymTab::empty();
    assert_eq!(table.count(), 0);
    assert_eq!(table.get(0), None);
    assert_eq!(table.find("main"), None);
    assert_eq!(table.find_tls("x"), None);
    assert_eq!(table.defined().count(), 0);
}

#[test]
fn lookups_skip_undefined_symbols_and_the_null_entry() {
    let syms = [
        sym(0, 0, 0, 0),
        sym(1, (1 << 4) | 6, 0, 0x10), // undefined TLS import
        sym(1, (1 << 4) | 6, 3, 0x40), // the definition
    ]
    .concat();
    let table = SymTab::new(&syms, b"\0tls_var\0");
    assert_eq!(table.find_tls("tls_var"), Some(0x40));
    assert_eq!(table.find("tls_var").map(|(i, _)| i), Some(2));
    assert_eq!(table.defined().count(), 1);
}

// ── Address to symbol ───────────────────────────────────────────────────
//
// `SymTab::resolve` is what a backtrace frame is named by, and the kernel's
// panic path is its caller — so every case here is one an address in a crash
// report can land on. Before this lived in the crate it was a raw-pointer scan
// in `kernel/src/symbols.rs` with no test of any kind.

/// The ordinary answer: the nearest function at or below the address, and how
/// far into it the address is.
#[test]
fn an_address_resolves_to_the_function_that_contains_it() {
    let syms = [
        sym(0, 0, 0, 0),
        sym_sized(1, FUNC, 1, 0x1000, 0x40),  // first
        sym_sized(7, FUNC, 1, 0x2000, 0x100), // second
    ]
    .concat();
    let table = SymTab::new(&syms, b"\0first\0second\0");

    assert_eq!(table.resolve(0x1000), Some(("first", 0)));
    assert_eq!(table.resolve(0x103f), Some(("first", 0x3f)));
    assert_eq!(table.resolve(0x2010), Some(("second", 0x10)));
    // Below every symbol, and between two of them where the first one's size
    // says the address is not inside it.
    assert_eq!(table.resolve(0x0fff), None);
    assert_eq!(table.resolve(0x1040), None);
}

/// **A return address is the instruction after the `call`**, so a call in tail
/// position lands one byte past its function's last byte — and refusing that is
/// deliberate, because the alternative is naming the *next* function as the one
/// that was executing. `resolve_return` is the kernel's answer to it and this
/// is the property it rests on.
#[test]
fn one_byte_past_a_sized_symbol_is_not_that_symbol() {
    let syms = [sym(0, 0, 0, 0), sym_sized(1, FUNC, 1, 0x1000, 0x10)].concat();
    let table = SymTab::new(&syms, b"\0f\0");
    assert_eq!(table.resolve(0x100f), Some(("f", 0xf)));
    assert_eq!(table.resolve(0x1010), None);
}

/// A symbol with no size bounds nothing, so every address above it is inside
/// it until a later symbol takes over. That is the assembly case — hand-written
/// entry points carry `st_size` 0 — and losing it would leave every frame in
/// `arch/entry.rs` unnamed.
#[test]
fn a_symbol_with_no_size_owns_everything_above_it() {
    let syms = [sym(0, 0, 0, 0), sym_sized(1, FUNC, 1, 0x1000, 0)].concat();
    let table = SymTab::new(&syms, b"\0naked\0");
    assert_eq!(table.resolve(0x1000), Some(("naked", 0)));
    assert_eq!(table.resolve(0xffff_ffff), Some(("naked", 0xffff_efff)));
}

/// Two symbols at one address is what an alias produces — `__memcpy` and
/// `memcpy` on the same byte — and the sized one is the one that can say
/// whether an address is still inside it. **Both orders, because the tie-break
/// is what a scan in table order gets wrong.**
#[test]
fn an_alias_pair_resolves_to_the_one_that_carries_a_size() {
    let sized_last = [
        sym(0, 0, 0, 0),
        sym_sized(1, FUNC, 1, 0x1000, 0),
        sym_sized(7, FUNC, 1, 0x1000, 0x20),
    ]
    .concat();
    let table = SymTab::new(&sized_last, b"\0plain\0sized\0");
    assert_eq!(table.resolve(0x1004), Some(("sized", 4)));

    let sized_first = [
        sym(0, 0, 0, 0),
        sym_sized(1, FUNC, 1, 0x1000, 0x20),
        sym_sized(7, FUNC, 1, 0x1000, 0),
    ]
    .concat();
    let table = SymTab::new(&sized_first, b"\0sized\0plain\0");
    assert_eq!(table.resolve(0x1004), Some(("sized", 4)));
}

/// Only `STT_FUNC`, and never a symbol at zero. A data object at the address
/// would otherwise name a frame after a variable, and `st_value == 0` is what
/// an undefined symbol and an empty section both look like.
#[test]
fn data_symbols_and_zero_valued_ones_name_no_frame() {
    let syms = [
        sym(0, 0, 0, 0),
        sym_sized(1, OBJECT, 1, 0x1000, 0x40), // data at the address
        sym_sized(7, FUNC, 1, 0, 0x40),        // a function at zero
    ]
    .concat();
    let table = SymTab::new(&syms, b"\0table\0nowhere\0");
    assert_eq!(table.resolve(0x1000), None);
    assert_eq!(table.resolve(0x10), None);
}

/// A hostile or truncated table answers nothing rather than panicking or
/// reading past its bytes — the same property every other view in this crate
/// has, on the one path that runs inside a panic handler.
#[test]
fn a_symbol_table_that_cannot_be_read_names_nothing() {
    assert_eq!(SymTab::empty().resolve(0x1000), None);

    // A last entry one byte short: `count` does not see it.
    let mut short = sym(0, 0, 0, 0).to_vec();
    short.extend_from_slice(&sym_sized(1, FUNC, 1, 0x1000, 0x10)[..23]);
    assert_eq!(SymTab::new(&short, b"\0f\0").resolve(0x1000), None);

    // A name offset past the string table is no name, and no name is no
    // answer: a frame reading `+0x4` with nothing in front of it is worse than
    // a bare address.
    let syms = [sym(0, 0, 0, 0), sym_sized(u32::MAX, FUNC, 1, 0x1000, 0x10)].concat();
    assert_eq!(SymTab::new(&syms, b"\0f\0").resolve(0x1004), None);
}

// ── Section headers ─────────────────────────────────────────────────────

#[test]
fn a_truncated_section_table_names_the_sections_it_covers() {
    let mut bytes = shdr(0, 0, 0, 0, 0).to_vec();
    bytes.extend_from_slice(&shdr(SHT_SYMTAB, 0x100, 48, 1, 24));
    bytes.truncate(bytes.len() - 1);
    let table = SectionTable::new(&bytes);
    assert_eq!(table.len(), 1);
    assert_eq!(table.find(SHT_SYMTAB), None);
}

#[test]
fn a_symbol_section_whose_link_names_nothing_is_no_symbol_section() {
    let bytes = [
        shdr(0, 0, 0, 0, 0),
        shdr(SHT_SYMTAB, 0x100, 48, 9, 24),
        shdr(SHT_DYNSYM, 0x200, 48, 0, 24),
    ]
    .concat();
    let table = SectionTable::new(&bytes);
    assert_eq!(table.symbols(SHT_SYMTAB), None);
    assert!(table.symbols(SHT_DYNSYM).is_some());
}

/// `.rela.dyn` is identified by shape — a `SHT_RELA` of 24-byte entries whose
/// first is `R_X86_64_RELATIVE` — because reading section names needs
/// `.shstrtab`, which needs `e_shstrndx`, which is not in this table.
#[test]
fn rela_dyn_is_found_by_shape_and_only_by_shape() {
    let bytes = [
        shdr(0, 0, 0, 0, 0),
        shdr(SHT_RELA, 0x100, 24, 0, 16), // wrong entry size
        shdr(SHT_RELA, 0x200, 0, 0, 24),  // empty
        shdr(SHT_RELA, 0x300, 48, 0, 24), // first entry is not RELATIVE
        shdr(SHT_RELA, 0x400, 72, 0, 24), // this one
    ]
    .concat();
    let table = SectionTable::new(&bytes);
    let mut reader = |off: u64| match off {
        0x300 => RelaTable::new(&rela(0, 1, 6, 0)).get(0),
        0x400 => RelaTable::new(&rela(0, 0, 8, 0)).get(0),
        _ => None,
    };
    assert_eq!(table.rela_dyn(&mut reader), Some((0x400, 72)));
}

// ── .gnu.hash ───────────────────────────────────────────────────────────

/// A `.gnu.hash` header whose divisors are zero, or whose bloom shift is not a
/// shift, is not a hash table. `bloom_shift >= 32` used to reach `u32 >> shift`
/// in the kernel, which is a panic with overflow checks on — and they are on.
#[test]
fn a_hash_header_that_cannot_be_used_is_refused() {
    assert!(GnuHash::parse(&[]).is_none());
    assert!(GnuHash::parse(&hash_table(0, 0, 1, 0, &[], &[])).is_none());
    assert!(GnuHash::parse(&hash_table(1, 0, 0, 0, &[], &[])).is_none());
    for shift in [64u32, 65, 255, u32::MAX] {
        assert!(
            GnuHash::parse(&hash_table(1, 0, 1, shift, &[], &[])).is_none(),
            "bloom_shift {shift}",
        );
    }
    for shift in [0u32, 5, 31, 32, 63] {
        let bytes = hash_table(1, 0, 1, shift, &[0], &[1]);
        let table = GnuHash::parse(&bytes).expect("a usable header");
        assert_eq!(table.lookup("anything", &SymTab::empty()), None);
    }
}

#[test]
fn a_chain_that_never_terminates_yields_no_symbol_count() {
    // One bucket pointing at chain entry 0, and a chain of zeros: no entry ever
    // sets the terminator bit, so the walk runs to the end of the bytes.
    let bytes = hash_table(1, 0, 1, 0, &[1], &[0; 64]);
    let table = GnuHash::parse(&bytes).unwrap();
    assert_eq!(table.sym_count(), None);
}

#[test]
fn a_bucket_pointing_past_the_chain_array_yields_no_symbol_count() {
    let bytes = hash_table(1, 0, 1, 0, &[9999], &[1, 1]);
    assert_eq!(GnuHash::parse(&bytes).unwrap().sym_count(), None);
}

#[test]
fn a_bucket_below_the_symbol_offset_is_the_symbol_offset() {
    let bytes = hash_table(1, 7, 1, 0, &[2], &[1]);
    assert_eq!(GnuHash::parse(&bytes).unwrap().sym_count(), Some(7));
}

/// A chain index below `symoffset` is a negative index into the chain array.
#[test]
fn a_chain_index_below_the_symbol_offset_ends_the_lookup() {
    let name = "sym";
    let h = gnu_hash::hash(name);
    let bloom = bloom_word_for(h, 0);
    let bytes = hash_table(1, 5, 1, 0, &[1], &[h | 1]);
    let mut bytes = bytes;
    bytes[16..24].copy_from_slice(&bloom.to_le_bytes());
    let table = GnuHash::parse(&bytes).unwrap();
    assert_eq!(table.lookup(name, &SymTab::empty()), None);
}

/// The positive control: a table the linker would have written answers the
/// question it exists to answer. Without this, every case above would pass on a
/// parser that always says `None`.
#[test]
fn a_consistent_table_finds_its_symbol_and_counts_it() {
    let name = "toyos_symbol";
    let h = gnu_hash::hash(name);
    let mut bytes = hash_table(1, 1, 1, 0, &[1], &[h | 1]);
    bytes[16..24].copy_from_slice(&bloom_word_for(h, 0).to_le_bytes());

    let table = GnuHash::parse(&bytes).unwrap();
    assert_eq!(table.sym_count(), Some(2));

    let syms = [sym(0, 0, 0, 0), sym(1, (1 << 4) | 1, 3, 0x2000)].concat();
    let mut strs = vec![0u8];
    strs.extend_from_slice(name.as_bytes());
    strs.push(0);
    let symtab = SymTab::new(&syms, &strs);

    assert_eq!(table.lookup(name, &symtab), Some(1));
    assert_eq!(table.lookup("absent", &symtab), None);
}

/// A chain entry naming an index the symbol table does not hold is skipped, not
/// trusted: nothing else bounds the indices a chain may contain.
#[test]
fn a_chain_naming_a_symbol_the_table_does_not_hold_resolves_to_nothing() {
    let name = "gone";
    let h = gnu_hash::hash(name);
    let mut bytes = hash_table(1, 1, 1, 0, &[1], &[h | 1]);
    bytes[16..24].copy_from_slice(&bloom_word_for(h, 0).to_le_bytes());
    let table = GnuHash::parse(&bytes).unwrap();
    assert_eq!(table.lookup(name, &SymTab::empty()), None);
}

/// `[nbuckets, symoffset, bloom_size, bloom_shift, bloom[], buckets[], chain[]]`
/// with a bloom filter of all ones, so it rejects nothing by itself.
fn hash_table(
    nbuckets: u32,
    symoffset: u32,
    bloom_size: u32,
    bloom_shift: u32,
    buckets: &[u32],
    chain: &[u32],
) -> Vec<u8> {
    let mut out = Vec::new();
    for v in [nbuckets, symoffset, bloom_size, bloom_shift] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for _ in 0..bloom_size {
        out.extend_from_slice(&u64::MAX.to_le_bytes());
    }
    for &b in buckets {
        out.extend_from_slice(&b.to_le_bytes());
    }
    for &c in chain {
        out.extend_from_slice(&c.to_le_bytes());
    }
    out
}

fn bloom_word_for(h: u32, shift: u32) -> u64 {
    (1u64 << (h % 64)) | (1u64 << ((h as u64 >> shift) % 64))
}

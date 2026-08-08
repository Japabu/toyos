//! The TLS block's arithmetic, over the whole space a file can name.
//!
//! `total_memsz` and `align` are sums of numbers a file declared, and the
//! kernel used to assert `dtv_size < tls_start` after computing them — a
//! kernel-bug assert reached from a crafted `PT_TLS`. The property is proved
//! here instead, which is what lets the assert go.

use toyos_elf::tls;

const TCB: usize = 64;
const DTV: usize = 16 + 64 * 8;
const GRANULE: usize = 2 * 1024 * 1024;

#[test]
fn the_dtv_is_never_overwritten_by_tls_data() {
    let sizes = [
        0usize,
        1,
        8,
        4095,
        4096,
        DTV,
        DTV + 1,
        0x1F_FF00,
        GRANULE - TCB - 1,
        GRANULE,
        GRANULE + 1,
        16 * GRANULE,
    ];
    let aligns = [0usize, 1, 2, 8, 16, 64, 4096, 65536, GRANULE];
    for &memsz in &sizes {
        for &align in &aligns {
            let plan = tls::plan(memsz, align, TCB, DTV, GRANULE)
                .unwrap_or_else(|| panic!("no plan for memsz {memsz} align {align}"));
            let effective = if align > 1 { align } else { 8 };
            assert!(
                plan.tls_start >= DTV,
                "memsz {memsz} align {align}: TLS data at {} overlaps a {DTV}-byte DTV",
                plan.tls_start,
            );
            assert_eq!(plan.tls_start % effective, 0, "memsz {memsz} align {align}");
            assert_eq!(plan.tp_offset, plan.tls_start + memsz);
            assert!(
                plan.tp_offset + TCB <= plan.alloc_size,
                "memsz {memsz} align {align}: the TCB runs past the allocation",
            );
            assert_eq!(plan.alloc_size % GRANULE, 0, "memsz {memsz} align {align}");
        }
    }
}

#[test]
fn a_size_no_allocation_can_hold_has_no_plan() {
    assert_eq!(tls::plan(usize::MAX, 8, TCB, DTV, GRANULE), None);
    assert_eq!(tls::plan(usize::MAX - TCB, 8, TCB, DTV, GRANULE), None);
    assert_eq!(tls::plan(usize::MAX - GRANULE, GRANULE, TCB, DTV, GRANULE), None);
}

/// `!(align - 1)` is only a mask for a power of two, and a mask that is not a
/// mask can place the TLS data anywhere — the DTV included.
#[test]
fn an_alignment_that_is_not_a_power_of_two_has_no_plan() {
    for align in [3usize, 5, 6, 100, usize::MAX] {
        assert_eq!(tls::plan(64, align, TCB, DTV, GRANULE), None, "align {align}");
    }
}

#[test]
fn modules_are_placed_in_order_with_the_first_at_zero() {
    let (base, cursor) = tls::place_module(0, 0x48).unwrap();
    assert_eq!((base, cursor), (0, 0x48));
    let (base, cursor) = tls::place_module(cursor, 0x10).unwrap();
    assert_eq!((base, cursor), (0x50, 0x60));
    let (base, cursor) = tls::place_module(cursor, 0).unwrap();
    assert_eq!((base, cursor), (0x60, 0x60));
    assert_eq!(tls::place_module(0x60, usize::MAX), None);
}

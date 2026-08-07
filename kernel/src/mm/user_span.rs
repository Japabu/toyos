//! The arithmetic every user-memory access rests on: the user/kernel bound, the
//! alignment a type needs, and whether a value the kernel is about to
//! dereference lies inside one mapping.
//!
//! Pure, and deliberately free of any `crate::` reference: `kernel-span/`
//! compiles this file into a host crate, so the boundary table below runs in
//! milliseconds rather than in a boot.

/// One past the highest address userland can name.
///
/// The hardware's canonical split with 48-bit linear addresses: PML4 indices
/// 0..255 are the user half and 256..511 are the kernel's. An address at or
/// above this is either a kernel address or non-canonical, and neither is
/// something a process may hand the kernel to dereference on its behalf.
pub const USER_TOP: u64 = 0x0000_8000_0000_0000;

/// The kernel's only user page size, which is also the granularity a
/// translation answers at.
pub const PAGE_2M: u64 = 2 * 1024 * 1024;

pub fn is_user_addr(addr: u64) -> bool {
    addr < USER_TOP
}

/// Whether `[ptr, ptr + len)` is entirely in the user half.
///
/// Also the bound `sys_mmap` applies to a range it will install rather than
/// dereference: the hardware split is the same either way, and a second copy of
/// the constant is a second thing to get wrong.
pub fn in_user_half(ptr: u64, len: u64) -> bool {
    match ptr.checked_add(len) {
        Some(end) => end <= USER_TOP,
        None => false,
    }
}

/// Whether the kernel may read or write a `size`-byte value of alignment
/// `align` at `ptr` through one translation.
///
/// A translation answers for the 2 MiB page holding its first byte; the
/// physical page after that one belongs to whoever the PMM last gave it to. So
/// an object crossing the boundary is refused rather than served out of the
/// page it started in — userland can always move a value that is at most 48
/// bytes long, and the alternative is a write into another process's memory.
pub fn is_user_object(ptr: u64, size: u64, align: u64) -> bool {
    if size == 0 || !align.is_power_of_two() {
        return false;
    }
    if ptr % align != 0 || !in_user_half(ptr, size) {
        return false;
    }
    ptr & !(PAGE_2M - 1) == (ptr + size - 1) & !(PAGE_2M - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `UserSafe` type, by the shape that decides this: size and align.
    const TYPES: &[(&str, u64, u64)] = &[
        ("u32", 4, 4),
        ("u64", 8, 8),
        ("[u32; 2]", 8, 4),
        ("[u64; 2]", 16, 8),
        ("RawKeyEvent", 2, 1),
        ("MouseEvent", 6, 2),
        ("Stat", 24, 8),
        ("SpawnArgs", 48, 8),
    ];

    #[test]
    fn a_kernel_address_is_not_a_user_address() {
        assert!(!is_user_addr(USER_TOP));
        assert!(!is_user_addr(0xFFFF_8000_0000_0000));
        assert!(!is_user_addr(u64::MAX));
        assert!(is_user_addr(USER_TOP - 1));
        assert!(is_user_addr(0));
    }

    #[test]
    fn a_range_ending_past_the_bound_is_refused_and_a_wrapping_one_too() {
        assert!(in_user_half(USER_TOP - 8, 8));
        assert!(!in_user_half(USER_TOP - 8, 9));
        assert!(!in_user_half(USER_TOP, 1));
        assert!(!in_user_half(u64::MAX, 1));
        assert!(!in_user_half(u64::MAX - 4, 8));
        assert!(!in_user_half(1 << 63, 1 << 63));
    }

    /// The straddle, for every type, at every offset that can produce one.
    #[test]
    fn no_type_may_cross_a_2_mib_boundary() {
        for &(name, size, align) in TYPES {
            for last in 1..size {
                let ptr = 4 * PAGE_2M - last;
                if ptr % align != 0 {
                    continue;
                }
                assert!(
                    !is_user_object(ptr, size, align),
                    "{name} at {ptr:#x} has {last} bytes below the boundary and {} above",
                    size - last
                );
            }
            assert!(is_user_object(4 * PAGE_2M - size, size, align), "{name} ending at a boundary");
            assert!(is_user_object(4 * PAGE_2M, size, align), "{name} starting at a boundary");
        }
    }

    #[test]
    fn an_object_is_refused_for_its_alignment_before_anything_else() {
        for &(name, size, align) in TYPES {
            for off in 1..align {
                assert!(!is_user_object(PAGE_2M + off, size, align), "{name} at +{off}");
            }
            assert!(is_user_object(PAGE_2M, size, align), "{name} at a page start");
        }
    }

    #[test]
    fn an_object_may_not_end_past_the_bound() {
        for &(name, size, align) in TYPES {
            assert!(is_user_object(USER_TOP - size, size, align), "{name} ending at the bound");
            assert!(!is_user_object(USER_TOP, size, align), "{name} at the bound");
            assert!(!is_user_object(USER_TOP + PAGE_2M, size, align), "{name} above the bound");
            assert!(!is_user_object(u64::MAX - size + 1, size, align), "{name} wrapping");
        }
    }

    #[test]
    fn a_zero_sized_object_is_nothing_to_dereference() {
        assert!(!is_user_object(PAGE_2M, 0, 1));
        assert!(in_user_half(PAGE_2M, 0));
    }
}

//! Running out of virtual address space must be an error return.
//!
//! `find_gap` returning `None` was an `.expect` on three paths in `spawn` and
//! two in `sys_dlopen` — a kernel panic in syscall context, reachable in
//! principle from a process that just keeps mapping. It is not reachable in
//! practice: the arena is ~1015 GB and every region in it costs at worst twice
//! its size in physical memory, so the PMM refuses hundreds of gigabytes
//! before `find_gap` does. `vma::ALLOC_FLOOR`'s `test-tiny-va` arm is the
//! actuator; the code under test is the shipped code.
//!
//! Runs only under that feature — `RUST_SKIP` keeps it out of the shared boot,
//! where the loop below would never terminate.

use toyos_abi::syscall::{self, MmapFlags, MmapProt};

/// More mappings than a 256 MiB arena can hold by a wide margin. Reaching it
/// means the floor did not move, which is a red result and not a hang.
const CAP: usize = 4096;

/// The arena is 256 MiB and each 4 KiB request costs `align_up_2m(4096)` plus
/// one 2 MiB guard, so ~64 mappings fill it — fewer once the process's own
/// heap has taken some. The band is what separates "address space ran out"
/// from "memory ran out": this guest has 4 GB, so exhausting *RAM* at 2 MiB a
/// mapping would take ~2000, well outside it.
const EXPECT: core::ops::RangeInclusive<usize> = 4..=128;

fn main() {
    // Everything that can allocate happens before the arena is full: past that
    // point the process's own heap cannot grow either, so a `println!` that
    // needs a new mapping would fail as part of the thing under test.
    println!("va_exhaustion: filling the arena");

    let mut addrs = [core::ptr::null_mut::<u8>(); CAP];
    let mut n = 0usize;
    while n < CAP {
        let p = unsafe {
            syscall::mmap(
                core::ptr::null_mut(),
                4096,
                MmapProt::READ | MmapProt::WRITE,
                MmapFlags::ANONYMOUS | MmapFlags::PRIVATE,
            )
        };
        if p.is_null() {
            break;
        }
        // Touch it: a mapping the kernel reports and cannot back is worse than
        // one it refuses, and this is the only place that distinction shows.
        unsafe { p.write_volatile(0xA5) };
        addrs[n] = p;
        n += 1;
    }

    // The kernel is still answering syscalls, which is the whole assertion —
    // and `dlopen` is the path whose refusal used to be an `.expect`. It has to
    // come while the arena is still full, and it must not allocate here: the
    // path is a byte-string literal for that reason.
    let dlopen_full = syscall::dl_open(b"/lib/libtls_lib.so");

    // Give the address space back before doing anything that allocates again.
    for &p in &addrs[..n] {
        unsafe { syscall::munmap(p, 4096) }.expect("munmap a region mmap just returned");
    }

    assert!(n < CAP, "the arena did not exhaust in {CAP} mappings — is test-tiny-va on?");
    assert!(
        EXPECT.contains(&n),
        "{n} mappings fit: that is not the 256 MiB arena running out, it is something else"
    );
    assert!(
        dlopen_full.is_err(),
        "dlopen succeeded with no address space left to map the image into"
    );

    // And the refusal was the arena, not the library: the same call works once
    // the space is back. This is what makes the assertion above mean "refused",
    // rather than "this .so never loads".
    syscall::dl_open(b"/lib/libtls_lib.so")
        .expect("dlopen failed after the arena was released");

    println!("va exhausted after {n} mappings, dlopen refused, kernel intact");
}

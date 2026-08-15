use toyos_abi::syscall::{mmap, munmap, MmapProt, MmapFlags};
use std::collections::HashSet;

fn main() {
    let page_2m: usize = 2 * 1024 * 1024;

    // No mapping may overlap another, or collide with a demand-paged ELF page.
    let mut regions: Vec<(*mut u8, usize)> = Vec::new();
    let mut seen_pages: HashSet<usize> = HashSet::new();

    for i in 0..64 {
        let size = page_2m * (1 + (i % 4)); // 2MB, 4MB, 6MB, 8MB
        let ptr = unsafe {
            mmap(
                core::ptr::null_mut(),
                size,
                MmapProt::READ | MmapProt::WRITE,
                MmapFlags::ANONYMOUS | MmapFlags::PRIVATE,
            )
        };
        assert!(!ptr.is_null(), "mmap #{i} failed (size={size})");

        let base = ptr as usize;
        assert!(base % page_2m == 0, "mmap #{i} returned unaligned address {base:#x}");
        for offset in (0..size).step_by(page_2m) {
            let page = base + offset;
            assert!(
                seen_pages.insert(page),
                "mmap #{i}: page {page:#x} overlaps a previous allocation!"
            );
        }

        let tag = (i & 0xFF) as u8;
        unsafe { ptr.write(tag) };
        unsafe { ptr.add(size - 1).write(tag) };

        regions.push((ptr, size));
    }

    for (i, &(ptr, size)) in regions.iter().enumerate() {
        let tag = (i & 0xFF) as u8;
        let first = unsafe { ptr.read() };
        let last = unsafe { ptr.add(size - 1).read() };
        assert_eq!(first, tag, "region #{i}: first byte corrupted ({first:#x} != {tag:#x})");
        assert_eq!(last, tag, "region #{i}: last byte corrupted ({last:#x} != {tag:#x})");
    }

    for (ptr, size) in regions {
        unsafe { munmap(ptr, size) }.expect("munmap failed");
    }

    // A FIXED mapping is placed at exactly the address asked for, so a request
    // the 2 MiB page granularity cannot express is refused rather than rounded:
    // rounding down under-maps the range the caller is told it got, and
    // rounding up maps past the end of the backing allocation.
    let scratch = unsafe {
        mmap(core::ptr::null_mut(), page_2m, MmapProt::READ | MmapProt::WRITE,
             MmapFlags::ANONYMOUS | MmapFlags::PRIVATE)
    };
    assert!(!scratch.is_null(), "scratch mmap failed");
    unsafe { munmap(scratch, page_2m) }.expect("scratch munmap failed");

    for skew in [0x1000usize, page_2m - 0x1000, 1] {
        let misaligned = scratch.wrapping_add(skew);
        let got = unsafe {
            mmap(misaligned, page_2m, MmapProt::READ | MmapProt::WRITE,
                 MmapFlags::ANONYMOUS | MmapFlags::PRIVATE | MmapFlags::FIXED)
        };
        assert!(got.is_null(), "FIXED accepted a misaligned address {misaligned:p}");
    }

    let fixed = unsafe {
        mmap(scratch, page_2m, MmapProt::READ | MmapProt::WRITE,
             MmapFlags::ANONYMOUS | MmapFlags::PRIVATE | MmapFlags::FIXED)
    };
    assert_eq!(fixed, scratch, "FIXED did not honour an aligned address");
    unsafe { fixed.write(0x5A) };
    unsafe { fixed.add(page_2m - 1).write(0xA5) };
    assert_eq!(unsafe { fixed.add(page_2m - 1).read() }, 0xA5);

    // The mapping stays live from here to the end, because everything below is
    // about what the rest of the address space is allowed to do around it.
    //
    // `scratch` was freed, so its range is the one the placement search hands
    // out next — and this FIXED mapping is living in it. The kernel keeps two
    // ledgers of that range, the process's mmap list and the address space's
    // regions, and the FIXED path wrote only the first: the placement search
    // reads the second, handed the range straight back, and mapping over a
    // present entry asserted. Three ordinary syscalls, from any C program that
    // passes `MAP_FIXED`, took the machine down.
    let beside = unsafe {
        mmap(core::ptr::null_mut(), page_2m, MmapProt::READ | MmapProt::WRITE,
             MmapFlags::ANONYMOUS | MmapFlags::PRIVATE)
    };
    assert!(!beside.is_null(), "an anonymous mmap beside a live FIXED mapping failed");
    let (f, b) = (fixed as usize, beside as usize);
    assert!(
        b + page_2m <= f || f + page_2m <= b,
        "an anonymous mmap at {beside:p} was placed over the live FIXED mapping at {fixed:p}"
    );
    unsafe { beside.write(0x11) };
    assert_eq!(
        unsafe { fixed.read() }, 0x5A,
        "the FIXED mapping's first byte was overwritten by a later anonymous mmap"
    );
    assert_eq!(
        unsafe { fixed.add(page_2m - 1).read() }, 0xA5,
        "the FIXED mapping's last byte was overwritten by a later anonymous mmap"
    );

    // A FIXED request the kernel cannot honour whole is refused, and refused
    // before it has touched anything: it tracks a mapping as one region and has
    // no way to take half of one away.
    let straddle = beside.wrapping_sub(page_2m);
    let got = unsafe {
        mmap(straddle, 2 * page_2m, MmapProt::READ | MmapProt::WRITE,
             MmapFlags::ANONYMOUS | MmapFlags::PRIVATE | MmapFlags::FIXED)
    };
    assert!(got.is_null(), "FIXED at {straddle:p} took half of the live mapping at {beside:p}");
    let got = unsafe {
        mmap(beside, 2 * page_2m, MmapProt::READ | MmapProt::WRITE,
             MmapFlags::ANONYMOUS | MmapFlags::PRIVATE | MmapFlags::FIXED)
    };
    assert!(got.is_null(), "FIXED grew a live mapping at {beside:p} instead of refusing");
    assert_eq!(
        unsafe { beside.read() }, 0x11,
        "a refused FIXED request unmapped the range it was refused for"
    );

    // Exactly one whole mapping, though, it replaces: the address keeps its
    // meaning and changes what it names. Replaces, not shadows — one munmap
    // frees the range and the second finds nothing, so there is one record of
    // it and not two.
    let replaced = unsafe {
        mmap(beside, page_2m, MmapProt::READ | MmapProt::WRITE,
             MmapFlags::ANONYMOUS | MmapFlags::PRIVATE | MmapFlags::FIXED)
    };
    assert_eq!(replaced, beside, "FIXED refused to replace one whole mapping of its own size");
    assert_eq!(
        unsafe { replaced.read() }, 0,
        "the replacement handed back the page it replaced"
    );
    unsafe { munmap(replaced, page_2m) }.expect("the replaced mapping could not be freed");
    assert!(
        unsafe { munmap(replaced, page_2m) }.is_err(),
        "the replaced mapping was recorded twice, so freeing it once left a live record"
    );

    // The same holds when what is replaced is itself FIXED.
    let again = unsafe {
        mmap(fixed, page_2m, MmapProt::READ | MmapProt::WRITE,
             MmapFlags::ANONYMOUS | MmapFlags::PRIVATE | MmapFlags::FIXED)
    };
    assert_eq!(again, fixed, "a FIXED mapping could not be replaced by a FIXED mapping");
    assert_eq!(unsafe { again.read() }, 0, "the replacement handed back the page it replaced");

    // And freeing it gives the range back to the placement search, which is the
    // other half of the ledgers agreeing: the same request that was refused
    // this range while it was live is handed it now.
    unsafe { munmap(again, page_2m) }.expect("FIXED region could not be freed");
    let reused = unsafe {
        mmap(core::ptr::null_mut(), page_2m, MmapProt::READ | MmapProt::WRITE,
             MmapFlags::ANONYMOUS | MmapFlags::PRIVATE)
    };
    assert_eq!(
        reused, fixed,
        "an unmapped FIXED range was not given back to the allocator: it placed {reused:p} \
         below the freed {fixed:p}"
    );
    unsafe { munmap(reused, page_2m) }.expect("the reused region could not be freed");

    println!("all mmap stress tests passed (64 regions, no overlaps)");
}

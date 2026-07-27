use toyos_abi::syscall::{mmap, munmap, MmapProt, MmapFlags};
use std::collections::HashSet;

fn main() {
    let page_2m: usize = 2 * 1024 * 1024;

    // Stress test: allocate many regions and verify no overlaps.
    // This catches the old bug where the bump allocator could hand out addresses
    // that collide with demand-paged ELF pages.
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

        // Verify this region doesn't overlap any previous allocation
        let base = ptr as usize;
        assert!(base % page_2m == 0, "mmap #{i} returned unaligned address {base:#x}");
        for offset in (0..size).step_by(page_2m) {
            let page = base + offset;
            assert!(
                seen_pages.insert(page),
                "mmap #{i}: page {page:#x} overlaps a previous allocation!"
            );
        }

        // Write a pattern to verify the pages are usable
        let tag = (i & 0xFF) as u8;
        unsafe { ptr.write(tag) };
        unsafe { ptr.add(size - 1).write(tag) };

        regions.push((ptr, size));
    }

    // Verify patterns survived (no cross-mapping corruption)
    for (i, &(ptr, size)) in regions.iter().enumerate() {
        let tag = (i & 0xFF) as u8;
        let first = unsafe { ptr.read() };
        let last = unsafe { ptr.add(size - 1).read() };
        assert_eq!(first, tag, "region #{i}: first byte corrupted ({first:#x} != {tag:#x})");
        assert_eq!(last, tag, "region #{i}: last byte corrupted ({last:#x} != {tag:#x})");
    }

    // Free all
    for (ptr, size) in regions {
        unsafe { munmap(ptr, size) }.expect("munmap failed");
    }

    println!("all mmap stress tests passed (64 regions, no overlaps)");
}

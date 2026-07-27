use std::collections::BTreeMap;

fn main() {
    test_slab_all_size_classes();
    test_slab_reuse();
    test_buddy_page_allocs();
    test_buddy_large_allocs();
    test_buddy_coalescing();
    test_mixed_workload();
    test_alignment();
    test_many_small_allocs();
    test_grow_and_shrink();
    test_fragmentation_resistance();
    test_memory_stats();
    test_oom_graceful();
    println!("all allocator stress tests passed");
}

/// Test all 9 slab size classes (8, 16, 32, 64, 128, 256, 512, 1024, 2048).
fn test_slab_all_size_classes() {
    // Allocate vectors of each size class and verify contents
    for &size in &[1, 2, 4, 8, 9, 16, 17, 32, 33, 64, 65, 128, 129, 256, 257, 512, 513, 1024, 1025, 2048] {
        let v: Vec<u8> = vec![0xAA; size];
        assert_eq!(v.len(), size);
        assert!(v.iter().all(|&b| b == 0xAA), "size {size}: corrupted data");
    }

    // Verify different size classes don't alias
    let a = vec![0x11u8; 8];
    let b = vec![0x22u8; 16];
    let c = vec![0x33u8; 32];
    let d = vec![0x44u8; 64];
    assert!(a.iter().all(|&x| x == 0x11));
    assert!(b.iter().all(|&x| x == 0x22));
    assert!(c.iter().all(|&x| x == 0x33));
    assert!(d.iter().all(|&x| x == 0x44));
    println!("  slab size classes: ok");
}

/// Verify freed memory is reused: alloc, free, alloc again should reuse the
/// same address range (not grow the heap).
fn test_slab_reuse() {
    let mut ptrs = Vec::new();

    // Allocate 100 boxes, record address range
    for i in 0u64..100 {
        let b = Box::new(i);
        ptrs.push(Box::into_raw(b) as usize);
    }
    let old_min = *ptrs.iter().min().unwrap();
    let old_max = *ptrs.iter().max().unwrap();

    // Free all
    for &ptr in &ptrs {
        drop(unsafe { Box::from_raw(ptr as *mut u64) });
    }

    // Allocate again — should reuse freed memory (not grow heap)
    let mut ptrs2 = Vec::new();
    for i in 0u64..100 {
        let b = Box::new(i);
        ptrs2.push(Box::into_raw(b) as usize);
    }
    let new_min = *ptrs2.iter().min().unwrap();
    let new_max = *ptrs2.iter().max().unwrap();

    // New allocations should fall within (or near) the original address range,
    // proving freed memory was reused rather than the heap growing.
    let in_range = ptrs2.iter().filter(|&&p| p >= old_min && p <= old_max + 8).count();
    assert!(in_range > 50, "freed memory not reused: only {in_range}/100 in original range \
        (old={old_min:#x}..{old_max:#x}, new={new_min:#x}..{new_max:#x})");

    // Clean up
    for &ptr in &ptrs2 {
        drop(unsafe { Box::from_raw(ptr as *mut u64) });
    }
    println!("  free-list reuse: ok ({in_range}/100 in original range)");
}

/// Test buddy allocator with page-sized allocations (>2048 bytes).
fn test_buddy_page_allocs() {
    // 4KB allocations (order 0)
    let pages: Vec<Vec<u8>> = (0..16).map(|i| vec![i as u8; 4096]).collect();
    for (i, page) in pages.iter().enumerate() {
        assert_eq!(page.len(), 4096);
        assert!(page.iter().all(|&b| b == i as u8), "page {i} corrupted");
    }

    // 8KB allocations (order 1)
    let pages2: Vec<Vec<u8>> = (0..8).map(|i| vec![(i + 100) as u8; 8192]).collect();
    for (i, page) in pages2.iter().enumerate() {
        assert!(page.iter().all(|&b| b == (i + 100) as u8));
    }
    println!("  buddy page allocs: ok");
}

/// Test large buddy allocations (multiple pages).
fn test_buddy_large_allocs() {
    // 64KB (order 4 = 16 pages)
    let big = vec![0xBBu8; 64 * 1024];
    assert!(big.iter().all(|&b| b == 0xBB));

    // 1MB (order 8 = 256 pages)
    let huge = vec![0xCCu8; 1024 * 1024];
    assert!(huge.iter().all(|&b| b == 0xCC));

    // 4MB (order 10 = 1024 pages)
    let very_large = vec![0xDDu8; 4 * 1024 * 1024];
    assert!(very_large.iter().all(|&b| b == 0xDD));

    drop(big);
    drop(huge);
    drop(very_large);
    println!("  buddy large allocs: ok");
}

/// Test that freed buddy blocks coalesce back into larger blocks.
/// Allocate two adjacent buddy blocks, free them, then allocate a block
/// of double the size — should succeed without fragmentation.
fn test_buddy_coalescing() {
    // Allocate 8 x 1MB blocks
    let mut blocks: Vec<Vec<u8>> = (0..8).map(|i| vec![i as u8; 1024 * 1024]).collect();

    // Verify each block
    for (i, block) in blocks.iter().enumerate() {
        assert!(block.iter().all(|&b| b == i as u8));
    }

    // Free all blocks
    blocks.clear();

    // Now allocate a single 8MB block — requires coalescing
    let big = vec![0xEEu8; 8 * 1024 * 1024];
    assert!(big.iter().all(|&b| b == 0xEE));
    drop(big);
    println!("  buddy coalescing: ok");
}

/// Interleaved small and large allocations.
fn test_mixed_workload() {
    let mut items: Vec<Box<dyn std::any::Any>> = Vec::new();

    for i in 0..200 {
        match i % 4 {
            0 => items.push(Box::new(vec![0u8; 16])),      // slab: 16 bytes
            1 => items.push(Box::new(vec![0u8; 256])),     // slab: 256 bytes
            2 => items.push(Box::new(vec![0u8; 4096])),    // buddy: 1 page
            3 => items.push(Box::new(vec![0u8; 32768])),   // buddy: 8 pages
            _ => unreachable!(),
        }

        // Drop every 3rd item to create holes
        if i % 3 == 0 && items.len() > 1 {
            items.remove(0);
        }
    }

    // Drop everything
    items.clear();

    // Verify allocator still works after mixed workload
    let check = vec![0xFFu8; 65536];
    assert!(check.iter().all(|&b| b == 0xFF));
    println!("  mixed workload: ok");
}

/// Verify alignment requirements are satisfied.
fn test_alignment() {
    // u64 requires 8-byte alignment
    for _ in 0..100 {
        let b = Box::new(0u64);
        let ptr = &*b as *const u64 as usize;
        assert_eq!(ptr % 8, 0, "u64 not 8-byte aligned: {ptr:#x}");
    }

    // u128 requires 16-byte alignment
    for _ in 0..100 {
        let b = Box::new(0u128);
        let ptr = &*b as *const u128 as usize;
        assert_eq!(ptr % 16, 0, "u128 not 16-byte aligned: {ptr:#x}");
    }

    // Page-sized allocation should be page-aligned
    let layout = std::alloc::Layout::from_size_align(4096, 4096).unwrap();
    let ptr = unsafe { std::alloc::alloc(layout) };
    assert!(!ptr.is_null());
    assert_eq!(ptr as usize % 4096, 0, "page alloc not page-aligned: {:#x}", ptr as usize);
    unsafe { std::alloc::dealloc(ptr, layout); }

    println!("  alignment: ok");
}

/// Stress test: many small allocations simultaneously.
fn test_many_small_allocs() {
    // Hold 10000 allocations simultaneously
    let mut vecs: Vec<Vec<u8>> = Vec::with_capacity(10_000);
    for i in 0u16..10_000 {
        let size = (i % 64 + 1) as usize;
        let mut v = vec![0u8; size];
        // Write a pattern
        for (j, byte) in v.iter_mut().enumerate() {
            *byte = ((i as usize + j) & 0xFF) as u8;
        }
        vecs.push(v);
    }

    // Verify all patterns intact (no corruption from adjacent allocations)
    for (i, v) in vecs.iter().enumerate() {
        let size = (i as u16 % 64 + 1) as usize;
        assert_eq!(v.len(), size, "vec {i} wrong len");
        for (j, &byte) in v.iter().enumerate() {
            let expected = ((i + j) & 0xFF) as u8;
            assert_eq!(byte, expected, "vec {i} byte {j}: expected {expected:#x}, got {byte:#x}");
        }
    }

    drop(vecs);
    println!("  many small allocs: ok (10000)");
}

/// Grow a collection then shrink it, verify memory reclaimed.
fn test_grow_and_shrink() {
    let mut map = BTreeMap::new();

    // Grow: insert 5000 entries
    for i in 0..5000u32 {
        map.insert(i, vec![0u8; 64]);
    }
    assert_eq!(map.len(), 5000);

    // Shrink: remove all
    map.clear();

    // Grow again: should reuse freed memory
    for i in 0..5000u32 {
        map.insert(i, vec![0u8; 64]);
    }
    assert_eq!(map.len(), 5000);

    // Grow further
    for i in 5000..10000u32 {
        map.insert(i, vec![0u8; 64]);
    }
    assert_eq!(map.len(), 10000);

    drop(map);
    println!("  grow and shrink: ok");
}

/// Test that the allocator handles fragmentation gracefully.
/// Allocate alternating sizes, free every other one, then try to
/// allocate in the freed gaps.
fn test_fragmentation_resistance() {
    let mut small: Vec<Vec<u8>> = Vec::new();
    let mut large: Vec<Vec<u8>> = Vec::new();

    // Interleave: 500 small (32B) and 500 large (4KB)
    for i in 0..500 {
        small.push(vec![(i & 0xFF) as u8; 32]);
        large.push(vec![((i + 128) & 0xFF) as u8; 4096]);
    }

    // Free all large allocations (creating holes in buddy free lists)
    large.clear();

    // Allocate many medium-sized blocks in the freed space
    let mut medium: Vec<Vec<u8>> = Vec::new();
    for _ in 0..500 {
        medium.push(vec![0xAA; 2048]);
    }

    // Verify small allocations weren't corrupted
    for (i, v) in small.iter().enumerate() {
        assert!(v.iter().all(|&b| b == (i & 0xFF) as u8), "small {i} corrupted after realloc");
    }

    drop(small);
    drop(medium);
    println!("  fragmentation resistance: ok");
}

/// Verify sysinfo reports reasonable memory stats.
fn test_memory_stats() {
    let mut buf = [0u8; 4096];
    let n = toyos_abi::syscall::sysinfo(&mut buf);
    assert!(n >= 48, "sysinfo returned only {n} bytes, need at least 48");

    // Binary header: total_mem(u64), used_mem(u64), ...
    let total = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let used = u64::from_le_bytes(buf[8..16].try_into().unwrap());

    // QEMU is configured with 8GB (test harness uses 4GB)
    // Total should be in a reasonable range (2-9 GB)
    let total_gb = total / (1024 * 1024 * 1024);
    assert!(total_gb >= 2 && total_gb <= 9, "mem_total={total} ({total_gb} GB) out of range");

    // Used should be less than total and non-zero
    assert!(used > 0, "mem_used=0 — allocator not tracking");
    assert!(used < total, "mem_used={used} >= mem_total={total}");

    println!("  memory stats: ok (total={total_gb}GB, used={}MB)", used / (1024 * 1024));
}

/// Verify that OOM returns an error instead of crashing the kernel.
fn test_oom_graceful() {
    // Try to allocate 16GB — way more than available RAM.
    // Must use try_reserve to get the error gracefully.
    let mut v: Vec<u8> = Vec::new();
    let result = v.try_reserve(16 * 1024 * 1024 * 1024);
    assert!(result.is_err(), "16GB allocation should fail");

    // Verify allocator still works after OOM
    let check = vec![0xAAu8; 4096];
    assert!(check.iter().all(|&b| b == 0xAA));

    // Try progressively larger allocations to find the limit
    let mut last_ok = 0usize;
    for shift in 20..34 { // 1MB .. 8GB
        let size = 1usize << shift;
        let mut v2: Vec<u8> = Vec::new();
        if v2.try_reserve(size).is_ok() {
            last_ok = size;
            drop(v2);
        } else {
            break;
        }
    }

    // Verify allocator still works after hitting the limit
    let check2 = vec![0xBBu8; 65536];
    assert!(check2.iter().all(|&b| b == 0xBB));

    println!("  oom graceful: ok (largest succeeded={}MB)", last_ok / (1024 * 1024));
}

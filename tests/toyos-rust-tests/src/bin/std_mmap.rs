use toyos_abi::syscall::{mmap, munmap, MmapProt, MmapFlags};

fn main() {
    // Allocate a 2MB anonymous region
    let size: usize = 2 * 1024 * 1024;
    let ptr = mmap(size, MmapProt::READ | MmapProt::WRITE, MmapFlags::ANONYMOUS | MmapFlags::PRIVATE);
    assert!(!ptr.is_null(), "mmap failed");

    // Write a pattern
    for i in 0..size {
        unsafe { ptr.add(i).write(0xAB) };
    }

    // Read back and verify
    for i in 0..size {
        let val = unsafe { ptr.add(i).read() };
        assert_eq!(val, 0xAB, "mismatch at offset {i}: expected 0xAB, got {val:#x}");
    }

    // Unmap
    munmap(ptr, size).expect("munmap failed");

    println!("all mmap tests passed");
}

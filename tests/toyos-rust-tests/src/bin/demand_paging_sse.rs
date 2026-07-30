/// Demand paging must preserve user SSE/XMM state: the Rust page fault handler
/// is free to clobber XMM0-XMM7 per System V, so `page_fault_entry` has to save
/// and restore them.
///
/// The data MUST live in a writable segment (.data, not .rodata) so the fault
/// takes the copy path (private page + memcpy), which uses XMM internally.
/// Read-only data is mapped straight from the page cache and would not clobber
/// anything.

use core::arch::asm;

// Mutable so it lands in .data, not .rodata: the fault handler must take the
// allocate-and-memcpy path, which is the one that uses SSE.
static mut DATA: [u64; 16384] = {
    let mut arr = [0u64; 16384];
    let mut i = 0;
    while i < 16384 {
        arr[i] = i as u64 * 7 + 3;
        i += 1;
    }
    arr
};

fn main() {
    // Test 1: XMM0 across demand page faults on writable pages
    for page in 0..8 {
        let pattern: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let mut result: u64 = 0;
        let ptr = unsafe { &DATA[page * 512] as *const u64 }; // 512 * 8 = 4096 = 1 page

        unsafe {
            asm!(
                "movq xmm0, [{pat}]",
                "mov {scratch}, [{data}]",
                "movq [{out}], xmm0",
                pat = in(reg) &pattern,
                data = in(reg) ptr,
                out = in(reg) &mut result,
                scratch = out(reg) _,
                out("xmm0") _,
            );
        }
        assert_eq!(
            result, pattern,
            "XMM0 corrupted on page {}: expected {:#018x}, got {:#018x}",
            page, pattern, result
        );
    }
    println!("PASS: XMM0 preserved across 8 demand page faults (writable pages)");

    // Test 2: Multiple XMM registers simultaneously
    {
        let p0: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let p1: u64 = 0x1234_5678_9ABC_DEF0;
        let p2: u64 = 0xFEDC_BA98_7654_3210;
        let p3: u64 = 0x0011_2233_4455_6677;
        let mut r0: u64 = 0;
        let mut r1: u64 = 0;
        let mut r2: u64 = 0;
        let mut r3: u64 = 0;
        let ptr = unsafe { &DATA[8 * 512] as *const u64 };

        unsafe {
            asm!(
                "movq xmm0, [{p0}]",
                "movq xmm1, [{p1}]",
                "movq xmm2, [{p2}]",
                "movq xmm3, [{p3}]",
                "mov {scratch}, [{data}]",
                "movq [{r0}], xmm0",
                "movq [{r1}], xmm1",
                "movq [{r2}], xmm2",
                "movq [{r3}], xmm3",
                p0 = in(reg) &p0,
                p1 = in(reg) &p1,
                p2 = in(reg) &p2,
                p3 = in(reg) &p3,
                data = in(reg) ptr,
                r0 = in(reg) &mut r0,
                r1 = in(reg) &mut r1,
                r2 = in(reg) &mut r2,
                r3 = in(reg) &mut r3,
                scratch = out(reg) _,
                out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            );
        }
        assert_eq!(r0, p0, "XMM0 corrupted: expected {:#018x}, got {:#018x}", p0, r0);
        assert_eq!(r1, p1, "XMM1 corrupted: expected {:#018x}, got {:#018x}", p1, r1);
        assert_eq!(r2, p2, "XMM2 corrupted: expected {:#018x}, got {:#018x}", p2, r2);
        assert_eq!(r3, p3, "XMM3 corrupted: expected {:#018x}, got {:#018x}", p3, r3);
    }
    println!("PASS: XMM0-XMM3 preserved across demand page fault (writable page)");

    // Test 3: More pages with different pattern
    for page in 9..16 {
        let pattern: u64 = 0x5555_6666_7777_8888;
        let mut result: u64 = 0;
        let ptr = unsafe { &DATA[page * 512] as *const u64 };

        unsafe {
            asm!(
                "movq xmm0, [{pat}]",
                "mov {scratch}, [{data}]",
                "movq [{out}], xmm0",
                pat = in(reg) &pattern,
                data = in(reg) ptr,
                out = in(reg) &mut result,
                scratch = out(reg) _,
                out("xmm0") _,
            );
        }
        assert_eq!(
            result, pattern,
            "XMM0 corrupted on page {}: expected {:#018x}, got {:#018x}",
            page, pattern, result
        );
    }
    println!("PASS: XMM0 preserved across additional writable page faults");

    println!("all demand paging SSE tests passed");
}

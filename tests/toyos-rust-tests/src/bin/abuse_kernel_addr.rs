//! Two syscalls take an address from userland and hand it to
//! `AddressSpace::translate`, which walked any PML4 index. A user address space
//! shallow-copies the kernel's PML4 half, so a kernel address resolved to a
//! present, writable 2 MiB leaf of the direct map.
//!
//! `SYS_DLOPEN`'s third argument was the one the syscall layer never validated
//! at all — sixteen bytes of arbitrary kernel memory, written by any process
//! that can call `dlopen`. `SYS_FUTEX_WAIT`'s word is the other: it was
//! guarded, but by an expression at the dispatch arm whose value was thrown
//! away, in a different file from the `pub fn` it protected. Both bounds now
//! live where they cannot be forgotten — in `translate` itself, and in
//! `futex_wait`'s signature.
//!
//! **The verdict is not the assertion.** A kernel that still made that write
//! would return the same error to a userland that cannot read a byte of the
//! kernel's address space to notice. So the kernel keeps sixteen bytes with a
//! known value and answers two questions about them
//! (`SYS_DEBUG` actions 10 and 11): where they are, and whether they still say
//! what it put there.

use toyos_abi::syscall::{self, MmapFlags, MmapProt, SyscallError, SYS_DLOPEN};

use toyos_abi::syscall::debug_action::{CANARY_ADDR, CANARY_CHANGED};

/// The one the TLS tests load, chosen because it exists in this image and has
/// an `init_array` — so a successful call has something to report.
const LIB: &[u8] = b"/lib/libtls_dlopen_lib.so";

const PAGE_2M: u64 = 2 * 1024 * 1024;

/// `dl_open` builds its own `init_info` on the stack, so the typed wrapper
/// cannot express the argument under test. Everything else about the call is
/// the ABI's own: number in rdi, arguments in rsi/rdx/r8/r9.
fn dlopen_raw(path: &[u8], init_out: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rdi") SYS_DLOPEN,
            in("rsi") path.as_ptr() as u64,
            in("rdx") path.len() as u64,
            in("r8") init_out,
            in("r9") 0u64,
            lateout("rax") ret,
            out("rcx") _,
            out("r11") _,
        );
    }
    ret
}

fn err(ret: u64) -> Option<SyscallError> {
    SyscallError::from_u64(ret)
}

fn canary_intact() -> bool {
    syscall::debug(CANARY_CHANGED) == 0
}

fn main() {
    let canary = syscall::debug(CANARY_ADDR);
    assert!(
        err(canary).is_none() && canary >= 0xFFFF_8000_0000_0000,
        "SYS_DEBUG 10 did not answer with a kernel address ({canary:#x}); \
         this kernel has no canary and the test would pass vacuously",
    );
    assert!(canary_intact(), "the canary was already changed before the test ran");

    // 1. The defect itself: a kernel address as `init_out`.
    let ret = dlopen_raw(LIB, canary);
    assert_eq!(err(ret), Some(SyscallError::BadAddress), "dlopen took a kernel address");
    assert!(canary_intact(), "dlopen wrote {canary:#x} — sixteen bytes of kernel memory");

    // 2. The rest of the kernel half, and the non-canonical hole above the user
    //    half, which `translate` reaches by the same walk.
    for &addr in &[0xFFFF_8000_0000_0000u64, 0x0000_8000_0000_0000, u64::MAX & !7] {
        let ret = dlopen_raw(LIB, addr);
        assert_eq!(err(ret), Some(SyscallError::BadAddress), "dlopen took {addr:#x}");
    }
    assert!(canary_intact(), "the canary changed while other kernel addresses were tried");

    // 3. A user address is still refused unless the kernel can write the whole
    //    16 bytes at it: misaligned, and straddling the 2 MiB page the one
    //    translation answers for.
    let region = unsafe {
        syscall::mmap(
            core::ptr::null_mut(),
            2 * PAGE_2M as usize,
            MmapProt::READ | MmapProt::WRITE,
            MmapFlags::ANONYMOUS | MmapFlags::PRIVATE,
        )
    };
    assert!(!region.is_null(), "mmap failed");
    let base = region as u64;
    let boundary = (base + PAGE_2M) & !(PAGE_2M - 1);
    for &addr in &[base + 4, boundary - 8] {
        let ret = dlopen_raw(LIB, addr);
        assert_eq!(err(ret), Some(SyscallError::BadAddress), "dlopen took {addr:#x}");
    }

    // 4. And the syscall still does its job, which is what none of the above
    //    may cost. Poisoned first, because a library with an empty init_array
    //    is written two zeros and that is not distinguishable from a write that
    //    never happened.
    const POISON: u64 = 0x_DEAD_BEEF_DEAD_BEEF;
    let out = boundary - 16;
    let words = out as *mut u64;
    unsafe {
        words.write_volatile(POISON);
        words.add(1).write_volatile(POISON);
    }
    let ret = dlopen_raw(LIB, out);
    assert!(err(ret).is_none(), "dlopen refused an init_out ending at a page boundary: {ret:#x}");
    let info = unsafe { [words.read_volatile(), words.add(1).read_volatile()] };
    assert!(
        info[0] != POISON && info[1] != POISON,
        "dlopen returned a handle and wrote nothing to init_out: {info:#x?}",
    );

    // 5. The futex word, which the *scheduler* dereferences on every wake check
    //    long after the syscall returned. A kernel address here is a read
    //    oracle: `futex_wait` blocks when the word equals `expected` and returns
    //    at once when it does not, so a caller that may name kernel memory can
    //    ask whether a kernel word holds a value — one bit per call, from
    //    timing alone. Neither answer is what an address it may not name gets.
    for &expected in &[0u32, 0x0F17_1E55, 0x1A55] {
        let ret = unsafe {
            syscall::futex_wait(canary as *const u32, expected, Some(50_000_000))
        };
        assert_eq!(
            err(ret),
            Some(SyscallError::BadAddress),
            "futex_wait answered {ret:#x} for a kernel address and expected={expected:#x}",
        );
    }
    let ret = unsafe { syscall::futex_wake(canary as *const u32, 1) };
    assert_eq!(err(ret), Some(SyscallError::BadAddress), "futex_wake took a kernel address");

    // The word must be one the scheduler can keep reading, so it is refused for
    // its alignment too — an unaligned one reads its tail out of the next
    // physical page.
    let word = base as *mut u32;
    unsafe { word.write_volatile(1) };
    let ret = unsafe { syscall::futex_wait((base + 2) as *const u32, 1, Some(0)) };
    assert_eq!(err(ret), Some(SyscallError::BadAddress), "futex_wait took an unaligned word");

    // And an honest futex still answers: the value does not match, so the call
    // returns rather than blocking.
    let ret = unsafe { syscall::futex_wait(word, 2, Some(50_000_000)) };
    assert!(err(ret).is_none(), "futex_wait refused a word of this process's own: {ret:#x}");
    let ret = unsafe { syscall::futex_wake(word, 1) };
    assert!(err(ret).is_none(), "futex_wake refused a word of this process's own: {ret:#x}");

    unsafe { syscall::munmap(region, 2 * PAGE_2M as usize) }.expect("munmap");
    assert!(canary_intact(), "the canary changed during the run");
    println!("dlopen and futex refuse an address they may not reach, and kernel memory is untouched");
}

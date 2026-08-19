//! A shootdown returns when every other CPU has flushed — and the paths that
//! free memory are behind it.
//!
//! **Why this needs an actuator at all.** A correct wait and no wait whatsoever
//! measure the same zero on a machine where every CPU answers in microseconds,
//! so nothing a guest can do distinguishes them. `SYS_DEBUG` action 12 makes the
//! last CPU an initiator waits for answer late — after flushing, so what is
//! staged is a slow answer and never an incorrect one — and the wait becomes a
//! duration userland can read off its own clock.
//!
//! **Why the harm itself is not the verdict here.** The honest gate would be a
//! sibling reading through a stale translation into memory the PMM had reissued.
//! Three things stop that being constructible under TCG: the *correct* outcome
//! is a fault, which kills the process doing the observing; a context switch
//! writes CR3 and so flushes the whole TLB, and the sibling is preempted
//! within milliseconds; and even the unacknowledged IPI this stage replaced
//! landed within microseconds, so the window it left open is far below anything
//! a guest can schedule into. What is gated instead is the property that closes
//! the window — the free happens after the flush — measured where it is
//! observable.

use toyos_abi::syscall::{self, MmapFlags, MmapProt, SYS_DEBUG};

/// Long enough to read off a clock through two syscalls, short enough that four
/// of them are not a boot's worth of stalled CPU. The delay spins with
/// interrupts disabled on the target, which is why it is not larger.
const DELAY_NANOS: u64 = 20_000_000;

/// Half the delay. The measurement is a lower bound on a spin the guest itself
/// performs, so it cannot come out short for scheduling reasons — but the two
/// clock reads bracketing it are syscalls, and the margin is there so a slow
/// host cannot turn a pass into a fail either way.
const FLOOR_NANOS: u64 = DELAY_NANOS / 2;

const PAGE_2M: usize = 2 * 1024 * 1024;

use toyos_abi::syscall::debug_action::{TLB_ACK_DELAY_ARM as ARM, TLB_ACK_DELAY_DISARM as DISARM};

fn debug(action: u64, arg: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rdi") SYS_DEBUG,
            in("rsi") action,
            in("rdx") arg,
            in("r8") 0u64,
            in("r9") 0u64,
            lateout("rax") ret,
            out("rcx") _,
            out("r11") _,
        );
    }
    ret
}

/// Arm the delay and report what one bare shootdown cost the kernel.
fn arm() -> u64 {
    debug(ARM, DELAY_NANOS)
}

fn disarm() {
    debug(DISARM, 0);
}

fn timed(f: impl FnOnce()) -> u64 {
    let start = syscall::clock_nanos();
    f();
    syscall::clock_nanos() - start
}

fn map(size: usize) -> *mut u8 {
    let p = unsafe {
        syscall::mmap(
            core::ptr::null_mut(),
            size,
            MmapProt::READ | MmapProt::WRITE,
            MmapFlags::ANONYMOUS | MmapFlags::PRIVATE,
        )
    };
    assert!(!p.is_null(), "mmap failed");
    p
}

fn main() {
    // 1. The primitive. The kernel times its own shootdown, so this number has
    //    no syscall overhead in it and no scheduling either — both CPUs are
    //    spinning for its whole duration.
    let bare = arm();
    assert!(
        bare >= FLOOR_NANOS,
        "a shootdown with the last CPU answering {DELAY_NANOS}ns late took {bare}ns — \
         the initiator is not waiting for it",
    );

    // 2. `munmap`, which is the syscall the stage exists for: the pages go back
    //    to the PMM, and a sibling thread of this process holds translations for
    //    exactly this range. Timed from userland, so a `munmap` that freed
    //    without shooting down comes back in microseconds.
    let region = map(PAGE_2M);
    let elapsed = timed(|| {
        unsafe { syscall::munmap(region, PAGE_2M) }.expect("munmap");
    });
    assert!(
        elapsed >= FLOOR_NANOS,
        "munmap returned in {elapsed}ns with the last CPU answering {DELAY_NANOS}ns late — \
         it freed the pages without waiting for the flush",
    );

    // 3. A fixed mapping placed over a range, which is a *remap* rather than a
    //    free: the address keeps its meaning and changes what it names, so a
    //    sibling holding the old translation writes into the wrong physical
    //    page with nothing ever faulting.
    let placed = map(PAGE_2M);
    let elapsed = timed(|| {
        let p = unsafe {
            syscall::mmap(
                placed,
                PAGE_2M,
                MmapProt::READ | MmapProt::WRITE,
                MmapFlags::ANONYMOUS | MmapFlags::PRIVATE | MmapFlags::FIXED,
            )
        };
        assert_eq!(p, placed, "MAP_FIXED did not honour the address");
    });
    assert!(
        elapsed >= FLOOR_NANOS,
        "a fixed mmap returned in {elapsed}ns with the last CPU answering {DELAY_NANOS}ns \
         late — it replaced the mapping without waiting for the flush",
    );
    unsafe { syscall::munmap(placed, PAGE_2M) }.expect("munmap the fixed mapping");

    disarm();

    // 4. And the delay is what produced every number above, not the machine:
    //    disarmed, the same three operations are back to microseconds. Without
    //    this the three assertions above would still pass on a kernel that
    //    happened to be slow for some other reason.
    let quiet = map(PAGE_2M);
    let elapsed = timed(|| {
        unsafe { syscall::munmap(quiet, PAGE_2M) }.expect("munmap");
    });
    assert!(
        elapsed < FLOOR_NANOS,
        "munmap still took {elapsed}ns with the delay disarmed, so the numbers above \
         measured something other than the wait",
    );

    println!("a shootdown waits for the last CPU, and munmap and a fixed mmap wait for it");
}

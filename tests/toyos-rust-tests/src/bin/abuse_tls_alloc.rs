//! `SYS_TLS_ALLOC_BLOCK` must hand a thread the same TLS block every time it
//! asks for one, and must take a thread's blocks out of the address space
//! before it gives their pages back.
//!
//! Both properties are memory safety, not bookkeeping: the block's virtual
//! address is returned to userland, so a mapping that outlives its pages is a
//! user-writable window onto whatever the PMM hands out next.

use std::sync::atomic::{AtomicU64, Ordering};

use toyos_abi::syscall::{self, MmapFlags, MmapProt, SyscallError};

const PAGE_2M: usize = 2 * 1024 * 1024;

/// The executable's own TLS module. Its DTV entry is filled at thread
/// creation, so nothing in the process reaches this syscall for it — which is
/// what makes it safe to drive directly.
const MODULE_EXE: u64 = 1;

/// The kernel's fixed DTV capacity (`loader::DTV_INITIAL_CAPACITY`).
const DTV_CAPACITY: u64 = 64;

/// This thread's DTV, from the TCB at `fs:[8]`.
fn dtv_entry(module_id: u64) -> *mut u64 {
    let p: u64;
    unsafe { core::arch::asm!("mov {}, fs:[8]", out(reg) p, options(nostack, readonly)) };
    unsafe { (p as *mut u64).add(2 + (module_id - 1) as usize) }
}

/// Allocate a block for `module_id`, leaving the DTV as it was found. The
/// syscall points the entry at the new block, and this thread's own
/// `#[thread_local]`s — the panic machinery and TLS destructors included —
/// resolve through it.
fn alloc_block_preserving_dtv(module_id: u64) -> Result<u64, SyscallError> {
    let slot = dtv_entry(module_id);
    let saved = unsafe { *slot };
    let r = syscall::tls_alloc_block(module_id);
    unsafe { *slot = saved };
    r
}

fn map_2m() -> *mut u8 {
    let p = unsafe {
        syscall::mmap(
            core::ptr::null_mut(),
            PAGE_2M,
            MmapProt::READ | MmapProt::WRITE,
            MmapFlags::ANONYMOUS | MmapFlags::PRIVATE,
        )
    };
    assert!(!p.is_null(), "mmap failed");
    p
}

static THREAD_BLOCK: AtomicU64 = AtomicU64::new(0);

/// Raw thread body. std's `thread::spawn` leaks its 2 MiB stack on every
/// spawn (`sys::thread::toyos` allocates it and `join` never frees it), which
/// walks the address space downwards and would swamp what part 3 measures.
extern "C" fn thread_body(_arg: u64) {
    let r = alloc_block_preserving_dtv(MODULE_EXE);
    THREAD_BLOCK.store(r.unwrap_or(0), Ordering::Release);
    syscall::thread_exit(0);
}

/// Run `thread_body` on `stack` and wait for it. Returns the block it got.
fn run_thread(stack: *mut u8) -> u64 {
    THREAD_BLOCK.store(0, Ordering::Release);
    let top = stack as u64 + PAGE_2M as u64;
    let tid = unsafe {
        syscall::thread_spawn(thread_body as *const () as u64, top, 0, stack as u64)
    };
    assert!(SyscallError::from_u64(tid).is_none(), "thread_spawn failed");
    syscall::thread_join(tid);
    let block = THREAD_BLOCK.load(Ordering::Acquire);
    assert!(block != 0, "thread failed to allocate a TLS block");
    block
}

fn main() {
    // 1. Every module id the process does not have is an error return, never a
    //    panic: zero, one past the DTV's capacity, a saturated one, and one
    //    inside the capacity that no module claims.
    assert_eq!(
        syscall::tls_alloc_block(0).unwrap_err(),
        SyscallError::InvalidArgument,
        "module_id 0",
    );
    assert_eq!(
        syscall::tls_alloc_block(DTV_CAPACITY).unwrap_err(),
        SyscallError::InvalidArgument,
        "module id inside the DTV but not in the module list",
    );
    assert_eq!(
        syscall::tls_alloc_block(DTV_CAPACITY + 1).unwrap_err(),
        SyscallError::ResourceExhausted,
        "module id past the DTV's capacity",
    );
    assert_eq!(
        syscall::tls_alloc_block(u64::MAX).unwrap_err(),
        SyscallError::ResourceExhausted,
        "saturated module id",
    );

    // 2. Asking twice returns the same block, and the first block's pages are
    //    still ours afterwards. A second block would have freed them while the
    //    first mapping stayed live and writable, so the loop below would read
    //    back whatever the new owner of those pages wrote.
    const MINE: u8 = 0x5A;
    const THEIRS: u8 = 0xC3;
    let first = alloc_block_preserving_dtv(MODULE_EXE).expect("first alloc");
    let block: *mut u8 = core::ptr::with_exposed_provenance_mut(first as usize);
    for off in (0..PAGE_2M).step_by(4096) {
        unsafe { block.add(off).write_volatile(MINE) };
    }

    let second = alloc_block_preserving_dtv(MODULE_EXE).expect("second alloc");
    assert_eq!(
        first, second,
        "a repeat call handed out a second block ({first:#x} then {second:#x})",
    );

    let claimed: Vec<*mut u8> = (0..16).map(|_| map_2m()).collect();
    for p in &claimed {
        for off in (0..PAGE_2M).step_by(4096) {
            unsafe { p.add(off).write_volatile(THEIRS) };
        }
    }
    for off in (0..PAGE_2M).step_by(4096) {
        assert_eq!(
            unsafe { block.add(off).read_volatile() },
            MINE,
            "TLS block at {first:#x}+{off:#x} was reissued to another mapping",
        );
    }
    for p in claimed {
        unsafe { syscall::munmap(p, PAGE_2M) }.expect("munmap");
    }

    // 3. A thread's mappings come back when it exits. Each run gets a fresh
    //    tid and so its own block; the kernel's `find_gap` is deterministic
    //    top-down, so if every region a run creates — the thread's own TLS
    //    block and the one it allocated — is released, every run must land on
    //    the same pair of addresses. A leak pushes each run below the last.
    let stack = map_2m();
    let runs: [u64; 4] = core::array::from_fn(|_| run_thread(stack));
    assert!(
        runs.iter().all(|a| *a == runs[0]),
        "a thread's TLS mappings outlived it: per-run blocks landed at {runs:#x?}",
    );

    println!("TLS block allocation is idempotent and released at thread exit");
}

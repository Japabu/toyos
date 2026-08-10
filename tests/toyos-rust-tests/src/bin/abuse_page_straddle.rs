//! A translation answers for one 2 MiB page. The typed syscall arguments are
//! read and written through exactly one of them, and ten of the twelve
//! `UserSafe` types are longer than their alignment — so a value placed near
//! the end of a page had its tail read or written *past the physical page*, in
//! whatever the PMM had handed out next.
//!
//! Two are reachable from userland: `fstat`'s 24-byte `Stat`, a kernel write,
//! and `spawn`'s 80-byte `SpawnArgs`, a kernel read whose `argv_ptr` and
//! `argv_len` the kernel then acts on.
//!
//! **The verdict is not the assertion.** The canary is the sixteen bytes on the
//! far side of the boundary: this process owns both pages, and mmap gives it
//! them contiguously, so the bytes the kernel would have written past the end
//! of the first physical page are bytes this process can read back.

use toyos_abi::RawHandle;
use toyos_abi::syscall::{
    self, MmapFlags, MmapProt, OpenFlags, ProcessStats, SchedInfo, SpawnArgs, SyscallError,
    SYS_FSTAT, SYS_PROCESS_STATS, SYS_SCHED_INFO,
};

const PAGE_2M: u64 = 2 * 1024 * 1024;
/// `Stat` is three `u64`, and `SpawnArgs` ten.
const STAT_LEN: usize = 24;
const CANARY: u8 = 0xA5;

/// Every typed wrapper fills the value on its own stack, so none of them can
/// express the argument under test. Number in rdi, arguments in rsi/rdx/r8/r9.
fn raw(num: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rdi") num,
            in("rsi") a1,
            in("rdx") a2,
            in("r8") a3,
            in("r9") 0u64,
            lateout("rax") ret,
            out("rcx") _,
            out("r11") _,
        );
    }
    ret
}

fn fstat_raw(fd: RawHandle, out: u64) -> u64 {
    raw(SYS_FSTAT, fd.0 as u64, out, 0)
}

fn err(ret: u64) -> Option<SyscallError> {
    SyscallError::from_u64(ret)
}

fn main() {
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
    let boundary = base + PAGE_2M;
    assert_eq!(base % PAGE_2M, 0, "mmap did not return a 2 MiB-aligned region");

    let fd = syscall::open(b"/bin/test_rs_abuse_page_straddle", OpenFlags::READ).expect("open self");

    // 1. A `Stat` eight bytes below the boundary: eight bytes of it are in this
    //    page and sixteen are not.
    let straddling = boundary - 8;
    let poison = |addr: u64, len: usize| {
        for i in 0..len {
            unsafe { (addr as *mut u8).add(i).write_volatile(CANARY) };
        }
    };
    let intact = |addr: u64, len: usize| {
        (0..len).all(|i| unsafe { (addr as *const u8).add(i).read_volatile() } == CANARY)
    };

    poison(straddling, STAT_LEN);
    let ret = fstat_raw(fd, straddling);
    // The memory before the verdict: an error return the kernel produced after
    // making the write is the failure this gate exists to catch, and asserting
    // the verdict first would stop the run before anyone looked.
    assert!(
        intact(boundary, STAT_LEN - 8),
        "fstat wrote {} bytes past the end of the physical page it translated",
        STAT_LEN - 8,
    );
    assert!(intact(straddling, 8), "fstat wrote the near half of a Stat it refused");
    assert_eq!(err(ret), Some(SyscallError::BadAddress), "fstat took a straddling Stat");

    // 2. The same call one byte-count lower, where the whole value fits: the
    //    refusal above is about the boundary and nothing else.
    let fitting = boundary - STAT_LEN as u64;
    poison(fitting, STAT_LEN);
    let ret = fstat_raw(fd, fitting);
    assert!(err(ret).is_none(), "fstat refused a Stat that ends at the boundary: {ret:#x}");
    let size = unsafe { (fitting as *const u64).add(1).read_volatile() };
    assert!(size > 0, "fstat wrote a Stat with no size in it");
    syscall::close(fd);

    // 3. `SpawnArgs` straddling. Every byte of it is this process's own and
    //    says the same thing on both sides of the boundary, so a kernel that
    //    reads it out of one translation gets a *correct* argv — and spawns.
    //    That is the observation: a child means the kernel acted on forty bytes
    //    it never validated.
    let argv = b"/bin/echo\0straddle\0";
    unsafe {
        core::ptr::copy_nonoverlapping(argv.as_ptr(), base as *mut u8, argv.len());
    }
    let args = SpawnArgs {
        argv_ptr: base,
        argv_len: argv.len() as u64,
        slot_map_ptr: 0,
        slot_map_count: 0,
        env_ptr: 0,
        env_len: 0,
        endow_ptr: 0,
        endow_count: 0,
        labels_ptr: 0,
        labels_len: 0,
    };
    let placed = (boundary - 8) as *mut SpawnArgs;
    unsafe { placed.write_volatile(args) };
    let err_ = unsafe { syscall::spawn(&*placed) }
        .map(|h| panic!("spawn read a straddling SpawnArgs and started {h:?}"))
        .unwrap_err();
    assert_eq!(err_, SyscallError::BadAddress, "wrong error for a straddling SpawnArgs");

    // 4. And spawn still works from a `SpawnArgs` that fits, so the refusal is
    //    the boundary and not the argument.
    let fitting = (boundary - core::mem::size_of::<SpawnArgs>() as u64) as *mut SpawnArgs;
    unsafe { fitting.write_volatile(args) };
    let child = unsafe { syscall::spawn(&*fitting) }.expect("spawn a SpawnArgs that fits");
    syscall::process_wait(child).expect("wait for the child that fits");

    // 5. `SchedInfo`, 24 bytes the kernel fills. It used to be reached by
    //    casting a validated byte slice to `&mut SchedInfo`, which checked
    //    neither the alignment the cast needs nor the page the write lands in.
    let info_len = core::mem::size_of::<SchedInfo>();
    let straddling = boundary - 8;
    poison(straddling, info_len);
    let ret = raw(SYS_SCHED_INFO, straddling, 0, 0);
    assert!(
        intact(boundary, info_len - 8),
        "sched_info wrote {} bytes past the end of the physical page it translated",
        info_len - 8,
    );
    assert!(intact(straddling, 8), "sched_info wrote the near half of a SchedInfo it refused");
    assert_eq!(err(ret), Some(SyscallError::BadAddress), "sched_info took a straddling SchedInfo");

    let ret = raw(SYS_SCHED_INFO, boundary - info_len as u64, 0, 0);
    assert!(err(ret).is_none(), "sched_info refused a SchedInfo that ends at the boundary: {ret:#x}");

    // 6. `ProcessStats` is 128 bytes, and the child waited for above still
    //    answers for one — the numbers are the object's and the handle outlives
    //    the process.
    let stats_len = core::mem::size_of::<ProcessStats>();
    let straddling = boundary - 8;
    poison(straddling, stats_len);
    let ret = raw(SYS_PROCESS_STATS, child.0 as u64, straddling, stats_len as u64);
    assert!(
        intact(boundary, stats_len - 8),
        "process_stats wrote {} bytes past the end of the physical page it translated",
        stats_len - 8,
    );
    assert_eq!(
        err(ret),
        Some(SyscallError::BadAddress),
        "process_stats took a straddling ProcessStats",
    );

    let ret = raw(SYS_PROCESS_STATS, child.0 as u64, base, stats_len as u64);
    assert!(
        err(ret).is_none(),
        "process_stats refused a ProcessStats that fits: {ret:#x}",
    );
    let wall_ns = unsafe { (base as *const u64).read_volatile() };
    assert!(wall_ns > 0, "process_stats wrote a snapshot with no wall time in it");

    unsafe { syscall::munmap(region, 2 * PAGE_2M as usize) }.expect("munmap");
    println!("a typed syscall argument may not cross the page its translation answers for");
}

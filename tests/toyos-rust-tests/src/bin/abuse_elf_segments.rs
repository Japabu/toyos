//! The kernel must survive an ELF whose program headers lie about their sizes.
//!
//! `p_filesz` and `p_memsz` were read straight out of the program header and
//! used as a (copy length, buffer size) pair. The ELF spec requires
//! `p_filesz <= p_memsz`, and the loader only ever handled the direction that
//! is harmless (zeroing a `.bss` tail). Inverted, the copy overran its
//! destination with attacker-chosen length *and* content:
//!
//! - `PT_TLS` on the exe path: `OwnedAlloc::new(tls_memsz)` then
//!   `copy_nonoverlapping(.., tls_filesz)` — a kernel heap overflow.
//! - `PT_LOAD` in a `dlopen`ed `.so`: the image is sized from `p_memsz` and
//!   each segment is read in at `p_filesz` — a PMM overflow past the image.
//!
//! A `p_memsz` no allocator can satisfy was a kernel panic on an `.expect`,
//! and program-header vaddrs outside the loaded image indexed a `KernelSlice`
//! out of bounds (an assert, i.e. also a panic).
//!
//! Every case must now be an error return, with the kernel intact afterwards.

use std::fs;

use toyos_abi::syscall::{self, SpawnArgs, SyscallError};

const DIR: &str = "/home/abuse_elf";

const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PT_TLS: u32 = 7;

/// One PT_LOAD covering the whole file plus one extra header the caller fills.
/// `entry` and the load segment are honest; only the extra header lies.
fn elf(extra: Phdr) -> Vec<u8> {
    let mut out = vec![0u8; 64 + 2 * 56];

    out[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    out[4] = 2; // ELFCLASS64
    out[5] = 1; // ELFDATA2LSB
    out[6] = 1; // EV_CURRENT
    out[16..18].copy_from_slice(&ET_DYN.to_le_bytes());
    out[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
    out[20..24].copy_from_slice(&1u32.to_le_bytes());
    out[24..32].copy_from_slice(&0x1000u64.to_le_bytes()); // e_entry
    out[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    out[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    out[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    out[56..58].copy_from_slice(&2u16.to_le_bytes()); // e_phnum
    out[58..60].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize

    let load = Phdr { kind: PT_LOAD, flags: 5, offset: 0, vaddr: 0, filesz: 0x2000, memsz: 0x2000, align: 0x1000 };
    load.write(&mut out[64..120]);
    extra.write(&mut out[120..176]);

    // Give the file real content past the headers, so an overrunning copy
    // moves a recognizable pattern rather than zeros.
    out.resize(0x4000, 0x41);
    out
}

struct Phdr {
    kind: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

impl Phdr {
    fn write(&self, out: &mut [u8]) {
        out[0..4].copy_from_slice(&self.kind.to_le_bytes());
        out[4..8].copy_from_slice(&self.flags.to_le_bytes());
        out[8..16].copy_from_slice(&self.offset.to_le_bytes());
        out[16..24].copy_from_slice(&self.vaddr.to_le_bytes());
        out[24..32].copy_from_slice(&self.vaddr.to_le_bytes()); // p_paddr
        out[32..40].copy_from_slice(&self.filesz.to_le_bytes());
        out[40..48].copy_from_slice(&self.memsz.to_le_bytes());
        out[48..56].copy_from_slice(&self.align.to_le_bytes());
    }
}

/// Write `bytes` to `path` and try to spawn it. Returns the spawn error.
fn spawn_err(name: &str, bytes: &[u8]) -> SyscallError {
    let path = format!("{DIR}/{name}");
    fs::write(&path, bytes).unwrap_or_else(|e| panic!("write {path}: {e}"));

    let argv = format!("{path}\0");
    unsafe {
        syscall::spawn(&SpawnArgs {
            argv_ptr: argv.as_ptr() as u64,
            argv_len: argv.len() as u64,
            fd_map_ptr: 0,
            fd_map_count: 0,
            env_ptr: 0,
            env_len: 0,
        })
    }
    .map(|pid| panic!("{name}: spawn succeeded (pid {pid:?}) — the header is malformed"))
    .unwrap_err()
}

/// Same bytes, reached through `dlopen` instead of `spawn`.
fn dlopen_err(name: &str, bytes: &[u8]) -> String {
    let path = format!("{DIR}/{name}");
    fs::write(&path, bytes).unwrap_or_else(|e| panic!("write {path}: {e}"));
    match unsafe { libloading::Library::new(&path) } {
        Ok(_) => panic!("{name}: dlopen succeeded — the header is malformed"),
        Err(e) => e.to_string(),
    }
}

fn main() {
    fs::create_dir_all(DIR).expect("create /home/abuse_elf");

    // 1. PT_TLS filesz > memsz: 16 KiB copied into a 16-byte kernel heap
    //    allocation. The overflow that made this test necessary.
    let err = spawn_err("tls_filesz", &elf(Phdr {
        kind: PT_TLS, flags: 4, offset: 0x2000, vaddr: 0x2000,
        filesz: 0x4000, memsz: 16, align: 8,
    }));
    assert_eq!(err, SyscallError::InvalidArgument, "PT_TLS filesz > memsz");

    // 2. PT_TLS memsz no allocator can satisfy — must be an error, not an
    //    `.expect` panic in syscall context.
    let err = spawn_err("tls_huge", &elf(Phdr {
        kind: PT_TLS, flags: 4, offset: 0x2000, vaddr: 0x2000,
        filesz: 0, memsz: u64::MAX / 2, align: 8,
    }));
    assert!(
        matches!(err, SyscallError::InvalidArgument | SyscallError::ResourceExhausted),
        "PT_TLS memsz = u64::MAX/2 gave {err:?}",
    );

    // 3. PT_TLS vaddr far outside every PT_LOAD — the file offset it
    //    extrapolates to is not in the image.
    let err = spawn_err("tls_vaddr", &elf(Phdr {
        kind: PT_TLS, flags: 4, offset: 0x2000, vaddr: 0x8000_0000,
        filesz: 8, memsz: 16, align: 8,
    }));
    assert_eq!(err, SyscallError::InvalidArgument, "PT_TLS vaddr outside the image");

    // 4. A second PT_LOAD with filesz > memsz. On the exe path the region
    //    bookkeeping clamps this, but it must still be refused rather than
    //    reaching the loader with an impossible header.
    let err = spawn_err("load_filesz", &elf(Phdr {
        kind: PT_LOAD, flags: 6, offset: 0x2000, vaddr: 0x4000,
        filesz: 0x2000, memsz: 0x10, align: 0x1000,
    }));
    assert_eq!(err, SyscallError::InvalidArgument, "PT_LOAD filesz > memsz");

    // 5. The same lie through `dlopen`, where the segment copy is not clamped:
    //    `load_shared_lib` sizes the image from p_memsz and reads p_filesz
    //    bytes into it, overrunning the PMM allocation.
    let msg = dlopen_err("load_filesz.so", &elf(Phdr {
        kind: PT_LOAD, flags: 6, offset: 0x2000, vaddr: 0x4000,
        filesz: 0x2000, memsz: 0x10, align: 0x1000,
    }));
    assert!(!msg.is_empty(), "dlopen error message");

    // 6. And a .so whose PT_TLS lies the same way.
    let msg = dlopen_err("tls_filesz.so", &elf(Phdr {
        kind: PT_TLS, flags: 4, offset: 0x2000, vaddr: 0x2000,
        filesz: 0x4000, memsz: 16, align: 8,
    }));
    assert!(!msg.is_empty(), "dlopen error message");

    // The kernel heap is intact: allocate and touch enough to walk it, then
    // prove spawn and the real loader still work.
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    for i in 0..256 {
        blocks.push(vec![(i % 251) as u8; 4096]);
    }
    for (i, b) in blocks.iter().enumerate() {
        assert!(b.iter().all(|&x| x == (i % 251) as u8), "kernel heap corrupted block {i}");
    }

    let status = std::process::Command::new("/bin/echo")
        .arg("loader still works")
        .status()
        .expect("spawn /bin/echo");
    assert!(status.success(), "/bin/echo exited {status:?}");

    let _ = fs::remove_dir_all(DIR);
    println!("malformed ELF program headers rejected, kernel intact");
}

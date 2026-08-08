//! A crafted ELF must not panic the kernel, and must not place a mapping the
//! kernel's own address-space rules forbid.
//!
//! `abuse_elf_segments` covers the sizes a program header declares.  This one
//! covers what the loader *derives* from them: the file offset a `DT_*` vaddr
//! maps to, the address a segment lands at once the image is rebased to
//! `USER_VM_BASE`, the TLS block's layout, and every table whose length the
//! file gets to name.  Each case is a kernel panic or a forbidden mapping
//! before the fix; each must be an error return after it, with the kernel
//! intact.

use std::fs;

use toyos_abi::syscall::{self, SpawnArgs, SyscallError};

const DIR: &str = "/home/abuse_loader";

/// The two cases about a table larger than one kernel allocation need a file
/// larger than one kernel allocation, because `read_file_range` clamps a
/// declared length to what the file actually holds. tmpfs is where that is
/// cheap: the pages are heap boxes, so a 3 MiB file costs no device I/O, and
/// the case under test is the loader rather than the filesystem.
///
/// Both cases asserted on the *declared* length alone until `/tmp` became
/// loadable — nothing in the tree could produce a >2 MiB file the loader would
/// open, so they passed without ever reaching the heap assert they exist for.
const BIG_DIR: &str = "/tmp/abuse_loader";

/// Comfortably past `mm::MAX_HEAP_ALLOC`, and past what dlmalloc rounds a
/// 2 MiB granule request up to.
const BIG: usize = 3 * 1024 * 1024;

/// Kernel constants this test aims at, by value.
///
/// `USER_VM_BASE` (`kernel/src/loader.rs`) is where every exe image is
/// rebased to; `ALLOC_FLOOR` (`kernel/src/vma.rs`) is the bottom of the arena
/// `find_gap` serves every library, TLS block and mmap out of; `KERNEL_HALF`
/// is where the direct map starts.  A segment vaddr is only interesting here
/// relative to one of the three.
const USER_VM_BASE: u64 = 0x100_0000_0000;
const ALLOC_FLOOR: u64 = 0x0002_0000_0000;
const KERNEL_HALF: u64 = 0xFFFF_8000_0000_0000;

const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_TLS: u32 = 7;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_STRSZ: i64 = 10;
const DT_JMPREL: i64 = 23;
const DT_PLTRELSZ: i64 = 2;
const DT_GNU_HASH: i64 = 0x6fff_fef5u32 as i32 as i64;

const SHT_DYNSYM: u32 = 11;

const R_X86_64_RELATIVE: u64 = 8;
const R_X86_64_DTPMOD64: u64 = 16;

const PH_OFF: usize = 64;
const PH_SIZE: usize = 56;
const SH_SIZE: usize = 64;

#[derive(Clone, Copy)]
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
    fn load(offset: u64, vaddr: u64, filesz: u64, memsz: u64, flags: u32) -> Self {
        Self { kind: PT_LOAD, flags, offset, vaddr, filesz, memsz, align: 0x1000 }
    }

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

/// A file the loader will accept as far as the case under test needs it to.
struct Elf {
    bytes: Vec<u8>,
    phdrs: Vec<Phdr>,
    entry: u64,
    shoff: u64,
    shnum: u16,
    shentsize: u16,
}

impl Elf {
    fn new(size: usize) -> Self {
        Self {
            bytes: vec![0u8; size],
            phdrs: Vec::new(),
            entry: 0x1000,
            shoff: 0,
            shnum: 0,
            shentsize: SH_SIZE as u16,
        }
    }

    fn ph(mut self, p: Phdr) -> Self {
        self.phdrs.push(p);
        self
    }

    fn entry(mut self, v: u64) -> Self {
        self.entry = v;
        self
    }

    fn sections(mut self, shoff: u64, shnum: u16, shentsize: u16) -> Self {
        self.shoff = shoff;
        self.shnum = shnum;
        self.shentsize = shentsize;
        self
    }

    fn poke(mut self, off: usize, data: &[u8]) -> Self {
        self.bytes[off..off + data.len()].copy_from_slice(data);
        self
    }

    /// A DT_NULL-terminated dynamic table at `off`.
    fn dynamic(mut self, off: usize, tags: &[(i64, u64)]) -> Self {
        let mut at = off;
        for &(tag, val) in tags {
            self.bytes[at..at + 8].copy_from_slice(&tag.to_le_bytes());
            self.bytes[at + 8..at + 16].copy_from_slice(&val.to_le_bytes());
            at += 16;
        }
        self
    }

    /// One Elf64_Rela at file offset `off`.
    fn rela(self, off: usize, r_offset: u64, r_info: u64, r_addend: i64) -> Self {
        self.poke(off, &r_offset.to_le_bytes())
            .poke(off + 8, &r_info.to_le_bytes())
            .poke(off + 16, &r_addend.to_le_bytes())
    }

    /// One Elf64_Sym at file offset `off`.
    fn sym(self, off: usize, st_name: u32, st_info: u8, st_shndx: u16, st_value: u64) -> Self {
        self.poke(off, &st_name.to_le_bytes())
            .poke(off + 4, &[st_info, 0])
            .poke(off + 6, &st_shndx.to_le_bytes())
            .poke(off + 8, &st_value.to_le_bytes())
    }

    /// One Elf64_Shdr at file offset `off`.
    fn shdr(self, off: usize, sh_type: u32, sh_offset: u64, sh_size: u64, sh_entsize: u64) -> Self {
        self.poke(off + 4, &sh_type.to_le_bytes())
            .poke(off + 24, &sh_offset.to_le_bytes())
            .poke(off + 32, &sh_size.to_le_bytes())
            .poke(off + 56, &sh_entsize.to_le_bytes())
    }

    fn build(mut self) -> Vec<u8> {
        let phnum = self.phdrs.len();
        assert!(PH_OFF + phnum * PH_SIZE <= self.bytes.len(), "phdrs past the buffer");

        self.bytes[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        self.bytes[4] = 2; // ELFCLASS64
        self.bytes[5] = 1; // ELFDATA2LSB
        self.bytes[6] = 1; // EV_CURRENT
        self.bytes[16..18].copy_from_slice(&ET_DYN.to_le_bytes());
        self.bytes[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        self.bytes[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
        self.bytes[24..32].copy_from_slice(&self.entry.to_le_bytes());
        self.bytes[32..40].copy_from_slice(&(PH_OFF as u64).to_le_bytes());
        self.bytes[40..48].copy_from_slice(&self.shoff.to_le_bytes());
        self.bytes[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
        self.bytes[54..56].copy_from_slice(&(PH_SIZE as u16).to_le_bytes());
        self.bytes[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());
        self.bytes[58..60].copy_from_slice(&self.shentsize.to_le_bytes());
        self.bytes[60..62].copy_from_slice(&self.shnum.to_le_bytes());

        for (i, p) in self.phdrs.iter().enumerate() {
            let at = PH_OFF + i * PH_SIZE;
            p.write(&mut self.bytes[at..at + PH_SIZE]);
        }
        self.bytes
    }
}

fn write_file(name: &str, bytes: &[u8]) -> String {
    write_file_in(DIR, name, bytes)
}

fn write_file_in(dir: &str, name: &str, bytes: &[u8]) -> String {
    let path = format!("{dir}/{name}");
    fs::write(&path, bytes).unwrap_or_else(|e| panic!("write {path}: {e}"));
    let got = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    assert_eq!(got as usize, bytes.len(), "{path}: short write");
    path
}

fn spawn_path(path: &str) -> Result<u64, SyscallError> {
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
    .map(|pid| pid.0 as u64)
}

fn spawn_result(name: &str, bytes: &[u8]) -> Result<u64, SyscallError> {
    spawn_path(&write_file(name, bytes))
}

/// Spawn must refuse this file. A success is as much a failure as a panic:
/// every case here describes a process the kernel cannot safely build.
fn refused(name: &str, outcome: Result<u64, SyscallError>) {
    match outcome {
        Ok(pid) => panic!("{name}: spawn returned pid {pid} for an ELF that must be refused"),
        Err(e) => assert!(
            matches!(e, SyscallError::InvalidArgument | SyscallError::ResourceExhausted),
            "{name}: spawn gave {e:?}",
        ),
    }
}

fn spawn_refused(name: &str, bytes: &[u8]) {
    refused(name, spawn_result(name, bytes));
}

/// Load it and throw it away. These cases are about a *walk* the loader does,
/// not about a header it can reject: a library whose `.gnu.hash` never
/// terminates or whose TLS symbol resolves nowhere is still a library, and it
/// may legitimately load with nothing in it. The assertion is that the kernel
/// comes back — which the checks at the end of `main` make, and which the
/// panic each of these used to raise made impossible.
fn dlopen_survives(name: &str, bytes: &[u8]) {
    let path = write_file(name, bytes);
    drop(unsafe { libloading::Library::new(&path) });
}

/// A minimal, honest exe: one PT_LOAD covering the whole file at vaddr 0.
fn base_exe(size: usize) -> Elf {
    Elf::new(size).ph(Phdr::load(0, 0, size as u64, size as u64, PF_R | PF_X)).entry(0)
}

fn main() {
    fs::create_dir_all(DIR).expect("create /home/abuse_loader");
    fs::create_dir_all(BIG_DIR).expect("create /tmp/abuse_loader");

    // 1. A DT_* vaddr below every PT_LOAD. `vaddr_to_file_offset` searched for
    //    the nearest segment at or below it and panicked outright when there
    //    was none — the one entry in this class with no failure path at all.
    spawn_refused(
        "rela_below_image",
        &Elf::new(0x4000)
            .ph(Phdr::load(0, 0x1000, 0x3000, 0x3000, PF_R | PF_W))
            .ph(Phdr { kind: PT_DYNAMIC, flags: PF_R, offset: 0x1000, vaddr: 0x2000, filesz: 0x200, memsz: 0x200, align: 8 })
            .entry(0x1000)
            .dynamic(0x1000, &[(DT_RELA, 0), (DT_RELASZ, 24)])
            .build(),
    );

    // 2. Two PT_LOADs whose page-rounded ranges overlap. Each becomes a VMA,
    //    and `insert_region` asserts that an address is not already covered —
    //    a kernel-bug assert reached from a file.
    spawn_refused(
        "load_overlap",
        &base_exe(0x4000)
            .ph(Phdr::load(0x1000, 0x800, 0x1000, 0x1000, PF_R | PF_W))
            .build(),
    );

    // 3. A segment whose rebased address lands in the kernel half. The image
    //    is placed at `USER_VM_BASE - vaddr_min`, and that addition wraps, so
    //    a file can name any address in the machine. The VMA is demand-paged,
    //    so the first user touch calls `remap`, which ORs PAGE_USER onto the
    //    *shared* kernel page tables — the mapping `sys_mmap` refuses a FIXED
    //    request for, reached through the loader instead.
    spawn_refused(
        "load_kernel_half",
        &base_exe(0x4000)
            .ph(Phdr::load(0, KERNEL_HALF.wrapping_sub(USER_VM_BASE), 0, 0x1000, PF_R | PF_W))
            .build(),
    );

    // 4. And the same wrap aimed at the allocation arena rather than the
    //    kernel: one segment covering everything from ALLOC_FLOOR upwards
    //    leaves `find_gap` nothing, and the TLS mapping every process gets
    //    was an `.expect` on that.
    spawn_refused(
        "load_covers_arena",
        &base_exe(0x4000)
            // The largest p_memsz that still leaves `p_vaddr + p_memsz` inside
            // a u64, which `parse_layout` checks before anything else.
            .ph(Phdr::load(0, ALLOC_FLOOR.wrapping_sub(USER_VM_BASE), 0, 0xFD_FFFF_FFFF, PF_R | PF_W))
            .build(),
    );

    // 5. PT_TLS p_align is a file-chosen addend to the TLS block's size.
    //    u64::MAX wrapped the size computation, and the layout that fell out
    //    of it tripped the block's own "DTV overlaps TLS data" assert.
    spawn_refused(
        "tls_align_wrap",
        &base_exe(0x4000)
            .ph(Phdr { kind: PT_TLS, flags: PF_R, offset: 0x2000, vaddr: 0x2000, filesz: 0, memsz: 8, align: u64::MAX })
            .build(),
    );

    // 6. A TLS size that leaves no room for the DTV in front of it. The DTV
    //    is a fixed 528-byte header the kernel writes at the start of the
    //    same allocation, and nothing counted it when sizing that allocation.
    //    Legal to load once it is counted, so the assertion is survival.
    let _ = spawn_result(
        "tls_crowds_dtv",
        &base_exe(0x4000)
            .ph(Phdr { kind: PT_TLS, flags: PF_R, offset: 0x2000, vaddr: 0x2000, filesz: 0, memsz: 0x1F_FF00, align: 8 })
            .build(),
    );

    // 7. A TLS template just under 2 MiB. The kernel heap's page source
    //    asserts above 2 MiB, and dlmalloc rounds a request up to a whole
    //    2 MiB granule *plus* its own bookkeeping — so the guard on the
    //    allocation was short by exactly that bookkeeping.
    spawn_refused(
        "tls_template_2m",
        &base_exe(0x4000)
            .ph(Phdr { kind: PT_TLS, flags: PF_R, offset: 0x2000, vaddr: 0x2000, filesz: 0, memsz: 0x1F_FFF0, align: 8 })
            .build(),
    );

    // 8. A section header table of 2.4 MiB, in a file big enough to hold it.
    //    The loader reads a declared table into one `Vec`, and dlmalloc serves
    //    an allocation that size by asking its page source for a 4 MiB
    //    granule — `mm/alloc.rs`'s assert, in syscall context.
    //
    //    The file has to be real: `read_file_range` clamps the declared length
    //    to what the file holds, so a 16 KiB file with a 2.4 MiB `e_shnum`
    //    reaches a 16 KiB allocation and proves nothing.
    refused(
        "shnum_past_heap",
        spawn_path(&write_file_in(
            BIG_DIR,
            "shnum_past_heap",
            &base_exe(BIG).sections(0x1000, 40_000, 64).build(),
        )),
    );

    // 9. Same ceiling, reached through DT_STRSZ instead — a size no section
    //    header has to agree with, in a file that can back it.
    refused(
        "strsz_past_heap",
        spawn_path(&write_file_in(
            BIG_DIR,
            "strsz_past_heap",
            &Elf::new(BIG)
                .ph(Phdr::load(0, 0, BIG as u64, BIG as u64, PF_R | PF_W))
                .ph(Phdr { kind: PT_DYNAMIC, flags: PF_R, offset: 0x1000, vaddr: 0x1000, filesz: 0x200, memsz: 0x200, align: 8 })
                .entry(0)
                .dynamic(0x1000, &[(DT_STRTAB, 0x2000), (DT_STRSZ, 2 * 1024 * 1024 + 4096)])
                .build(),
        )),
    );

    // 10. A .gnu.hash whose chain array never terminates. The symbol-count
    //     walk followed `chain[i] & 1` with no bound but the slice's own
    //     assert, and a zeroed image never sets that bit.
    dlopen_survives(
        "gnu_hash_runaway.so",
        &Elf::new(0x4000)
            .ph(Phdr::load(0, 0, 0x4000, 0x4000, PF_R | PF_W))
            .ph(Phdr { kind: PT_DYNAMIC, flags: PF_R, offset: 0x1000, vaddr: 0x1000, filesz: 0x200, memsz: 0x200, align: 8 })
            .dynamic(0x1000, &[(DT_GNU_HASH, 0x2000), (DT_STRTAB, 0x3000), (DT_STRSZ, 16)])
            // nbuckets=1, symoffset=0, bloom_size=1, bloom_shift=0, bloom[0],
            // buckets[0]=1 — then chains, all zero to the end of the image.
            .poke(0x2000, &1u32.to_le_bytes())
            .poke(0x2008, &1u32.to_le_bytes())
            .poke(0x2018, &1u32.to_le_bytes())
            .build(),
    );

    // 12. Two relocation tables the ceiling *accepts*, whose derived index
    //     does not fit. This is the gap in cases 8 and 9: they prove an input
    //     over the ceiling is refused, and say nothing about an input under it
    //     from which the kernel derives something over.
    //
    //     `DT_RELASZ` and `DT_PLTRELSZ` are bounded separately at
    //     MAX_HEAP_ALLOC and both feed one `RelocationIndex`, so two tables of
    //     87,210 entries each are 174,420 entries of 16 bytes = 2.7 MiB in a
    //     single Vec. Under the old growth-by-doubling that overshot to 4 MiB
    //     on the push.
    //
    //     This case is also the actuator for the allocator-lock defect
    //     (`specs/issues/panic-path/`): the >2 MiB assert fires inside
    //     `KernelAllocator::alloc` *while it holds the dlmalloc lock*, so the
    //     recovered CPU's next allocation spins on a lock the dead thread
    //     still owns. Nothing else in the suite stages that, which is part of
    //     why it has stayed open -- do not build a second one.
    {
        const N: usize = 87_210; // MAX_HEAP_ALLOC / 24, the most either table can declare
        const SZ: usize = N * 24;
        const RELA: usize = 0x4000;
        const JMPREL: usize = RELA + SZ;
        let total = JMPREL + SZ;
        let mut bytes = Elf::new(total)
            .ph(Phdr::load(0, 0, total as u64, total as u64, PF_R | PF_W))
            .ph(Phdr { kind: PT_DYNAMIC, flags: PF_R, offset: 0x1000, vaddr: 0x1000, filesz: 0x200, memsz: 0x200, align: 8 })
            .entry(0)
            .dynamic(0x1000, &[
                (DT_RELA, RELA as u64), (DT_RELASZ, SZ as u64),
                (DT_JMPREL, JMPREL as u64), (DT_PLTRELSZ, SZ as u64),
            ])
            .build();
        for i in 0..N * 2 {
            let at = RELA + i * 24;
            bytes[at..at + 8].copy_from_slice(&((i as u64) * 8).to_le_bytes());
            bytes[at + 8..at + 16].copy_from_slice(&R_X86_64_RELATIVE.to_le_bytes());
        }
        refused(
            "rela_index_past_heap",
            spawn_path(&write_file_in(BIG_DIR, "rela_index_past_heap", &bytes)),
        );
    }

    // 11. A DTPMOD64 relocation naming a TLS symbol no loaded module defines.
    //     Its two resolvers panicked where every other unresolved-symbol path
    //     in the loader logs and carries on.
    dlopen_survives("dtpmod_unresolved.so", &so_with_dtpmod());

    // The kernel heap is intact: allocate and touch enough to walk it, then
    // prove the real loader still works.
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
    let _ = fs::remove_dir_all(BIG_DIR);
    println!("crafted ELF derivations rejected, kernel intact");
}

/// A library that loads far enough to reach `apply_dtpmod_relocs`, carrying
/// one DTPMOD64 whose symbol is undefined here and defined nowhere else.
///
/// Everything in it exists to get past a check: PT_TLS so the module is given
/// a DTV id at all, a writable PT_LOAD so the relocation's `r_offset` is
/// inside the private window the loader validates against, and an SHT_DYNSYM
/// section header so `r_sym` is inside a symbol table.
fn so_with_dtpmod() -> Vec<u8> {
    Elf::new(0x5000)
        .ph(Phdr::load(0, 0, 0x4000, 0x20_0000, PF_R | PF_X))
        .ph(Phdr::load(0x4000, 0x20_0000, 0x1000, 0x20_0000, PF_R | PF_W))
        .ph(Phdr { kind: PT_TLS, flags: PF_R, offset: 0x100, vaddr: 0x100, filesz: 0, memsz: 8, align: 8 })
        .ph(Phdr { kind: PT_DYNAMIC, flags: PF_R, offset: 0x1000, vaddr: 0x1000, filesz: 0x200, memsz: 0x200, align: 8 })
        .sections(0x3800, 1, 64)
        .dynamic(
            0x1000,
            &[
                (DT_SYMTAB, 0x2000),
                (DT_STRTAB, 0x3000),
                (DT_STRSZ, 0x100),
                (DT_RELA, 0x2800),
                (DT_RELASZ, 24),
            ],
        )
        // sym[1]: undefined (st_shndx == 0), so resolution goes cross-module.
        .sym(0x2018, 1, 6 /* STT_TLS */, 0, 0)
        .poke(0x3001, b"tls_nowhere\0")
        .rela(0x2800, 0x20_0000, (1u64 << 32) | R_X86_64_DTPMOD64, 0)
        .shdr(0x3800, SHT_DYNSYM, 0x2000, 48, 24)
        .build()
}

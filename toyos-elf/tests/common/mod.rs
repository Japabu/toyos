//! An ELF64 builder that produces exactly the bytes it is told to.
//!
//! Deliberately not a linker: every field is settable to a value no linker
//! would emit, because that is the whole point of the corpus these tests are.

pub const PH_OFF: usize = 64;
pub const PH_SIZE: usize = 56;
pub const SH_SIZE: usize = 64;

pub const ET_DYN: u16 = 3;
pub const ET_EXEC: u16 = 2;
pub const EM_X86_64: u16 = 62;
pub const EM_AARCH64: u16 = 183;

pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;
pub const PT_TLS: u32 = 7;
pub const PT_GNU_EH_FRAME: u32 = 0x6474_e550;

pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

#[derive(Clone, Copy)]
pub struct Phdr {
    pub kind: u32,
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

impl Phdr {
    pub fn load(offset: u64, vaddr: u64, filesz: u64, memsz: u64, flags: u32) -> Phdr {
        Phdr { kind: PT_LOAD, flags, offset, vaddr, filesz, memsz, align: 0x1000 }
    }

    pub fn tls(vaddr: u64, filesz: u64, memsz: u64, align: u64) -> Phdr {
        Phdr { kind: PT_TLS, flags: PF_R, offset: vaddr, vaddr, filesz, memsz, align }
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

pub struct Elf {
    pub bytes: Vec<u8>,
    phdrs: Vec<Phdr>,
    class: u8,
    data: u8,
    version: u8,
    e_type: u16,
    machine: u16,
    entry: u64,
    phoff: u64,
    phnum: Option<u16>,
    phentsize: u16,
    shoff: u64,
    shnum: u16,
    shentsize: u16,
}

impl Elf {
    pub fn new(size: usize) -> Elf {
        Elf {
            bytes: vec![0u8; size],
            phdrs: Vec::new(),
            class: 2,
            data: 1,
            version: 1,
            e_type: ET_DYN,
            machine: EM_X86_64,
            entry: 0,
            phoff: PH_OFF as u64,
            phnum: None,
            phentsize: PH_SIZE as u16,
            shoff: 0,
            shnum: 0,
            shentsize: SH_SIZE as u16,
        }
    }

    /// One `PT_LOAD` covering the whole file at vaddr 0, entry at 0.
    pub fn honest(size: usize) -> Elf {
        Elf::new(size).ph(Phdr::load(0, 0, size as u64, size as u64, PF_R | PF_X))
    }

    pub fn ph(mut self, p: Phdr) -> Elf {
        self.phdrs.push(p);
        self
    }

    pub fn class(mut self, v: u8) -> Elf {
        self.class = v;
        self
    }

    pub fn endian(mut self, v: u8) -> Elf {
        self.data = v;
        self
    }

    pub fn version(mut self, v: u8) -> Elf {
        self.version = v;
        self
    }

    pub fn kind(mut self, v: u16) -> Elf {
        self.e_type = v;
        self
    }

    pub fn machine(mut self, v: u16) -> Elf {
        self.machine = v;
        self
    }

    pub fn entry(mut self, v: u64) -> Elf {
        self.entry = v;
        self
    }

    pub fn phoff(mut self, v: u64) -> Elf {
        self.phoff = v;
        self
    }

    /// Override `e_phnum` independently of the headers actually written.
    pub fn phnum(mut self, v: u16) -> Elf {
        self.phnum = Some(v);
        self
    }

    pub fn phentsize(mut self, v: u16) -> Elf {
        self.phentsize = v;
        self
    }

    pub fn sections(mut self, shoff: u64, shnum: u16, shentsize: u16) -> Elf {
        self.shoff = shoff;
        self.shnum = shnum;
        self.shentsize = shentsize;
        self
    }

    pub fn poke(mut self, off: usize, data: &[u8]) -> Elf {
        self.bytes[off..off + data.len()].copy_from_slice(data);
        self
    }

    pub fn build(mut self) -> Vec<u8> {
        let written = self.phdrs.len();
        let phnum = self.phnum.unwrap_or(written as u16);

        self.bytes[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        self.bytes[4] = self.class;
        self.bytes[5] = self.data;
        self.bytes[6] = self.version;
        self.bytes[16..18].copy_from_slice(&self.e_type.to_le_bytes());
        self.bytes[18..20].copy_from_slice(&self.machine.to_le_bytes());
        self.bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        self.bytes[24..32].copy_from_slice(&self.entry.to_le_bytes());
        self.bytes[32..40].copy_from_slice(&self.phoff.to_le_bytes());
        self.bytes[40..48].copy_from_slice(&self.shoff.to_le_bytes());
        self.bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
        self.bytes[54..56].copy_from_slice(&self.phentsize.to_le_bytes());
        self.bytes[56..58].copy_from_slice(&phnum.to_le_bytes());
        self.bytes[58..60].copy_from_slice(&self.shentsize.to_le_bytes());
        self.bytes[60..62].copy_from_slice(&self.shnum.to_le_bytes());

        if self.phoff == PH_OFF as u64 {
            for (i, p) in self.phdrs.iter().enumerate() {
                let at = PH_OFF + i * PH_SIZE;
                p.write(&mut self.bytes[at..at + PH_SIZE]);
            }
        }
        self.bytes
    }
}

/// One `Elf64_Rela`, as bytes.
pub fn rela(r_offset: u64, r_sym: u32, r_type: u32, r_addend: i64) -> [u8; 24] {
    let mut out = [0u8; 24];
    out[0..8].copy_from_slice(&r_offset.to_le_bytes());
    out[8..16].copy_from_slice(&(((r_sym as u64) << 32) | r_type as u64).to_le_bytes());
    out[16..24].copy_from_slice(&r_addend.to_le_bytes());
    out
}

/// One `Elf64_Sym`, as bytes.
pub fn sym(st_name: u32, st_info: u8, st_shndx: u16, st_value: u64) -> [u8; 24] {
    let mut out = [0u8; 24];
    out[0..4].copy_from_slice(&st_name.to_le_bytes());
    out[4] = st_info;
    out[6..8].copy_from_slice(&st_shndx.to_le_bytes());
    out[8..16].copy_from_slice(&st_value.to_le_bytes());
    out
}

/// One `Elf64_Shdr`, as bytes.
pub fn shdr(sh_type: u32, sh_offset: u64, sh_size: u64, sh_link: u32, sh_entsize: u64) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[4..8].copy_from_slice(&sh_type.to_le_bytes());
    out[24..32].copy_from_slice(&sh_offset.to_le_bytes());
    out[32..40].copy_from_slice(&sh_size.to_le_bytes());
    out[40..44].copy_from_slice(&sh_link.to_le_bytes());
    out[56..64].copy_from_slice(&sh_entsize.to_le_bytes());
    out
}

/// A `DT_NULL`-terminated dynamic table, as bytes.
pub fn dynamic(tags: &[(i64, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(tag, val) in tags {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&val.to_le_bytes());
    }
    out.extend_from_slice(&0i64.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out
}

/// The same, with no `DT_NULL` at the end.
pub fn dynamic_unterminated(tags: &[(i64, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(tag, val) in tags {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&val.to_le_bytes());
    }
    out
}

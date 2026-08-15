//! Synthetic linker inputs and a run harness, shared by the test files here.
//!
//! One copy: two would drift, and both are asked to build the same shapes of
//! object.

#![allow(dead_code)]

use object::write::{Object, Relocation, StandardSection, Symbol, SymbolSection};
use object::{
    elf, Architecture, BinaryFormat, Endianness, RelocationFlags, SymbolFlags, SymbolKind,
    SymbolScope,
};
use std::path::{Path, PathBuf};
use std::process::Command;

// ── Input synthesis ──────────────────────────────────────────────────────

/// How many samples a determinism case takes. Two suffices for a case whose
/// hazard is wide — each of them is, deliberately, so that a hash order and a
/// sorted order essentially never coincide. Eight is the margin for the narrow
/// case somebody adds later: `toyos-cc`'s gate has one hazard of a single stack
/// slot, and two runs caught it 39 times in 40 where eight caught it 40.
pub const RUNS: usize = 8;

/// `mov rax, [rip + disp32]` — the byte sequence Cranelift emits for a GOT load.
pub const MOV_RAX_RIP: [u8; 3] = [0x48, 0x8B, 0x05];
/// `call rel32`
pub const CALL_REL32: u8 = 0xE8;
pub const RET: u8 = 0xC3;

pub struct ObjBuilder {
    obj: Object<'static>,
}

impl ObjBuilder {
    pub fn new() -> Self {
        ObjBuilder {
            obj: Object::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little),
        }
    }

    pub fn text(&mut self, name: &str, code: &[u8], scope: SymbolScope) -> object::write::SymbolId {
        let section = self.obj.section_id(StandardSection::Text);
        let offset = self.obj.append_section_data(section, code, 16);
        self.obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: offset,
            size: code.len() as u64,
            kind: SymbolKind::Text,
            scope,
            weak: false,
            section: SymbolSection::Section(section),
            flags: SymbolFlags::None,
        })
    }

    pub fn data(&mut self, name: &str, bytes: &[u8], scope: SymbolScope) -> object::write::SymbolId {
        let section = self.obj.section_id(StandardSection::Data);
        let offset = self.obj.append_section_data(section, bytes, 8);
        self.obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: offset,
            size: bytes.len() as u64,
            kind: SymbolKind::Data,
            scope,
            weak: false,
            section: SymbolSection::Section(section),
            flags: SymbolFlags::None,
        })
    }

    pub fn undefined(&mut self, name: &str, kind: SymbolKind) -> object::write::SymbolId {
        self.obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: 0,
            size: 0,
            kind,
            scope: SymbolScope::Dynamic,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        })
    }

    /// Append a function that GOT-loads each of `targets`, then returns. The
    /// GOT slot order is what `.rela.dyn` is built from.
    pub fn got_loader(
        &mut self,
        name: &str,
        targets: &[object::write::SymbolId],
        scope: SymbolScope,
    ) -> object::write::SymbolId {
        let mut code = Vec::new();
        for _ in targets {
            code.extend_from_slice(&MOV_RAX_RIP);
            code.extend_from_slice(&0i32.to_le_bytes());
        }
        code.push(RET);
        let sym = self.text(name, &code, scope);
        let section = self.obj.section_id(StandardSection::Text);
        let base = self.symbol_offset(sym);
        for (i, &target) in targets.iter().enumerate() {
            self.obj.add_relocation(
                section,
                Relocation {
                    offset: base + (i * 7) as u64 + MOV_RAX_RIP.len() as u64,
                    symbol: target,
                    addend: -4,
                    flags: RelocationFlags::Elf { r_type: elf::R_X86_64_REX_GOTPCRELX },
                },
            )
            .unwrap();
        }
        sym
    }

    /// Append a function that calls each of `targets` through PLT32.
    pub fn caller(
        &mut self,
        name: &str,
        targets: &[object::write::SymbolId],
        scope: SymbolScope,
    ) -> object::write::SymbolId {
        let mut code = Vec::new();
        for _ in targets {
            code.push(CALL_REL32);
            code.extend_from_slice(&0i32.to_le_bytes());
        }
        code.push(RET);
        let sym = self.text(name, &code, scope);
        let section = self.obj.section_id(StandardSection::Text);
        let base = self.symbol_offset(sym);
        for (i, &target) in targets.iter().enumerate() {
            self.obj.add_relocation(
                section,
                Relocation {
                    offset: base + (i * 5) as u64 + 1,
                    symbol: target,
                    addend: -4,
                    flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 },
                },
            )
            .unwrap();
        }
        sym
    }

    pub fn symbol_offset(&self, id: object::write::SymbolId) -> u64 {
        self.obj.symbol(id).value
    }

    pub fn finish(self) -> Vec<u8> {
        self.obj.write().unwrap()
    }
}

/// `ar` archive with the given members, in the format `collect` parses.
pub fn archive(members: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, data) in members {
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            format!("{name}/"),
            0,
            0,
            0,
            "100644",
            data.len()
        );
        assert_eq!(header.len(), 60);
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

// ── Harness ──────────────────────────────────────────────────────────────

pub struct Case {
    dir: PathBuf,
    inputs: Vec<PathBuf>,
    args: Vec<String>,
}

impl Case {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("toyos-ld-det-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Case { dir, inputs: Vec::new(), args: Vec::new() }
    }

    pub fn input(mut self, name: &str, bytes: Vec<u8>) -> Self {
        let path = self.dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        self.inputs.push(path);
        self
    }

    pub fn arg(mut self, a: &str) -> Self {
        self.args.push(a.to_string());
        self
    }

    pub fn link_once(&self, out: &Path) {
        let status = Command::new(env!("CARGO_BIN_EXE_toyos-ld"))
            .args(&self.args)
            .arg("-o")
            .arg(out)
            .args(&self.inputs)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "link failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    /// Link once and return the output bytes.
    pub fn link(&self) -> Vec<u8> {
        let out = self.dir.join("out");
        self.link_once(&out);
        std::fs::read(&out).unwrap()
    }

    /// Link once, expecting it to fail, and return what the linker said.
    pub fn link_expecting_failure(&self) -> String {
        let out = self.dir.join("out");
        let result = Command::new(env!("CARGO_BIN_EXE_toyos-ld"))
            .args(&self.args)
            .arg("-o")
            .arg(&out)
            .args(&self.inputs)
            .output()
            .unwrap();
        assert!(!result.status.success(), "expected the link to fail, and it succeeded");
        String::from_utf8_lossy(&result.stderr).into_owned()
    }

    /// Link `RUNS` times and return the index of the first run whose output
    /// differs from run 0, with the number of differing bytes.
    pub fn diff(&self) -> Option<(usize, usize)> {
        let mut first: Option<Vec<u8>> = None;
        for run in 0..RUNS {
            let out = self.dir.join(format!("out.{run}"));
            self.link_once(&out);
            let bytes = std::fs::read(&out).unwrap();
            match &first {
                None => first = Some(bytes),
                Some(f) => {
                    if *f != bytes {
                        let differing = if f.len() == bytes.len() {
                            f.iter().zip(&bytes).filter(|(a, b)| a != b).count()
                        } else {
                            usize::MAX
                        };
                        return Some((run, differing));
                    }
                }
            }
        }
        None
    }

    pub fn assert_identical(&self, what: &str) {
        if let Some((run, bytes)) = self.diff() {
            panic!("{what}: run {run} of {RUNS} differs from run 0 in {bytes} bytes");
        }
    }
}


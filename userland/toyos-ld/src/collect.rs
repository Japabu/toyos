use object::elf;
use object::pe;
use object::macho;
use object::read::elf::ElfFile64;
use object::read::{self, Object, ObjectSection, ObjectSymbol};
use object::RelocationFlags;
use crate::LinkError;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

/// Newtype for indices into `LinkState::sections`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SectionIdx(pub usize);

impl std::ops::Index<SectionIdx> for Vec<InputSection> {
    type Output = InputSection;
    fn index(&self, idx: SectionIdx) -> &InputSection { &self[idx.0] }
}

impl std::ops::IndexMut<SectionIdx> for Vec<InputSection> {
    fn index_mut(&mut self, idx: SectionIdx) -> &mut InputSection { &mut self[idx.0] }
}

/// Newtype for indices into the input objects slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ObjIdx(pub usize);

/// Type-safe symbol reference that distinguishes global from local symbols.
/// Local symbols carry their originating object index, ensuring they are never
/// confused with same-named locals from other objects (e.g. `.str.63`).
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub(crate) enum SymbolRef {
    Global(String),
    Local(ObjIdx, String),
}

impl SymbolRef {
    pub(crate) fn name(&self) -> &str {
        match self {
            SymbolRef::Global(n) | SymbolRef::Local(_, n) => n,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SectionKind {
    Code,
    ReadOnly,
    Data,
    Bss,
    Tls,
    TlsBss,
    /// Mach-O `__thread_vars`: TLV descriptors (thunk + key + offset).
    TlsVariables,
    InitArray,
    FiniArray,
}

/// Target architecture for the link output, detected from input object metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arch {
    Aarch64,
    X86_64,
}

impl SectionKind {
    pub fn is_writable(self) -> bool {
        matches!(self, Self::Data | Self::Bss | Self::InitArray | Self::FiniArray)
    }
    pub fn is_nobits(self) -> bool {
        matches!(self, Self::Bss | Self::TlsBss)
    }
    pub fn is_tls(self) -> bool {
        matches!(self, Self::Tls | Self::TlsBss | Self::TlsVariables)
    }
}

#[derive(Clone)]
pub(crate) struct InputSection {
    pub(crate) name: String,
    pub(crate) data: Vec<u8>,
    pub(crate) align: u64,
    pub(crate) size: u64,
    pub(crate) vaddr: Option<u64>,
    pub(crate) kind: SectionKind,
    /// Section has SHF_MERGE flag (eligible for deduplication)
    pub(crate) merge: bool,
    /// Section has SHF_STRINGS flag (null-terminated string entries)
    pub(crate) strings: bool,
    /// Entry size for merge sections (e.g., 1 for .rodata.str1.1)
    pub(crate) entsize: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocType {
    // x86_64
    X86_64,
    X86Pc32,
    X86Plt32,
    X86_32,
    X86_32S,
    X86Gotpcrel,
    X86Gotpcrelx,
    X86RexGotpcrelx,
    X86Tpoff32,
    X86Gottpoff,
    X86Tlsgd,
    X86Tlsld,
    X86Dtpoff32,
    // AArch64
    Aarch64Abs64,
    Aarch64Abs32,
    Aarch64Prel32,
    Aarch64Call26,
    Aarch64Jump26,
    Aarch64AdrPrelPgHi21,
    Aarch64AddAbsLo12Nc,
    Aarch64Ldst8AbsLo12Nc,
    Aarch64Ldst16AbsLo12Nc,
    Aarch64Ldst32AbsLo12Nc,
    Aarch64Ldst64AbsLo12Nc,
    Aarch64Ldst128AbsLo12Nc,
    Aarch64MovwUabsG0Nc,
    Aarch64MovwUabsG1Nc,
    Aarch64MovwUabsG2Nc,
    Aarch64MovwUabsG3,
    Aarch64AdrGotPage,
    Aarch64Ld64GotLo12Nc,
    Aarch64GotPcrel32,
    Aarch64TlvpLoadPage21,
}

impl std::fmt::Display for RelocType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelocType::X86_64 => write!(f, "R_X86_64_64"),
            RelocType::X86Pc32 => write!(f, "R_X86_64_PC32"),
            RelocType::X86Plt32 => write!(f, "R_X86_64_PLT32"),
            RelocType::X86_32 => write!(f, "R_X86_64_32"),
            RelocType::X86_32S => write!(f, "R_X86_64_32S"),
            RelocType::X86Gotpcrel => write!(f, "R_X86_64_GOTPCREL"),
            RelocType::X86Gotpcrelx => write!(f, "R_X86_64_GOTPCRELX"),
            RelocType::X86RexGotpcrelx => write!(f, "R_X86_64_REX_GOTPCRELX"),
            RelocType::X86Tpoff32 => write!(f, "R_X86_64_TPOFF32"),
            RelocType::X86Gottpoff => write!(f, "R_X86_64_GOTTPOFF"),
            RelocType::X86Tlsgd => write!(f, "R_X86_64_TLSGD"),
            RelocType::X86Tlsld => write!(f, "R_X86_64_TLSLD"),
            RelocType::X86Dtpoff32 => write!(f, "R_X86_64_DTPOFF32"),
            RelocType::Aarch64Abs64 => write!(f, "R_AARCH64_ABS64"),
            RelocType::Aarch64Abs32 => write!(f, "R_AARCH64_ABS32"),
            RelocType::Aarch64Prel32 => write!(f, "R_AARCH64_PREL32"),
            RelocType::Aarch64Call26 => write!(f, "R_AARCH64_CALL26"),
            RelocType::Aarch64Jump26 => write!(f, "R_AARCH64_JUMP26"),
            RelocType::Aarch64AdrPrelPgHi21 => write!(f, "R_AARCH64_ADR_PREL_PG_HI21"),
            RelocType::Aarch64AddAbsLo12Nc => write!(f, "R_AARCH64_ADD_ABS_LO12_NC"),
            RelocType::Aarch64Ldst8AbsLo12Nc => write!(f, "R_AARCH64_LDST8_ABS_LO12_NC"),
            RelocType::Aarch64Ldst16AbsLo12Nc => write!(f, "R_AARCH64_LDST16_ABS_LO12_NC"),
            RelocType::Aarch64Ldst32AbsLo12Nc => write!(f, "R_AARCH64_LDST32_ABS_LO12_NC"),
            RelocType::Aarch64Ldst64AbsLo12Nc => write!(f, "R_AARCH64_LDST64_ABS_LO12_NC"),
            RelocType::Aarch64Ldst128AbsLo12Nc => write!(f, "R_AARCH64_LDST128_ABS_LO12_NC"),
            RelocType::Aarch64MovwUabsG0Nc => write!(f, "R_AARCH64_MOVW_UABS_G0_NC"),
            RelocType::Aarch64MovwUabsG1Nc => write!(f, "R_AARCH64_MOVW_UABS_G1_NC"),
            RelocType::Aarch64MovwUabsG2Nc => write!(f, "R_AARCH64_MOVW_UABS_G2_NC"),
            RelocType::Aarch64MovwUabsG3 => write!(f, "R_AARCH64_MOVW_UABS_G3"),
            RelocType::Aarch64AdrGotPage => write!(f, "R_AARCH64_ADR_GOT_PAGE"),
            RelocType::Aarch64Ld64GotLo12Nc => write!(f, "R_AARCH64_LD64_GOT_LO12_NC"),
            RelocType::Aarch64GotPcrel32 => write!(f, "ARM64_RELOC_POINTER_TO_GOT"),
            RelocType::Aarch64TlvpLoadPage21 => write!(f, "ARM64_RELOC_TLVP_LOAD_PAGE21"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct InputReloc {
    pub(crate) section: SectionIdx,
    pub(crate) offset: u64,
    pub(crate) r_type: RelocType,
    pub(crate) target: SymbolRef,
    pub(crate) addend: i64,
    /// Mach-O SUBTRACTOR pairs: `target - subtrahend + addend`.
    pub(crate) subtrahend: Option<SymbolRef>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SymbolDef {
    Defined { section: SectionIdx, value: u64 },
    Dynamic,
}

pub(crate) struct LinkState {
    pub(crate) sections: Vec<InputSection>,
    pub(crate) relocs: Vec<InputReloc>,
    pub(crate) globals: HashMap<String, SymbolDef>,
    pub(crate) locals: HashMap<(ObjIdx, String), SymbolDef>,
    pub(crate) tls_sections: Vec<SectionIdx>,
    /// Non-loadable metadata sections (e.g. .rustc) preserved in shared library output.
    pub(crate) metadata: Vec<(String, Vec<u8>)>,
    /// Symbol names provided by shared library (.so) inputs.
    pub(crate) dynamic_imports: HashSet<String>,
    /// Bare filenames of .so inputs (for DT_NEEDED entries).
    pub(crate) dynamic_libs: Vec<String>,
    /// Target architecture, detected from input object file metadata.
    pub(crate) arch: Arch,
}

pub(crate) fn collect(objects: &[(String, Vec<u8>)]) -> Result<LinkState, LinkError> {
    // Flatten archives into individual object files before processing.
    let mut flat: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, data) in objects {
        if is_archive(data) {
            extract_archive(name, data, &mut flat)?;
        } else {
            flat.push((name.clone(), data.clone()));
        }
    }

    let mut state = LinkState {
        sections: Vec::new(),
        relocs: Vec::new(),
        globals: HashMap::new(),
        locals: HashMap::new(),
        tls_sections: Vec::new(),
        metadata: Vec::new(),
        dynamic_imports: HashSet::new(),
        dynamic_libs: Vec::new(),
        arch: Arch::Aarch64,
    };

    let mut sec_map: HashMap<(ObjIdx, object::SectionIndex), SectionIdx> = HashMap::new();

    for (obj_idx, (name, data)) in flat.iter().enumerate() {
        let obj_idx = ObjIdx(obj_idx);
        // ELF shared library input: extract dynamic symbols, skip section processing.
        // Shared libraries are always ELF, so try ELF-specific parse first.
        if let Ok(elf) = ElfFile64::parse(data.as_slice()) {
            if elf.elf_header().e_type.get(object::Endianness::Little) == elf::ET_DYN {
                collect_so_symbols(&elf, &mut state.globals, &mut state.dynamic_imports);
                let filename = std::path::Path::new(name)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if !state.dynamic_libs.contains(&filename) {
                    state.dynamic_libs.push(filename);
                }
                continue;
            }
        }

        // Generic parse: handles both ELF .o and COFF .o
        let obj = object::File::parse(data.as_slice())
            .map_err(|e| LinkError::Parse { file: name.clone(), message: e.to_string() })?;

        collect_object(&mut state, &obj, obj_idx, &mut sec_map)?;
    }

    Ok(state)
}

/// Extract exported dynamic symbols from an ET_DYN ELF (.so) and add them
/// to `globals` with a sentinel section index. These symbols satisfy undefined
/// references without contributing any code/data to the output.
fn collect_so_symbols(elf: &ElfFile64, globals: &mut HashMap<String, SymbolDef>, dynamic_imports: &mut HashSet<String>) {
    for sym in elf.dynamic_symbols() {
        let name = match sym.name() {
            Ok(n) if !n.is_empty() => n,
            _ => continue,
        };
        // Only defined symbols (not UND)
        if sym.is_undefined() {
            continue;
        }
        let name = name.to_string();
        globals.entry(name.clone()).or_insert(SymbolDef::Dynamic);
        dynamic_imports.insert(name);
    }
}

/// Resolve a relocation's target to a `SymbolRef`. Section symbols and local
/// symbols produce `SymbolRef::Local` (carrying the object index), while global
/// and undefined symbols produce `SymbolRef::Global`.
/// Strip the Mach-O leading `_` prefix from C symbol names so all internal
/// names use ELF convention. The prefix is re-added at Mach-O emit time.
fn demangle_macho(name: &str, is_macho: bool) -> String {
    if is_macho {
        name.strip_prefix('_').unwrap_or(name).to_string()
    } else {
        name.to_string()
    }
}

fn resolve_reloc_target(
    obj: &object::File,
    reloc: &read::Relocation,
    obj_idx: ObjIdx,
    sec_map: &HashMap<(ObjIdx, object::SectionIndex), SectionIdx>,
    state: &mut LinkState,
    is_macho: bool,
) -> Option<SymbolRef> {
    let sym_idx = match reloc.target() {
        read::RelocationTarget::Symbol(idx) => idx,
        // Absolute/Section targets have no symbol to resolve (non_exhaustive enum)
        _ => return None,
    };
    let sym = obj.symbol_by_index(sym_idx).ok()?;
    let name = sym.name().unwrap_or("");
    let name = &demangle_macho(name, is_macho);

    // Section symbols need unique synthetic names because COFF objects can have
    // multiple sections with the same name (e.g. many `.rdata` COMDAT sections).
    let is_section_sym = name.is_empty() || sym.kind() == read::SymbolKind::Section;
    if is_section_sym {
        let si = match sym.section() {
            read::SymbolSection::Section(si) => si,
            // Undefined/Absolute/Common symbols have no section (non_exhaustive enum)
            _ => return None,
        };
        let &gsec = sec_map.get(&(obj_idx, si))?;
        let syn = format!("__section_sym_{}_{}", obj_idx.0, gsec.0);
        let sec_addr = obj.section_by_index(si).map(|s| s.address()).unwrap_or(0);
        state.locals.entry((obj_idx, syn.clone())).or_insert(SymbolDef::Defined {
            section: gsec,
            value: sym.address() - sec_addr,
        });
        Some(SymbolRef::Local(obj_idx, syn))
    } else if sym.is_global() || sym.is_undefined() {
        Some(SymbolRef::Global(name.to_string()))
    } else {
        Some(SymbolRef::Local(obj_idx, name.to_string()))
    }
}

/// Collect sections, symbols, and relocations from a single object file.
/// Works with both ELF and COFF objects via the generic `Object` trait.
fn collect_object(
    state: &mut LinkState,
    obj: &object::File,
    obj_idx: ObjIdx,
    sec_map: &mut HashMap<(ObjIdx, object::SectionIndex), SectionIdx>,
) -> Result<(), LinkError> {
    if matches!(obj.architecture(), object::Architecture::X86_64) {
        state.arch = Arch::X86_64;
    }
    for section in obj.sections() {
        let sec_name = section.name().unwrap_or("");

        // Capture metadata sections (e.g. .rustc) regardless of SectionKind
        if sec_name.starts_with(".rustc") {
            let data = section.data().unwrap_or(&[]).to_vec();
            if !data.is_empty() {
                state.metadata.push((sec_name.to_string(), data));
            }
            continue;
        }

        let kind = match section.kind() {
            read::SectionKind::Text => SectionKind::Code,
            read::SectionKind::Data | read::SectionKind::Common => SectionKind::Data,
            read::SectionKind::ReadOnlyData
            | read::SectionKind::ReadOnlyDataWithRel
            | read::SectionKind::ReadOnlyString => SectionKind::ReadOnly,
            read::SectionKind::UninitializedData => SectionKind::Bss,
            read::SectionKind::Tls => SectionKind::Tls,
            read::SectionKind::UninitializedTls => SectionKind::TlsBss,
            read::SectionKind::TlsVariables => SectionKind::TlsVariables,
            read::SectionKind::Elf(elf::SHT_INIT_ARRAY) => SectionKind::InitArray,
            read::SectionKind::Elf(elf::SHT_FINI_ARRAY) => SectionKind::FiniArray,
            // Non-loadable sections — skip
            read::SectionKind::OtherString
            | read::SectionKind::Other
            | read::SectionKind::Debug
            | read::SectionKind::DebugString
            | read::SectionKind::Linker
            | read::SectionKind::Note
            | read::SectionKind::Metadata => continue,
            read::SectionKind::Elf(_) => continue,
            read::SectionKind::Unknown => continue,
            // non_exhaustive: panic on new variants so we notice them
            _ => panic!(
                "unhandled section kind {:?} in section {}",
                section.kind(),
                section.name().unwrap_or("<unnamed>"),
            ),
        };

        let sec_data = section.data().unwrap_or(&[]).to_vec();
        let global_idx = SectionIdx(state.sections.len());
        sec_map.insert((obj_idx, section.index()), global_idx);

        // Extract ELF merge/strings flags
        let (merge, strings, entsize) = match section.flags() {
            read::SectionFlags::Elf { sh_flags } => {
                let m = (sh_flags & elf::SHF_MERGE as u64) != 0;
                let s = (sh_flags & elf::SHF_STRINGS as u64) != 0;
                (m, s, if m { section.file_range().map(|_| {
                    // entsize from ELF section header — parse from raw header
                    // For ReadOnlyString, the object crate detects SHF_STRINGS
                    // but doesn't directly expose entsize via ObjectSection.
                    // Common convention: section name .rodata.str1.N → entsize = N
                    // Fallback to 1 for string sections
                    if s { 1u64 } else { 0 }
                }).unwrap_or(0) } else { 0 })
            }
            // Non-ELF flags (COFF, Mach-O) have no merge/strings concept (non_exhaustive enum)
            _ => (false, false, 0),
        };

        state.sections.push(InputSection {
            name: sec_name.to_string(),
            data: sec_data,
            align: section.align().max(1),
            size: section.size(),
            vaddr: None,
            kind,
            merge,
            strings,
            entsize,
        });

        if kind.is_tls() {
            state.tls_sections.push(global_idx);
        }
    }

    let is_macho = matches!(obj.format(), object::BinaryFormat::MachO);

    for symbol in obj.symbols() {
        let sym_name = match symbol.name() {
            Ok(n) if !n.is_empty() => demangle_macho(n, is_macho),
            _ => continue,
        };
        if symbol.is_undefined() {
            continue;
        }
        // Skip section symbols — relocations resolve these via synthetic
        // names keyed on section index, so they don't belong in locals.
        if symbol.kind() == read::SymbolKind::Section {
            continue;
        }
        let sec_idx = match symbol.section() {
            read::SymbolSection::Section(idx) => idx,
            // Undefined/Absolute/Common symbols have no section (non_exhaustive enum)
            _ => continue,
        };
        let global_sec = match sec_map.get(&(obj_idx, sec_idx)) {
            Some(&g) => g,
            None => continue,
        };
        let sec_addr = obj.section_by_index(sec_idx).map(|s| s.address()).unwrap_or(0);
        let def = SymbolDef::Defined {
            section: global_sec,
            value: symbol.address() - sec_addr,
        };
        if symbol.is_global() {
            // COFF weak externals: `.weak.FOO.default` (LLVM) or `.weak.FOO` (object crate)
            // provides the actual code for `FOO`. Register the alias as a global too.
            if let Some(rest) = sym_name.strip_prefix(".weak.") {
                let alias = rest.strip_suffix(".default").unwrap_or(rest);
                let alias = alias.to_string();
                match state.globals.get(&alias) {
                    Some(SymbolDef::Defined { .. }) => {}
                    _ => { state.globals.insert(alias, def); }
                }
            }
            // Concrete .o definitions always override .so dynamic imports
            match state.globals.get(&sym_name) {
                Some(SymbolDef::Defined { .. }) => {}
                _ => { state.globals.insert(sym_name, def); }
            }
        } else {
            if let Some(SymbolDef::Defined { section: existing_sec, .. }) = state.locals.get(&(obj_idx, sym_name.clone())) {
                assert_eq!(
                    *existing_sec, global_sec,
                    "local symbol {sym_name:?} in obj {} defined in two \
                     different sections ({} vs {})",
                    obj_idx.0, existing_sec.0, global_sec.0
                );
            }
            state.locals.insert((obj_idx, sym_name), def);
        }
    }

    for section in obj.sections() {
        let global_sec = match sec_map.get(&(obj_idx, section.index())) {
            Some(&g) => g,
            None => continue,
        };
        // BSS sections have no data to relocate
        if state.sections[global_sec].kind.is_nobits() { continue; }

        // Track pending Mach-O SUBTRACTOR relocation (first half of a
        // difference pair). The next relocation at the same offset must be
        // UNSIGNED — together they encode `symbolA - symbolB`.
        let mut pending_subtractor: Option<(u64, SymbolRef)> = None;

        for (offset, reloc) in section.relocations() {
            let sym_name = match resolve_reloc_target(obj, &reloc, obj_idx, sec_map, state, is_macho) {
                Some(name) => name,
                None => continue,
            };

            // Mach-O SUBTRACTOR: save the subtrahend and wait for the
            // paired UNSIGNED relocation at the same offset.
            if let RelocationFlags::MachO { r_type, .. } = reloc.flags() {
                if is_macho_subtractor(r_type, state.arch) {
                    assert!(pending_subtractor.is_none(),
                        "consecutive SUBTRACTOR without paired UNSIGNED");
                    pending_subtractor = Some((offset, sym_name));
                    continue;
                }
            }

            // Consume pending SUBTRACTOR: this relocation must be the
            // paired UNSIGNED at the same offset.
            let subtrahend = pending_subtractor.take().map(|(sub_offset, sub_sym)| {
                assert_eq!(sub_offset, offset,
                    "SUBTRACTOR at offset {sub_offset:#x} not paired with \
                     relocation at same offset (got {offset:#x})");
                sub_sym
            });

            let (r_type, is_macho_instruction) = match reloc.flags() {
                RelocationFlags::Elf { r_type } => match elf_to_reloc_type(r_type) {
                    Some(r) => (r, false),
                    // R_*_NONE (0) is a no-op padding relocation
                    None if r_type == 0 => continue,
                    None => return Err(LinkError::UnsupportedRawRelocation {
                        raw_type: format!("ELF {r_type}"),
                        symbol: sym_name.name().to_string(),
                    }),
                },
                RelocationFlags::Coff { typ } => match coff_to_reloc_type(typ) {
                    Some(r) => (r, false),
                    None => return Err(LinkError::UnsupportedRawRelocation {
                        raw_type: format!("COFF {typ}"),
                        symbol: sym_name.name().to_string(),
                    }),
                },
                RelocationFlags::MachO { r_type, r_length, .. } => {
                    let mapped = match state.arch {
                        Arch::X86_64 => macho_x86_64_to_reloc_type(r_type, r_length),
                        Arch::Aarch64 => {
                            let data = &state.sections[global_sec].data;
                            let off = offset as usize;
                            let insn = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
                            macho_arm64_to_reloc_type(r_type, r_length, insn)
                        }
                    };
                    match mapped {
                        // x86_64 Mach-O stores addends as raw data bytes (not
                        // instruction-encoded like ARM64 ADRP/LDR/BL).
                        Some(r) => (r, matches!(state.arch, Arch::Aarch64) && macho_arm64_is_instruction_reloc(r_type)),
                        None => return Err(LinkError::UnsupportedRawRelocation {
                            raw_type: format!("Mach-O type={r_type} length={r_length}"),
                            symbol: sym_name.name().to_string(),
                        }),
                    }
                }
                // Xcoff/Generic/etc. — reject unknown formats (non_exhaustive enum)
                flags => return Err(LinkError::UnsupportedRawRelocation {
                    raw_type: format!("{flags:?}"),
                    symbol: sym_name.name().to_string(),
                }),
            };

            // COFF and Mach-O data relocations use implicit addends stored in
            // section data. Mach-O instruction relocations (ADRP, LDR, BL) encode
            // the immediate in instruction bits — don't read raw bytes as addend.
            // SUBTRACTOR pairs: the data bytes contain the pre-link difference
            // which is replaced entirely — don't read them as an addend.
            let addend = if subtrahend.is_some() {
                0
            } else if is_macho_instruction {
                0
            } else if reloc.has_implicit_addend() {
                let data = &state.sections[global_sec].data;
                let off = offset as usize;
                let implicit = match reloc.size() {
                    64 => i64::from_le_bytes(data[off..off + 8].try_into().unwrap()),
                    32 => i32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as i64,
                    16 => i16::from_le_bytes(data[off..off + 2].try_into().unwrap()) as i64,
                    sz => panic!("unexpected implicit addend size {sz} bits for {:?}", sym_name),
                };
                reloc.addend() + implicit
            } else {
                reloc.addend()
            };

            state.relocs.push(InputReloc {
                section: global_sec,
                offset,
                r_type,
                target: sym_name,
                addend,
                subtrahend,
            });
        }
    }
    Ok(())
}

fn elf_to_reloc_type(r_type: u32) -> Option<RelocType> {
    // R_X86_64_NONE (0) is a no-op relocation that can appear in object files.
    // Other unknown types are rejected at the call site.
    Some(match r_type {
        elf::R_X86_64_64 => RelocType::X86_64,
        elf::R_X86_64_PC32 => RelocType::X86Pc32,
        elf::R_X86_64_PLT32 => RelocType::X86Plt32,
        elf::R_X86_64_32 => RelocType::X86_32,
        elf::R_X86_64_32S => RelocType::X86_32S,
        elf::R_X86_64_GOTPCREL => RelocType::X86Gotpcrel,
        elf::R_X86_64_GOTPCRELX => RelocType::X86Gotpcrelx,
        elf::R_X86_64_REX_GOTPCRELX => RelocType::X86RexGotpcrelx,
        elf::R_X86_64_TPOFF32 => RelocType::X86Tpoff32,
        elf::R_X86_64_GOTTPOFF => RelocType::X86Gottpoff,
        elf::R_X86_64_TLSGD => RelocType::X86Tlsgd,
        elf::R_X86_64_TLSLD => RelocType::X86Tlsld,
        elf::R_X86_64_DTPOFF32 => RelocType::X86Dtpoff32,
        elf::R_AARCH64_ABS64 => RelocType::Aarch64Abs64,
        elf::R_AARCH64_ABS32 => RelocType::Aarch64Abs32,
        elf::R_AARCH64_PREL32 => RelocType::Aarch64Prel32,
        elf::R_AARCH64_CALL26 => RelocType::Aarch64Call26,
        elf::R_AARCH64_JUMP26 => RelocType::Aarch64Jump26,
        elf::R_AARCH64_ADR_PREL_PG_HI21 => RelocType::Aarch64AdrPrelPgHi21,
        elf::R_AARCH64_ADD_ABS_LO12_NC => RelocType::Aarch64AddAbsLo12Nc,
        elf::R_AARCH64_LDST8_ABS_LO12_NC => RelocType::Aarch64Ldst8AbsLo12Nc,
        elf::R_AARCH64_LDST16_ABS_LO12_NC => RelocType::Aarch64Ldst16AbsLo12Nc,
        elf::R_AARCH64_LDST32_ABS_LO12_NC => RelocType::Aarch64Ldst32AbsLo12Nc,
        elf::R_AARCH64_LDST64_ABS_LO12_NC => RelocType::Aarch64Ldst64AbsLo12Nc,
        elf::R_AARCH64_LDST128_ABS_LO12_NC => RelocType::Aarch64Ldst128AbsLo12Nc,
        elf::R_AARCH64_MOVW_UABS_G0_NC => RelocType::Aarch64MovwUabsG0Nc,
        elf::R_AARCH64_MOVW_UABS_G1_NC => RelocType::Aarch64MovwUabsG1Nc,
        elf::R_AARCH64_MOVW_UABS_G2_NC => RelocType::Aarch64MovwUabsG2Nc,
        elf::R_AARCH64_MOVW_UABS_G3 => RelocType::Aarch64MovwUabsG3,
        elf::R_AARCH64_ADR_GOT_PAGE => RelocType::Aarch64AdrGotPage,
        elf::R_AARCH64_LD64_GOT_LO12_NC => RelocType::Aarch64Ld64GotLo12Nc,
        _ => return None,
    })
}

/// Map COFF x86_64 relocation types to RelocType.
fn coff_to_reloc_type(typ: u16) -> Option<RelocType> {
    Some(match typ {
        pe::IMAGE_REL_AMD64_ADDR64 => RelocType::X86_64,
        pe::IMAGE_REL_AMD64_ADDR32 => RelocType::X86_32,
        pe::IMAGE_REL_AMD64_ADDR32NB => RelocType::X86_32S,
        pe::IMAGE_REL_AMD64_REL32
        | pe::IMAGE_REL_AMD64_REL32_1
        | pe::IMAGE_REL_AMD64_REL32_2
        | pe::IMAGE_REL_AMD64_REL32_3
        | pe::IMAGE_REL_AMD64_REL32_4
        | pe::IMAGE_REL_AMD64_REL32_5 => RelocType::X86Plt32,
        pe::IMAGE_REL_AMD64_SECREL => RelocType::X86_32,
        _ => return None,
    })
}

/// Check if a Mach-O relocation is a SUBTRACTOR (first half of a difference pair).
fn is_macho_subtractor(r_type: u8, arch: Arch) -> bool {
    match arch {
        Arch::X86_64 => r_type == macho::X86_64_RELOC_SUBTRACTOR,
        Arch::Aarch64 => r_type == macho::ARM64_RELOC_SUBTRACTOR,
    }
}

/// Map Mach-O arm64 relocation types to RelocType.
///
/// `ARM64_RELOC_PAGEOFF12` is used for both ADD and LDR/STR instructions.
/// We inspect the instruction opcode to determine the correct ELF-style
/// relocation type (ADD vs LDST with appropriate scale).
fn macho_arm64_to_reloc_type(r_type: u8, r_length: u8, insn: u32) -> Option<RelocType> {
    Some(match r_type {
        macho::ARM64_RELOC_UNSIGNED if r_length == 3 => RelocType::Aarch64Abs64,
        macho::ARM64_RELOC_BRANCH26 => RelocType::Aarch64Call26,
        macho::ARM64_RELOC_PAGE21 => RelocType::Aarch64AdrPrelPgHi21,
        macho::ARM64_RELOC_PAGEOFF12 => classify_pageoff12(insn),
        macho::ARM64_RELOC_GOT_LOAD_PAGE21 => RelocType::Aarch64AdrGotPage,
        macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12 => RelocType::Aarch64Ld64GotLo12Nc,
        macho::ARM64_RELOC_POINTER_TO_GOT if r_length == 2 => RelocType::Aarch64GotPcrel32,
        macho::ARM64_RELOC_TLVP_LOAD_PAGE21 => RelocType::Aarch64TlvpLoadPage21,
        macho::ARM64_RELOC_TLVP_LOAD_PAGEOFF12 => classify_pageoff12(insn),
        _ => return None,
    })
}

/// Classify a Mach-O `ARM64_RELOC_PAGEOFF12` by inspecting the instruction.
/// ADD immediate: bits [28:24] = 10001
/// LDR/STR unsigned immediate: bits [27:24] = 1110 or 1111, bits [29:28] = size
fn classify_pageoff12(insn: u32) -> RelocType {
    if (insn >> 24) & 0x1F == 0b10001 {
        // ADD immediate
        RelocType::Aarch64AddAbsLo12Nc
    } else {
        // LDR/STR: size field in bits [31:30] gives the scale
        let size = insn >> 30;
        let is_v = (insn >> 26) & 1; // SIMD/FP bit
        if is_v == 1 && size == 0 {
            // 128-bit SIMD load/store: opc[1]=1 with size=0 and V=1
            RelocType::Aarch64Ldst128AbsLo12Nc
        } else {
            match size {
                0 => RelocType::Aarch64Ldst8AbsLo12Nc,
                1 => RelocType::Aarch64Ldst16AbsLo12Nc,
                2 => RelocType::Aarch64Ldst32AbsLo12Nc,
                3 => RelocType::Aarch64Ldst64AbsLo12Nc,
                _ => unreachable!(),
            }
        }
    }
}

/// Returns true if this Mach-O arm64 relocation type encodes its value inside
/// an instruction (as opposed to a raw data pointer).
fn macho_arm64_is_instruction_reloc(r_type: u8) -> bool {
    !matches!(r_type, macho::ARM64_RELOC_UNSIGNED | macho::ARM64_RELOC_SUBTRACTOR)
}

/// Map Mach-O x86_64 relocation types to RelocType.
fn macho_x86_64_to_reloc_type(r_type: u8, r_length: u8) -> Option<RelocType> {
    Some(match r_type {
        macho::X86_64_RELOC_UNSIGNED if r_length == 3 => RelocType::X86_64,
        macho::X86_64_RELOC_UNSIGNED if r_length == 2 => RelocType::X86_32,
        macho::X86_64_RELOC_SIGNED
        | macho::X86_64_RELOC_SIGNED_1
        | macho::X86_64_RELOC_SIGNED_2
        | macho::X86_64_RELOC_SIGNED_4 => RelocType::X86Pc32,
        macho::X86_64_RELOC_BRANCH => RelocType::X86Plt32,
        macho::X86_64_RELOC_GOT_LOAD => RelocType::X86Gotpcrelx,
        macho::X86_64_RELOC_GOT => RelocType::X86Gotpcrel,
        _ => return None,
    })
}

pub(crate) fn is_archive(data: &[u8]) -> bool {
    data.starts_with(b"!<arch>\n") || data.starts_with(b"!<thin>\n")
}

pub(crate) fn extract_archive(name: &str, data: &[u8], out: &mut Vec<(String, Vec<u8>)>) -> Result<(), LinkError> {
    let archive = object::read::archive::ArchiveFile::parse(data)
        .map_err(|e| LinkError::Parse { file: name.to_string(), message: e.to_string() })?;
    for member in archive.members() {
        let member = member
            .map_err(|e| LinkError::Parse { file: name.to_string(), message: e.to_string() })?;
        let member_name = String::from_utf8_lossy(member.name()).to_string();
        if !member_name.ends_with(".o") {
            continue;
        }
        let member_data = member.data(data)
            .map_err(|e| LinkError::Parse { file: format!("{name}({member_name})"), message: e.to_string() })?;
        out.push((format!("{name}({member_name})"), member_data.to_vec()));
    }
    Ok(())
}

pub(crate) fn find_lib(name: &str, paths: &[PathBuf]) -> Option<(String, Vec<u8>)> {
    let exact = [format!("lib{name}.rlib"), format!("lib{name}.a")];
    for dir in paths {
        for candidate in &exact {
            let path = dir.join(candidate);
            if let Ok(data) = fs::read(&path) {
                return Some((path.display().to_string(), data));
            }
        }
    }
    // Hash-suffixed Rust rlibs (e.g. libstd-abc123.rlib)
    let prefix = format!("lib{name}-");
    for dir in paths {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let fname = fname.to_string_lossy();
                if fname.starts_with(&prefix)
                    && (fname.ends_with(".rlib") || fname.ends_with(".a"))
                {
                    if let Ok(data) = fs::read(entry.path()) {
                        return Some((entry.path().display().to_string(), data));
                    }
                }
            }
        }
    }
    None
}

/// Quickly scan an object file for its defined and referenced (undefined) symbols.
/// Used for selective archive member extraction.
pub(crate) fn scan_symbols(data: &[u8]) -> (HashSet<String>, HashSet<String>) {
    let mut defined = HashSet::new();
    let mut referenced = HashSet::new();

    let obj = match read::File::parse(data) {
        Ok(o) => o,
        Err(e) => panic!("scan_symbols: failed to parse object: {e}"),
    };

    for sym in obj.symbols() {
        let name = match sym.name() {
            Ok(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };
        if sym.is_undefined() {
            referenced.insert(name);
        } else if sym.is_global() {
            defined.insert(name);
        }
    }

    (defined, referenced)
}

/// Remove unreachable sections (dead code elimination).
/// Roots: entry symbol's section + .init_array/.fini_array sections.
pub(crate) fn gc_sections(state: &mut LinkState, entry: &str) {
    let num_sections = state.sections.len();
    if num_sections == 0 { return; }

    // Resolve symbol name → section index
    let sym_to_section = |name: &str| -> Option<SectionIdx> {
        if let Some(SymbolDef::Defined { section, .. }) = state.globals.get(name) {
            return Some(*section);
        }
        None
    };

    // Build adjacency list: source_section → set of target sections
    let mut edges: Vec<HashSet<usize>> = vec![HashSet::new(); num_sections];
    for reloc in &state.relocs {
        for sym in std::iter::once(&reloc.target).chain(reloc.subtrahend.iter()) {
            if let Some(target_sec) = sym_to_section(sym.name()) {
                edges[reloc.section.0].insert(target_sec.0);
            }
            if let SymbolRef::Local(obj_idx, name) = sym {
                if let Some(SymbolDef::Defined { section: target_sec, .. }) = state.locals.get(&(*obj_idx, name.clone())) {
                    edges[reloc.section.0].insert(target_sec.0);
                }
            }
        }
    }

    // Find roots
    let mut reachable = vec![false; num_sections];
    let mut queue = std::collections::VecDeque::new();

    // Entry symbol's section
    if let Some(sec_idx) = sym_to_section(entry) {
        reachable[sec_idx.0] = true;
        queue.push_back(sec_idx.0);
    }

    // .init_array / .fini_array sections are always roots
    for (idx, sec) in state.sections.iter().enumerate() {
        if sec.kind == SectionKind::InitArray || sec.kind == SectionKind::FiniArray {
            if !reachable[idx] {
                reachable[idx] = true;
                queue.push_back(idx);
            }
        }
    }

    // BFS
    while let Some(idx) = queue.pop_front() {
        for &target in &edges[idx] {
            if !reachable[target] {
                reachable[target] = true;
                queue.push_back(target);
            }
        }
    }

    // Build old→new index mapping
    let mut remap = vec![SectionIdx(0); num_sections];
    let mut new_idx = 0;
    for old_idx in 0..num_sections {
        if reachable[old_idx] {
            remap[old_idx] = SectionIdx(new_idx);
            new_idx += 1;
        }
    }

    // Remove dead sections
    let mut new_sections = Vec::new();
    for (idx, sec) in state.sections.drain(..).enumerate() {
        if reachable[idx] {
            new_sections.push(sec);
        }
    }
    state.sections = new_sections;

    // Remap relocs and remove dead ones
    state.relocs.retain(|r| reachable[r.section.0]);
    for reloc in &mut state.relocs {
        reloc.section = remap[reloc.section.0];
    }

    // Remap globals
    state.globals.retain(|_, def| match def {
        SymbolDef::Dynamic => true,
        SymbolDef::Defined { section, .. } => reachable[section.0],
    });
    for def in state.globals.values_mut() {
        if let SymbolDef::Defined { section, .. } = def {
            *section = remap[section.0];
        }
    }

    // Remap locals
    state.locals.retain(|_, def| match def {
        SymbolDef::Dynamic => true,
        SymbolDef::Defined { section, .. } => reachable[section.0],
    });
    for def in state.locals.values_mut() {
        if let SymbolDef::Defined { section, .. } = def {
            *section = remap[section.0];
        }
    }

    // Remap tls_sections
    state.tls_sections.retain(|idx| reachable[idx.0]);
    for idx in &mut state.tls_sections {
        *idx = remap[idx.0];
    }
}

// ── Allocator shim synthesis ─────────────────────────────────────────────
// rustc normally generates these during final linking. We synthesize them
// as real code sections: each __rust_X is a `jmp __rdl_X` trampoline,
// and __rust_no_alloc_shim_is_unstable_v2 is a single `ret`.

const ALLOC_SHIMS: &[(&str, &str)] = &[
    (
        "_RNvCs2fcwfXhWpkc_7___rustc12___rust_alloc",
        "_RNvCs2fcwfXhWpkc_7___rustc11___rdl_alloc",
    ),
    (
        "_RNvCs2fcwfXhWpkc_7___rustc14___rust_dealloc",
        "_RNvCs2fcwfXhWpkc_7___rustc13___rdl_dealloc",
    ),
    (
        "_RNvCs2fcwfXhWpkc_7___rustc14___rust_realloc",
        "_RNvCs2fcwfXhWpkc_7___rustc13___rdl_realloc",
    ),
    (
        "_RNvCs2fcwfXhWpkc_7___rustc19___rust_alloc_zeroed",
        "_RNvCs2fcwfXhWpkc_7___rustc18___rdl_alloc_zeroed",
    ),
];

const SHIM_NO_ALLOC_UNSTABLE: &str =
    "_RNvCs2fcwfXhWpkc_7___rustc35___rust_no_alloc_shim_is_unstable_v2";

pub(crate) fn synthesize_alloc_shims(state: &mut LinkState) {
    // Only create shims for symbols that are actually referenced but undefined
    let undefined: HashSet<String> = state
        .relocs
        .iter()
        .map(|r| r.target.name().to_string())
        .filter(|name| !state.globals.contains_key(name.as_str()))
        .collect();

    // Each trampoline is: `jmp rel32` (E9 xx xx xx xx) = 5 bytes, padded to 16
    for &(shim_name, target_name) in ALLOC_SHIMS {
        if !undefined.contains(shim_name) {
            continue;
        }
        let mut code = vec![0xE9, 0, 0, 0, 0];
        code.resize(16, 0xCC); // pad with int3
        let sec_idx = SectionIdx(state.sections.len());
        state.sections.push(InputSection {
            name: format!(".text.{shim_name}"),
            data: code,
            align: 16,
            size: 16,
            vaddr: None,
            kind: SectionKind::Code,
            merge: false,
            strings: false,
            entsize: 0,
        });
        state.globals.insert(
            shim_name.to_string(),
            SymbolDef::Defined { section: sec_idx, value: 0 },
        );
        state.relocs.push(InputReloc {
            section: sec_idx,
            offset: 1,
            r_type: RelocType::X86Plt32,
            target: SymbolRef::Global(target_name.to_string()),
            addend: -4,
            subtrahend: None,
        });
    }

    // __rust_no_alloc_shim_is_unstable_v2: single `ret` (C3)
    if undefined.contains(SHIM_NO_ALLOC_UNSTABLE)
        && !state.globals.contains_key(SHIM_NO_ALLOC_UNSTABLE)
    {
        let mut code = vec![0xC3];
        code.resize(16, 0xCC);
        let sec_idx = SectionIdx(state.sections.len());
        state.sections.push(InputSection {
            name: format!(".text.{SHIM_NO_ALLOC_UNSTABLE}"),
            data: code,
            align: 16,
            size: 16,
            vaddr: None,
            kind: SectionKind::Code,
            merge: false,
            strings: false,
            entsize: 0,
        });
        state.globals.insert(
            SHIM_NO_ALLOC_UNSTABLE.to_string(),
            SymbolDef::Defined { section: sec_idx, value: 0 },
        );
    }
}

/// Merge SHF_MERGE|SHF_STRINGS sections with entsize=1 (null-terminated strings).
/// Deduplicates identical strings across input sections with the same name.
/// Updates symbol values and relocation addends to reflect merged offsets.
pub(crate) fn merge_string_sections(state: &mut LinkState) {
    // Find sections eligible for string merging
    let merge_indices: Vec<SectionIdx> = (0..state.sections.len())
        .filter(|&i| state.sections[i].merge && state.sections[i].strings && state.sections[i].entsize == 1)
        .map(SectionIdx)
        .collect();

    if merge_indices.is_empty() { return; }

    // Group by section name
    let mut groups: HashMap<String, Vec<SectionIdx>> = HashMap::new();
    for &idx in &merge_indices {
        groups.entry(state.sections[idx].name.clone()).or_default().push(idx);
    }

    // For each group with more than one section, merge them
    // offset_remap: (old_section_idx, old_offset) → new_offset in merged section
    let mut offset_remap: HashMap<(SectionIdx, u64), u64> = HashMap::new();
    let mut replaced_sections: HashSet<SectionIdx> = HashSet::new();

    for (_, group) in &groups {
        if group.len() < 2 { continue; }

        // Parse strings from all sections and intern them
        let mut string_map: HashMap<Vec<u8>, u64> = HashMap::new(); // string → offset in merged data
        let mut merged_data = Vec::new();

        for &sec_idx in group {
            let data = &state.sections[sec_idx].data;
            let mut pos = 0;
            while pos < data.len() {
                // Find null terminator
                let end = data[pos..].iter().position(|&b| b == 0)
                    .map(|p| pos + p + 1)
                    .unwrap_or(data.len());
                let string = &data[pos..end];

                let new_offset = if let Some(&existing_off) = string_map.get(string) {
                    existing_off
                } else {
                    let off = merged_data.len() as u64;
                    string_map.insert(string.to_vec(), off);
                    merged_data.extend_from_slice(string);
                    off
                };

                offset_remap.insert((sec_idx, pos as u64), new_offset);
                pos = end;
            }
        }

        // Replace the first section with merged data; mark the rest for removal
        let keep_idx = group[0];
        state.sections[keep_idx].data = merged_data;
        state.sections[keep_idx].size = state.sections[keep_idx].data.len() as u64;

        for &sec_idx in &group[1..] {
            replaced_sections.insert(sec_idx);
        }
    }

    if replaced_sections.is_empty() && offset_remap.is_empty() { return; }

    let merge_set: HashSet<SectionIdx> = merge_indices.iter().copied().collect();

    // Save old symbol state before remapping (needed for relocation addend updates)
    let old_globals: HashMap<String, SymbolDef> = state.globals.clone();
    let old_locals: HashMap<(ObjIdx, String), SymbolDef> = state.locals.clone();

    // Update symbol definitions pointing into merged sections
    for def in state.globals.values_mut() {
        if let SymbolDef::Defined { section, value } = def {
            if let Some(&new_off) = offset_remap.get(&(*section, *value)) {
                let old_name = &state.sections[*section].name;
                if let Some(group) = groups.get(old_name) {
                    *section = group[0];
                }
                *value = new_off;
            }
        }
    }
    for def in state.locals.values_mut() {
        if let SymbolDef::Defined { section, value } = def {
            if let Some(&new_off) = offset_remap.get(&(*section, *value)) {
                let old_name = &state.sections[*section].name;
                if let Some(group) = groups.get(old_name) {
                    *section = group[0];
                }
                *value = new_off;
            }
        }
    }

    // Update relocation addends: when a relocation targets a symbol in a merged
    // section, the addend may encode an offset into that section that needs remapping.
    for reloc in &mut state.relocs {
        let old_def = match &reloc.target {
            SymbolRef::Global(name) => old_globals.get(name),
            SymbolRef::Local(oi, name) => old_globals.get(name)
                .or_else(|| old_locals.get(&(*oi, name.clone()))),
        };
        if let Some(SymbolDef::Defined { section: old_sec, value: old_val }) = old_def {
            if !merge_set.contains(old_sec) { continue; }
            let old_offset = old_val + reloc.addend as u64;
            if let Some(&new_offset) = offset_remap.get(&(*old_sec, old_offset)) {
                let new_def = match &reloc.target {
                    SymbolRef::Global(name) => state.globals.get(name),
                    SymbolRef::Local(oi, name) => state.globals.get(name)
                        .or_else(|| state.locals.get(&(*oi, name.clone()))),
                };
                if let Some(SymbolDef::Defined { value: new_val, .. }) = new_def {
                    reloc.addend = new_offset as i64 - *new_val as i64;
                }
            }
        }
    }

    // Remove replaced sections and remap all indices
    if !replaced_sections.is_empty() {
        let mut index_map: Vec<Option<SectionIdx>> = Vec::with_capacity(state.sections.len());
        let mut new_idx = 0;
        for i in 0..state.sections.len() {
            if replaced_sections.contains(&SectionIdx(i)) {
                index_map.push(None);
            } else {
                index_map.push(Some(SectionIdx(new_idx)));
                new_idx += 1;
            }
        }

        // Remap section indices in symbols
        for def in state.globals.values_mut() {
            if let SymbolDef::Defined { section, .. } = def {
                if let Some(new) = index_map.get(section.0).and_then(|x| *x) {
                    *section = new;
                }
            }
        }
        for def in state.locals.values_mut() {
            if let SymbolDef::Defined { section, .. } = def {
                if let Some(new) = index_map.get(section.0).and_then(|x| *x) {
                    *section = new;
                }
            }
        }

        // Remap section indices in relocations, drop relocs targeting removed sections
        state.relocs.retain(|r| {
            index_map.get(r.section.0).and_then(|x| *x).is_some()
        });
        for reloc in &mut state.relocs {
            if let Some(new) = index_map.get(reloc.section.0).and_then(|x| *x) {
                reloc.section = new;
            }
        }

        // Remap TLS section indices
        state.tls_sections = state.tls_sections.iter()
            .filter_map(|idx| index_map.get(idx.0).and_then(|x| *x))
            .collect();

        // Remove the sections
        let mut i = 0;
        state.sections.retain(|_| {
            let keep = !replaced_sections.contains(&SectionIdx(i));
            i += 1;
            keep
        });
    }
}

/// Collect unique symbols in insertion order (deduplicating with a HashSet).
pub(crate) fn collect_unique_symbols<'a>(
    relocs: impl Iterator<Item = &'a InputReloc>,
    predicate: impl Fn(&InputReloc) -> bool,
) -> Vec<SymbolRef> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for reloc in relocs {
        if predicate(reloc) && seen.insert(reloc.target.clone()) {
            result.push(reloc.target.clone());
        }
    }
    result
}

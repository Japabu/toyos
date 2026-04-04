use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;
use std::{env, fs, process};

fn main() {
    let total_start = Instant::now();
    let args = parse_args();

    let t = Instant::now();
    let objects = match toyos_ld::resolve_libs_with_entry(&args.inputs, &args.lib_paths, &args.libs, Some(&args.entry)) {
        Ok(o) => o,
        Err(e) => {
            eprint!("toyos-ld: {e}");
            process::exit(1);
        }
    };
    let resolve_time = t.elapsed();

    if objects.is_empty() {
        eprintln!("toyos-ld: no input files");
        process::exit(1);
    }

    let t = Instant::now();
    let result = match args.format {
        OutputFormat::Shared => toyos_ld::link_shared_full(&objects, args.build_id),
        OutputFormat::Pe { subsystem } => toyos_ld::link_pe_with(&objects, &args.entry, subsystem.to_u16(), args.gc_sections),
        OutputFormat::Macho => toyos_ld::link_macho(&objects, &args.entry, args.gc_sections),
        OutputFormat::Static { image_base } => toyos_ld::link_static_full(&objects, &args.entry, image_base, args.gc_sections, args.build_id),
        OutputFormat::Pie => toyos_ld::link_full(&objects, &args.entry, args.gc_sections, args.build_id),
    };
    let link_time = t.elapsed();

    match result {
        Ok(output_bytes) => {
            // Write to a temp file then atomically rename. On macOS, overwriting
            // a signed binary in-place leaves a stale code-signature cache on the
            // old inode, causing the kernel to hang the next launch in _dyld_start.
            // Rename gives the output a fresh inode every time.
            let tmp = args.output.with_extension("tmp");
            fs::write(&tmp, &output_bytes).unwrap_or_else(|e| {
                eprintln!("toyos-ld: cannot write {}: {e}", tmp.display());
                process::exit(1);
            });
            fs::rename(&tmp, &args.output).unwrap_or_else(|e| {
                eprintln!("toyos-ld: cannot rename {} -> {}: {e}", tmp.display(), args.output.display());
                process::exit(1);
            });
            if let Some(map_path) = &args.map_file {
                let map = generate_map(&output_bytes, &args.output, &objects);
                fs::write(map_path, map).unwrap_or_else(|e| {
                    eprintln!("toyos-ld: cannot write map {}: {e}", map_path.display());
                    process::exit(1);
                });
            }
        }
        Err(e) => {
            eprint!("toyos-ld: {e}");
            process::exit(1);
        }
    }

    let output_name = args.output.file_name().unwrap_or_default().to_string_lossy();
    eprintln!("[toyos-ld] {output_name}: {:.3}s (resolve {:.3}s, link {:.3}s, {} objects, {:.1} MB)",
        total_start.elapsed().as_secs_f64(),
        resolve_time.as_secs_f64(),
        link_time.as_secs_f64(),
        objects.len(),
        objects.iter().map(|(_, d)| d.len()).sum::<usize>() as f64 / 1_048_576.0);
}

fn generate_map(output: &[u8], output_path: &PathBuf, inputs: &[(String, Vec<u8>)]) -> String {
    use object::read::{Object, ObjectSection, ObjectSymbol};

    let mut map = String::new();
    let _ = writeln!(map, "Linker Map: {}", output_path.display());
    let _ = writeln!(map);

    // Input files
    let _ = writeln!(map, "Input files:");
    for (name, data) in inputs {
        let _ = writeln!(map, "  {name} ({} bytes)", data.len());
    }
    let _ = writeln!(map);

    // Try parsing as ELF
    if let Ok(elf) = object::read::elf::ElfFile64::<object::Endianness>::parse(output) {
        // Sections
        let _ = writeln!(map, "Sections:");
        let _ = writeln!(map, "  {:>16}  {:>16}  {:>10}  {}", "Address", "Offset", "Size", "Name");
        for section in elf.sections() {
            let name = section.name().unwrap_or("<unknown>");
            if name.is_empty() { continue; }
            let _ = writeln!(map, "  {:>16x}  {:>16x}  {:>10x}  {}",
                section.address(), section.file_range().map(|(o, _)| o).unwrap_or(0),
                section.size(), name);
        }
        let _ = writeln!(map);

        // Symbols
        let _ = writeln!(map, "Symbols:");
        let _ = writeln!(map, "  {:>16}  {:>8}  {}", "Value", "Bind", "Name");
        let mut syms: Vec<_> = elf.symbols().collect();
        syms.sort_by_key(|s| s.address());
        for sym in syms {
            let name = sym.name().unwrap_or("");
            if name.is_empty() { continue; }
            let bind = if sym.is_global() { "GLOBAL" } else { "LOCAL" };
            let _ = writeln!(map, "  {:>16x}  {:>8}  {}", sym.address(), bind, name);
        }
    }

    map
}

enum PeSubsystem {
    EfiApplication,
    EfiBootServiceDriver,
    EfiRuntimeDriver,
}

impl PeSubsystem {
    fn to_u16(self) -> u16 {
        match self {
            PeSubsystem::EfiApplication => 10,
            PeSubsystem::EfiBootServiceDriver => 11,
            PeSubsystem::EfiRuntimeDriver => 12,
        }
    }
}

enum OutputFormat {
    Pie,
    Static { image_base: u64 },
    Shared,
    Pe { subsystem: PeSubsystem },
    Macho,
}

struct Args {
    output: PathBuf,
    entry: String,
    format: OutputFormat,
    gc_sections: bool,
    build_id: bool,
    map_file: Option<PathBuf>,
    inputs: Vec<PathBuf>,
    lib_paths: Vec<PathBuf>,
    libs: Vec<String>,
}

fn parse_args() -> Args {
    let argv: Vec<String> = env::args().collect();
    let mut output = PathBuf::from("a.out");
    let mut entry = String::from("_start");
    let mut shared = false;
    let mut is_static = false;
    let mut pe = false;
    let mut macho = false;
    let mut gc_sections = false;
    let mut build_id = false;
    let mut map_file: Option<PathBuf> = None;
    let mut image_base = 0x200000u64;
    let mut subsystem = PeSubsystem::EfiApplication;
    let mut inputs = Vec::new();
    let mut lib_paths = Vec::new();
    let mut libs = Vec::new();
    let mut i = 1;

    while i < argv.len() {
        match argv[i].as_str() {
            "-o" => { i += 1; output = PathBuf::from(&argv[i]); }
            "-e" | "--entry" => { i += 1; entry = argv[i].clone(); }
            "-L" => { i += 1; lib_paths.push(PathBuf::from(&argv[i])); }
            s if s.starts_with("-L") => { lib_paths.push(PathBuf::from(&s[2..])); }
            s if s.starts_with("-l") => { libs.push(s[2..].to_string()); }
            "--shared" | "-shared" => { shared = true; }
            "--static" => { is_static = true; }
            "--pe" => { pe = true; }
            "--macho" => { macho = true; }
            s if s.starts_with("--subsystem=") => {
                subsystem = parse_pe_subsystem(&s["--subsystem=".len()..]);
            }
            s if s.starts_with("--image-base=") => {
                let val = &s["--image-base=".len()..];
                image_base = if val.starts_with("0x") || val.starts_with("0X") {
                    u64::from_str_radix(&val[2..], 16).unwrap_or_else(|_| {
                        eprintln!("toyos-ld: invalid --image-base value: {val}");
                        process::exit(1);
                    })
                } else {
                    val.parse().unwrap_or_else(|_| {
                        eprintln!("toyos-ld: invalid --image-base value: {val}");
                        process::exit(1);
                    })
                };
            }
            // MSVC-style flags (from rustc MSVC linker flavor)
            s if s.starts_with("/OUT:") || s.starts_with("/out:") => {
                output = PathBuf::from(&s[5..]);
            }
            s if s.to_ascii_uppercase().starts_with("/ENTRY:") => {
                entry = s[7..].to_string();
                pe = true;
            }
            s if s.to_ascii_uppercase().starts_with("/SUBSYSTEM:") => {
                subsystem = parse_pe_subsystem(&s[11..]);
                pe = true;
            }
            s if s.to_ascii_uppercase().starts_with("/LIBPATH:") => {
                lib_paths.push(PathBuf::from(&s[9..]));
            }
            s if s.starts_with('/') && !s[1..].contains('/') && s.as_bytes().get(1).is_some_and(|c| c.is_ascii_uppercase()) => {
                // Ignore other MSVC flags (/NOLOGO, /DEBUG, /INCREMENTAL:NO, etc.)
            }
            // GNU-style flags
            "--gc-sections" => { gc_sections = true; }
            "--no-gc-sections" => { gc_sections = false; }
            "-Map" => { i += 1; map_file = Some(PathBuf::from(&argv[i])); }
            s if s.starts_with("-Map=") => { map_file = Some(PathBuf::from(&s[5..])); }
            "--build-id" => { build_id = true; }
            "-pie" | "--as-needed" | "--no-as-needed" | "--eh-frame-hdr"
            | "--hash-style=gnu" | "-Bstatic" | "-static"
            | "--no-dynamic-linker" => {}
            s if s.starts_with("-z") => { if s == "-z" { i += 1; } }
            s if s.starts_with("--") => {}
            s if s.starts_with('-') && s.len() > 1 => {}
            path => { inputs.push(PathBuf::from(path)); }
        }
        i += 1;
    }

    let format = if shared {
        OutputFormat::Shared
    } else if pe {
        OutputFormat::Pe { subsystem }
    } else if macho {
        OutputFormat::Macho
    } else if is_static {
        OutputFormat::Static { image_base }
    } else {
        OutputFormat::Pie
    };

    Args { output, entry, format, gc_sections, build_id, map_file, inputs, lib_paths, libs }
}

fn parse_pe_subsystem(val: &str) -> PeSubsystem {
    match val.to_ascii_lowercase().as_str() {
        "efi_application" | "10" => PeSubsystem::EfiApplication,
        "efi_boot_service_driver" | "11" => PeSubsystem::EfiBootServiceDriver,
        "efi_runtime_driver" | "12" => PeSubsystem::EfiRuntimeDriver,
        _ => {
            eprintln!("toyos-ld: invalid subsystem value: {val}");
            process::exit(1);
        }
    }
}

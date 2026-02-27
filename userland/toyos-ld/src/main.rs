use std::path::PathBuf;
use std::{env, fs, process};

fn main() {
    let args = parse_args();
    let objects = toyos_ld::resolve_libs(&args.inputs, &args.lib_paths, &args.libs);

    if objects.is_empty() {
        eprintln!("toyos-ld: no input files");
        process::exit(1);
    }

    let result = if args.shared {
        toyos_ld::link_shared(&objects)
    } else {
        toyos_ld::link(&objects, &args.entry)
    };

    match result {
        Ok(elf) => {
            fs::write(&args.output, &elf).unwrap_or_else(|e| {
                eprintln!("toyos-ld: cannot write {}: {e}", args.output.display());
                process::exit(1);
            });
        }
        Err(syms) => {
            for sym in &syms {
                eprintln!("toyos-ld: undefined symbol: {sym}");
            }
            process::exit(1);
        }
    }
}

struct Args {
    output: PathBuf,
    entry: String,
    shared: bool,
    inputs: Vec<PathBuf>,
    lib_paths: Vec<PathBuf>,
    libs: Vec<String>,
}

fn parse_args() -> Args {
    let argv: Vec<String> = env::args().collect();
    let mut output = PathBuf::from("a.out");
    let mut entry = String::from("_start");
    let mut shared = false;
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
            "-pie" | "--as-needed" | "--no-as-needed" | "--eh-frame-hdr"
            | "--hash-style=gnu" | "--build-id" | "-Bstatic" | "-static"
            | "--gc-sections" | "--no-gc-sections" | "--no-dynamic-linker" => {}
            s if s.starts_with("-z") => { if s == "-z" { i += 1; } }
            s if s.starts_with("--") => {}
            s if s.starts_with('-') && s.len() > 1 => {}
            path => { inputs.push(PathBuf::from(path)); }
        }
        i += 1;
    }

    Args { output, entry, shared, inputs, lib_paths, libs }
}

use std::path::PathBuf;
use std::{env, fs, process};

use toyos_cc::CompileOptions;

fn main() {
    // TCC has deeply nested expressions; use a larger stack
    std::thread::Builder::new()
        .stack_size(128 * 1024 * 1024)
        .spawn(|| run())
        .unwrap()
        .join()
        .unwrap();
}

fn run() {
    let mut args = parse_args();

    // Auto-add compiler's own include/ dir (lowest priority — after all user -I paths).
    // Binary is at target/{profile}/toyos-cc; include/ is at project root (two levels up).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(root) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            let include = root.join("include");
            if include.is_dir() {
                args.include_paths.push(include);
            }
        }
    }



    let mut link_objects: Vec<(String, Vec<u8>)> = Vec::new();

    let opts = CompileOptions {
        include_paths: args.include_paths.clone(),
        defines: args.defines.clone(),
        target: args.target.clone(),
        opt_level: args.opt_level,
        force_includes: args.force_includes.clone(),
    };

    for input in &args.inputs {
        // .o and .a files pass through to the linker
        if input.extension().is_some_and(|e| e == "o" || e == "a") {
            if !args.compile_only {
                let data = fs::read(input).unwrap_or_else(|e| {
                    panic!("toyos-cc: cannot read {}: {e}", input.display());
                });
                link_objects.push((input.display().to_string(), data));
            }
            continue;
        }

        let source = fs::read_to_string(input).unwrap_or_else(|e| {
            panic!("toyos-cc: cannot read {}: {e}", input.display());
        });

        if args.preprocess_only {
            let preprocessed = toyos_cc::preprocess_source(
                &source,
                &input.to_string_lossy(),
                &opts,
                args.suppress_line_markers,
            );
            print!("{preprocessed}");
            continue;
        }

        let object_bytes = toyos_cc::compile(&source, &input.to_string_lossy(), &opts);

        if args.compile_only {
            let output_path = args.output.clone().unwrap_or_else(|| input.with_extension("o"));
            fs::write(&output_path, &object_bytes).unwrap_or_else(|e| {
                panic!("toyos-cc: cannot write {}: {e}", output_path.display());
            });
        } else {
            link_objects.push((input.display().to_string(), object_bytes));
        }
    }

    if !args.compile_only && !args.preprocess_only {
        eprintln!("toyos-cc: linking...");
        let output = args.output.clone().unwrap_or_else(|| PathBuf::from("a.out"));
        let is_macho = args.target.as_ref()
            .map_or(cfg!(target_os = "macos"), |t| t.contains("apple"));
        let entry = if is_macho { "_main" } else { "main" };

        let result = if is_macho {
            toyos_ld::link_macho(&link_objects, entry, false)
        } else {
            toyos_ld::link_full(&link_objects, entry, false, false)
        };

        match result {
            Ok(bytes) => {
                fs::write(&output, bytes).unwrap_or_else(|e| {
                    panic!("toyos-cc: cannot write {}: {e}", output.display());
                });
            }
            Err(e) => {
                eprintln!("toyos-cc: link error: {e}");
                process::exit(1);
            }
        }
    }
}

struct Args {
    inputs: Vec<PathBuf>,
    output: Option<PathBuf>,
    include_paths: Vec<PathBuf>,
    force_includes: Vec<PathBuf>,
    defines: Vec<(String, String)>,
    target: Option<String>,
    compile_only: bool,          // -c
    preprocess_only: bool,       // -E
    suppress_line_markers: bool, // -P
    opt_level: u8,               // 0-3
}

fn parse_args() -> Args {
    let argv: Vec<String> = env::args().collect();
    let mut inputs = Vec::new();
    let mut output = None;
    let mut include_paths = Vec::new();
    let mut force_includes = Vec::new();
    let mut defines = Vec::new();
    let mut target = None;
    let mut compile_only = false;
    let mut preprocess_only = false;
    let mut suppress_line_markers = false;
    let mut opt_level: u8 = 0;
    let mut i = 1;

    while i < argv.len() {
        match argv[i].as_str() {
            "-o" => { i += 1; output = Some(PathBuf::from(&argv[i])); }
            "-c" => compile_only = true,
            "-E" => preprocess_only = true,
            "-P" => suppress_line_markers = true,
            "-v" | "--verbose" => {}
            "--target" => { i += 1; target = Some(argv[i].clone()); }
            "-I" => { i += 1; include_paths.push(PathBuf::from(&argv[i])); }
            s if s.starts_with("-I") => include_paths.push(PathBuf::from(&s[2..])),
            "-include" => { i += 1; force_includes.push(PathBuf::from(&argv[i])); }
            "-D" => {
                i += 1;
                let d = &argv[i];
                if let Some(eq) = d.find('=') {
                    defines.push((d[..eq].to_string(), d[eq + 1..].to_string()));
                } else {
                    defines.push((d.clone(), "1".to_string()));
                }
            }
            s if s.starts_with("-D") => {
                let d = &s[2..];
                if let Some(eq) = d.find('=') {
                    defines.push((d[..eq].to_string(), d[eq + 1..].to_string()));
                } else {
                    defines.push((d.to_string(), "1".to_string()));
                }
            }
            s if s.starts_with("--target=") => target = Some(s["--target=".len()..].to_string()),
            // Ignore common flags we don't support yet
            "-w" | "-Wall" | "-Wextra" | "-Werror" | "-pedantic" | "-g" | "-g3" => {}
            "-O0" => opt_level = 0,
            "-O1" => opt_level = 1,
            "-O2" | "-Os" | "-Oz" => opt_level = 2,
            "-O3" => opt_level = 3,
            s if s.starts_with("-O") || s.starts_with("-W") || s.starts_with("-f")
              || s.starts_with("-m") || s.starts_with("-std=") || s.starts_with("-march")
              || s.starts_with("-x") => {}
            "-pipe" | "-pthread" | "-ldl" | "-lm" | "-lc" => {}
            s if s.starts_with("-l") || s.starts_with("-L") => {}
            s if s.starts_with('-') => {
                eprintln!("toyos-cc: warning: ignoring unknown flag: {s}");
            }
            path => inputs.push(PathBuf::from(path)),
        }
        i += 1;
    }

    if inputs.is_empty() && !preprocess_only {
        eprintln!("toyos-cc: no input files");
        process::exit(1);
    }

    Args { inputs, output, include_paths, force_includes, defines, target, compile_only, preprocess_only, suppress_line_markers, opt_level }
}

#[macro_use]
mod verbose;
#[allow(dead_code)] // AST grammar: fields parsed but not all consumed by codegen yet
mod ast;
mod codegen;
mod emit;
mod lex;
mod parse;
mod preprocess;
#[allow(dead_code)] // type model: methods defined for completeness
mod types;

use std::path::PathBuf;
use std::{env, fs, process};

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

    if args.verbose {
        verbose::set(true);
        eprintln!("toyos-cc: verbose mode enabled");
    }

    if !args.preprocess_only {
        eprintln!("toyos-cc: starting");
    }

    for input in &args.inputs {
        let source = fs::read_to_string(input).unwrap_or_else(|e| {
            panic!("toyos-cc: cannot read {}: {e}", input.display());
        });

        if !args.preprocess_only {
            eprintln!("toyos-cc: preprocessing...");
        }
        let mut pp = preprocess::Preprocessor::new(args.include_paths.clone(), args.defines.clone(), args.target.as_deref());
        pp.suppress_line_markers = args.suppress_line_markers;
        let preprocessed = pp.preprocess(&source, &input.to_string_lossy());
        if !args.preprocess_only {
            eprintln!("toyos-cc: preprocessing done, {} bytes", preprocessed.len());
        }

        if args.preprocess_only {
            print!("{preprocessed}");
            continue;
        }

        eprintln!("toyos-cc: lexing...");
        let lexer = lex::Lexer::new(&preprocessed, &input.to_string_lossy());
        let tokens = lexer.tokenize();
        eprintln!("toyos-cc: lexing done, {} tokens", tokens.len());

        eprintln!("toyos-cc: parsing...");
        let parser = parse::Parser::new(tokens);
        let (tu, type_env) = parser.parse();
        eprintln!("toyos-cc: parsing done, {} decls", tu.len());

        eprintln!("toyos-cc: codegen...");
        let obj_name = args.output.as_ref()
            .map(|o| o.to_string_lossy().into_owned())
            .unwrap_or_else(|| input.with_extension("o").to_string_lossy().into_owned());

        let module = emit::create_module(&obj_name, args.target.as_deref());
        let mut cg = codegen::Codegen::new(module, type_env);
        cg.compile_unit(&tu);

        let object_bytes = emit::finish(cg.module);

        let output_path = if args.compile_only {
            args.output.clone().unwrap_or_else(|| input.with_extension("o"))
        } else {
            panic!("linking not supported");
        };

        if args.compile_only {
            fs::write(&output_path, &object_bytes).unwrap_or_else(|e| {
                panic!("toyos-cc: cannot write {}: {e}", output_path.display());
            });
        } else {
            panic!("linking not supported");
        }
    }
}

struct Args {
    inputs: Vec<PathBuf>,
    output: Option<PathBuf>,
    include_paths: Vec<PathBuf>,
    defines: Vec<(String, String)>,
    target: Option<String>,
    compile_only: bool,          // -c
    preprocess_only: bool,       // -E
    suppress_line_markers: bool, // -P
    verbose: bool,               // -v / --verbose
}

fn parse_args() -> Args {
    let argv: Vec<String> = env::args().collect();
    let mut inputs = Vec::new();
    let mut output = None;
    let mut include_paths = Vec::new();
    let mut defines = Vec::new();
    let mut target = None;
    let mut compile_only = false;
    let mut preprocess_only = false;
    let mut suppress_line_markers = false;
    let mut verbose = false;
    let mut i = 1;

    while i < argv.len() {
        match argv[i].as_str() {
            "-o" => { i += 1; output = Some(PathBuf::from(&argv[i])); }
            "-c" => compile_only = true,
            "-E" => preprocess_only = true,
            "-P" => suppress_line_markers = true,
            "-v" | "--verbose" => verbose = true,
            "--target" => { i += 1; target = Some(argv[i].clone()); }
            "-I" => { i += 1; include_paths.push(PathBuf::from(&argv[i])); }
            s if s.starts_with("-I") => include_paths.push(PathBuf::from(&s[2..])),
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
            "-w" | "-Wall" | "-Wextra" | "-Werror" | "-pedantic" | "-g" | "-g3"
            | "-O0" | "-O1" | "-O2" | "-O3" | "-Os" | "-Oz" => {}
            s if s.starts_with("-O") || s.starts_with("-W") || s.starts_with("-f")
              || s.starts_with("-m") || s.starts_with("-std=") || s.starts_with("-march") => {}
            "-pipe" | "-pthread" | "-ldl" | "-lm" | "-lc" => {}
            s if s.starts_with("-l") || s.starts_with("-L") => {} // linker flags
            s if s.starts_with('-') => {
                eprintln!("toyos-cc: warning: unknown flag: {s}");
                process::exit(1);
            }
            path => inputs.push(PathBuf::from(path)),
        }
        i += 1;
    }

    if inputs.is_empty() && !preprocess_only {
        eprintln!("toyos-cc: no input files");
        process::exit(1);
    }

    Args { inputs, output, include_paths, defines, target, compile_only, preprocess_only, suppress_line_markers, verbose }
}

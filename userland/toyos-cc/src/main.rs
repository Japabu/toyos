mod ast;
mod codegen;
mod emit;
mod lex;
mod parse;
mod preprocess;
mod types;

use std::path::PathBuf;
use std::{env, fs, process};

fn main() {
    let args = parse_args();

    for input in &args.inputs {
        let source = fs::read_to_string(input).unwrap_or_else(|e| {
            eprintln!("toyos-cc: cannot read {}: {e}", input.display());
            process::exit(1);
        });

        // Preprocess
        let mut pp = preprocess::Preprocessor::new(args.include_paths.clone(), args.defines.clone());
        let preprocessed = pp.preprocess(&source, &input.to_string_lossy());

        if args.preprocess_only {
            print!("{preprocessed}");
            continue;
        }

        // Lex
        let lexer = lex::Lexer::new(&preprocessed, &input.to_string_lossy());
        let tokens = lexer.tokenize();

        // Parse
        let parser = parse::Parser::new(tokens);
        let (tu, type_env) = parser.parse();

        // Codegen
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
            args.output.clone().unwrap_or_else(|| PathBuf::from("a.out"))
        };

        if args.compile_only {
            // Write object file
            fs::write(&output_path, &object_bytes).unwrap_or_else(|e| {
                eprintln!("toyos-cc: cannot write {}: {e}", output_path.display());
                process::exit(1);
            });
        } else {
            // Write object file temporarily, then link
            let tmp_obj = input.with_extension("o");
            fs::write(&tmp_obj, &object_bytes).unwrap_or_else(|e| {
                eprintln!("toyos-cc: cannot write {}: {e}", tmp_obj.display());
                process::exit(1);
            });

            // Link with toyos-ld (or system linker)
            let linker = env::var("CC_LINKER").unwrap_or_else(|_| "toyos-ld".to_string());
            let status = process::Command::new(&linker)
                .arg("-o")
                .arg(&output_path)
                .arg(&tmp_obj)
                .status()
                .unwrap_or_else(|e| {
                    eprintln!("toyos-cc: cannot run linker '{}': {e}", linker);
                    process::exit(1);
                });

            let _ = fs::remove_file(&tmp_obj);

            if !status.success() {
                eprintln!("toyos-cc: linker failed");
                process::exit(1);
            }
        }
    }
}

struct Args {
    inputs: Vec<PathBuf>,
    output: Option<PathBuf>,
    include_paths: Vec<PathBuf>,
    defines: Vec<(String, String)>,
    target: Option<String>,
    compile_only: bool,    // -c
    preprocess_only: bool, // -E
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
    let mut i = 1;

    while i < argv.len() {
        match argv[i].as_str() {
            "-o" => { i += 1; output = Some(PathBuf::from(&argv[i])); }
            "-c" => compile_only = true,
            "-E" => preprocess_only = true,
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
            }
            path => inputs.push(PathBuf::from(path)),
        }
        i += 1;
    }

    if inputs.is_empty() && !preprocess_only {
        eprintln!("toyos-cc: no input files");
        process::exit(1);
    }

    Args { inputs, output, include_paths, defines, target, compile_only, preprocess_only }
}

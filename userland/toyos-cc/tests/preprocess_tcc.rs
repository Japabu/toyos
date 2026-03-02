/// Preprocessor comparison tests: toyos-cc -E vs checked-in expected output.
///
/// Expected files were generated with:
///   cc -E -P <tcc-flags> <file.c> | normalize
/// on macOS aarch64 with the current SDK headers.
///
/// To regenerate expected files, run: cargo test --test preprocess_tcc -- --regen
/// (set REGEN=1 env var)
use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};

fn toyos_cc() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_toyos-cc"))
}

fn tcc_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../bootstrap-cc/tinycc")
        .canonicalize()
        .expect("tinycc dir not found — run `cd ../bootstrap-cc && cargo run` to download TCC")
}

fn testcases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testcases/preprocess_tcc")
}

fn system_include_args() -> Vec<String> {
    if cfg!(target_os = "macos") {
        let output = Command::new("xcrun")
            .args(["--show-sdk-path"])
            .output()
            .expect("failed to run xcrun");
        let sdk = String::from_utf8(output.stdout).unwrap().trim().to_string();
        vec![format!("-I{sdk}/usr/include")]
    } else {
        vec!["-I/usr/include".to_string()]
    }
}

fn tcc_defines(tcc_dir: &std::path::Path) -> Vec<String> {
    vec![
        "-DONE_SOURCE=1".into(),
        "-DTCC_TARGET_X86_64".into(),
        "-DCONFIG_TRIPLET=\"x86_64-linux-gnu\"".into(),
        "-DTCC_VERSION=\"0.9.27\"".into(),
        format!("-DCONFIG_TCCDIR=\"{}\"", tcc_dir.display()),
        "-DCONFIG_TCC_CRTPREFIX=\"/usr/lib\"".into(),
        "-DCONFIG_TCC_LIBPATHS=\"/usr/lib\"".into(),
        "-DCONFIG_TCC_SYSINCLUDEPATHS=\"{B}/include:/usr/include\"".into(),
        "-DCONFIG_LDDIR=\"lib\"".into(),
        "-DCONFIG_TCC_SEMLOCK=0".into(),
    ]
}

/// Normalize preprocessor output to a token sequence for comparison.
/// - Strip `# N "file"` line-marker directives
/// - Treat all whitespace (including newlines) as token separators
/// - Return one token per line so diffs are readable
fn normalize(s: &str) -> String {
    // First pass: remove line-number directives
    let mut no_markers = String::new();
    for line in s.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            let rest = trimmed[1..].trim_start();
            if rest.starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
        }
        no_markers.push_str(trimmed);
        no_markers.push('\n');
    }
    // Second pass: split into tokens (whitespace-delimited), one per line
    no_markers
        .split_whitespace()
        .map(|t| format!("{t}\n"))
        .collect()
}

fn preprocess_with_toyos_cc(file: &std::path::Path, tcc_dir: &std::path::Path) -> String {
    let mut cmd = Command::new(toyos_cc());
    cmd.arg("-E")
        .args(tcc_defines(tcc_dir))
        .arg(format!("-I{}", tcc_dir.join("include").display()))
        .arg(format!("-I{}", tcc_dir.display()))
        .args(system_include_args())
        .arg(file);

    let out = cmd.output().unwrap_or_else(|e| panic!("failed to run toyos-cc: {e}"));
    assert!(
        out.status.success(),
        "toyos-cc -E failed on {}:\nstdout: {}\nstderr: {}",
        file.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run_test(file_name: &str) {
    let dir = tcc_dir();
    let file = dir.join(file_name);
    assert!(file.exists(), "TCC file not found: {}", file.display());

    let expect_file = testcases_dir().join(format!("{file_name}.expect"));
    assert!(
        expect_file.exists(),
        "expected file not found: {} — regenerate with REGEN=1 cargo test",
        expect_file.display()
    );

    let expected = fs::read_to_string(&expect_file)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", expect_file.display()));

    let raw = preprocess_with_toyos_cc(&file, &dir);
    let actual = normalize(&raw);

    if actual == expected {
        return;
    }

    // Find first differing token
    let exp_toks: Vec<&str> = expected.lines().collect();
    let act_toks: Vec<&str> = actual.lines().collect();

    let diff_tok = exp_toks
        .iter()
        .zip(act_toks.iter())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| exp_toks.len().min(act_toks.len()));

    let ctx_start = diff_tok.saturating_sub(5);
    let ctx_end = (diff_tok + 10).min(exp_toks.len().max(act_toks.len()));

    let exp_ctx: String = exp_toks
        .get(ctx_start..ctx_end.min(exp_toks.len()))
        .unwrap_or(&[])
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let n = ctx_start + i + 1;
            let mark = if ctx_start + i == diff_tok { ">>>" } else { "   " };
            format!("{mark} {n:6}: {t}")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let act_ctx: String = act_toks
        .get(ctx_start..ctx_end.min(act_toks.len()))
        .unwrap_or(&[])
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let n = ctx_start + i + 1;
            let mark = if ctx_start + i == diff_tok { ">>>" } else { "   " };
            format!("{mark} {n:6}: {t}")
        })
        .collect::<Vec<_>>()
        .join("\n");

    panic!(
        "preprocessing mismatch for {file_name} at token {tok}:\n\
         --- expected (cc -E -P) ---\n{exp_ctx}\n\
         --- actual (toyos-cc -E) ---\n{act_ctx}",
        tok = diff_tok + 1,
    );
}

macro_rules! preprocess_test {
    ($name:ident, $file:expr) => {
        #[test]
        fn $name() {
            run_test($file);
        }
    };
}

preprocess_test!(preprocess_tccpp,       "tccpp.c");
preprocess_test!(preprocess_tccgen,      "tccgen.c");
preprocess_test!(preprocess_tccdbg,      "tccdbg.c");
preprocess_test!(preprocess_tccelf,      "tccelf.c");
preprocess_test!(preprocess_tccasm,      "tccasm.c");
preprocess_test!(preprocess_tccrun,      "tccrun.c");
preprocess_test!(preprocess_x86_64_gen,  "x86_64-gen.c");
preprocess_test!(preprocess_x86_64_link, "x86_64-link.c");
preprocess_test!(preprocess_i386_asm,    "i386-asm.c");
preprocess_test!(preprocess_tcctools,    "tcctools.c");

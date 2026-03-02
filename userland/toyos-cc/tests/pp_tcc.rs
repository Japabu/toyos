/// Preprocessor tests from the TinyCC pp test suite.
///
/// Each test runs `toyos-cc -E -P <file>` and compares against the checked-in
/// `.expect` file (which were generated with `gcc -E -P`).
use std::path::PathBuf;
use std::process::Command;
use std::fs;

fn toyos_cc() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_toyos-cc"))
}

fn testcases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testcases/pp_tcc")
}

/// Normalize whitespace: trim each line, collapse internal runs of whitespace
/// to a single space, and strip blank lines. Mirrors `diff -b`.
fn normalize(s: &str) -> String {
    s.lines()
        .map(|l| {
            l.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strip all whitespace. Mirrors `diff -w` (used for test 02).
fn strip_whitespace(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn run_test(name: &str, ignore_all_whitespace: bool) {
    let dir = testcases_dir();

    // Find source file (.c or .S)
    let c_file = dir.join(format!("{name}.c"));
    let s_file = dir.join(format!("{name}.S"));
    let src = if c_file.exists() {
        c_file
    } else if s_file.exists() {
        s_file
    } else {
        panic!("no source file for test {name}");
    };

    let expect_file = dir.join(format!("{name}.expect"));
    let expected = fs::read_to_string(&expect_file)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", expect_file.display()));

    // Run from the testcases dir with just the filename so warnings use short paths.
    let src_name = src.file_name().unwrap().to_str().unwrap().to_string();
    let out = Command::new(toyos_cc())
        .arg("-E")
        .arg("-P")
        .arg(&src_name)
        .current_dir(&dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run toyos-cc: {e}"));

    assert!(
        out.status.success(),
        "toyos-cc -E -P failed on {name}:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Combine stdout and stderr (warnings like redefinition warnings go to stderr).
    let actual = String::from_utf8_lossy(&out.stdout).into_owned()
        + &String::from_utf8_lossy(&out.stderr);

    if ignore_all_whitespace {
        assert_eq!(
            strip_whitespace(&actual),
            strip_whitespace(&expected),
            "output mismatch for {name} (ignoring whitespace)\n--- expected ---\n{expected}\n--- actual ---\n{actual}",
        );
    } else {
        assert_eq!(
            normalize(&actual),
            normalize(&expected),
            "output mismatch for {name}\n--- expected ---\n{expected}\n--- actual ---\n{actual}",
        );
    }
}

macro_rules! pp_test {
    ($rust_name:ident, $file:expr) => {
        #[test]
        fn $rust_name() {
            run_test($file, false);
        }
    };
    ($rust_name:ident, $file:expr, ignore_whitespace) => {
        #[test]
        fn $rust_name() {
            run_test($file, true);
        }
    };
}

pp_test!(pp01, "01");
pp_test!(pp02, "02", ignore_whitespace);
pp_test!(pp03, "03");
pp_test!(pp04, "04");
pp_test!(pp05, "05");
pp_test!(pp06, "06");
pp_test!(pp07, "07");
pp_test!(pp08, "08");
pp_test!(pp09, "09");
pp_test!(pp10, "10");
pp_test!(pp11, "11");
pp_test!(pp13, "13");
pp_test!(pp14, "14");
pp_test!(pp15, "15");
pp_test!(pp16, "16");
pp_test!(pp17, "17");
pp_test!(pp18, "18");
pp_test!(pp19, "19");
pp_test!(pp20, "20");
pp_test!(pp21, "21");
pp_test!(pp22, "22");
pp_test!(pp23, "23");
pp_test!(pp24, "24");
pp_test!(pp_counter, "pp-counter");

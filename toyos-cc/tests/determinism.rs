//! One binary, one source, the same bytes out. Without that, no object hash can
//! be cached, no `cmp` on a `.o` is evidence that a compiler change was inert,
//! and the image the owner flashes is not the one anyone else builds.
//!
//! Each case below is a shape whose emission order once came out of a hash
//! container, and each is written so that a lost order shows in the bytes:
//! the symbols are same-sized and interchangeable, so nothing but their order
//! distinguishes one emission from another.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Compiles in one process rather than across several: `RandomState::new`
/// advances a per-thread counter, so consecutive maps in one process are seeded
/// as differently as maps in two processes are, and the test pays for one
/// compile each instead of one exec each.
const RUNS: usize = 8;

fn options(include_paths: Vec<PathBuf>) -> toyos_cc::CompileOptions {
    toyos_cc::CompileOptions {
        include_paths,
        target: Some("x86_64-unknown-toyos".to_string()),
        ..Default::default()
    }
}

/// The one failure this file reports: which run diverged, and where.
fn assert_stable(case: &str, source: &str, include_paths: Vec<PathBuf>) {
    let opts = options(include_paths);
    let first = toyos_cc::compile(source, &format!("{case}.c"), &opts);
    for run in 1..RUNS {
        let again = toyos_cc::compile(source, &format!("{case}.c"), &opts);
        if again != first {
            let at = first
                .iter()
                .zip(&again)
                .position(|(a, b)| a != b)
                .map(|i| format!("byte {i}"))
                .unwrap_or_else(|| format!("length {} vs {}", first.len(), again.len()));
            panic!("{case}: run {run} differs from run 0 at {at}");
        }
    }
}

/// Tentative definitions land in BSS, and interchangeable ones let any
/// permutation of them produce a valid object of the same size — which is how
/// `d_event.c`'s `eventhead`, `events` and `eventtail` traded offsets.
#[test]
fn tentative_globals_keep_their_order() {
    let mut source = String::new();
    for i in 0..12 {
        writeln!(source, "int tentative_{i}[4];").unwrap();
    }
    source.push_str("int main(void) { return tentative_0[0]; }\n");
    assert_stable("tentative", &source, vec![]);
}

/// x86_64 calls a variadic function through a hand-emitted stub, one per callee.
#[test]
fn variadic_call_stubs_keep_their_order() {
    let mut source = String::new();
    for i in 0..12 {
        writeln!(source, "extern int variadic_{i}(int, ...);").unwrap();
    }
    source.push_str("int main(void) { return");
    for i in 0..12 {
        write!(source, " {} variadic_{i}(1, 2)", if i == 0 { "" } else { "+" }).unwrap();
    }
    source.push_str("; }\n");
    assert_stable("variadic", &source, vec![]);
}

/// A parameter whose address is taken is spilled to a stack slot in the entry
/// block, and the slots are laid out in the order they are created.
#[test]
fn spilled_parameters_keep_their_order() {
    let mut source = String::new();
    source.push_str("extern void sink(int *);\n");
    source.push_str("int f(");
    for i in 0..6 {
        write!(source, "{}int p{i}", if i == 0 { "" } else { ", " }).unwrap();
    }
    source.push_str(") {\n");
    for i in 0..6 {
        writeln!(source, "  sink(&p{i});").unwrap();
    }
    source.push_str("  return 0;\n}\n");
    assert_stable("spilled", &source, vec![]);
}

/// Breadth against the shapes nobody thought to write a case for. Driven
/// through the binary, not the library: a build is a process per file, so this
/// is the seeding a build actually sees, and a case this compiler refuses drops
/// out by its exit status instead of needing a list that would rot.
#[test]
fn the_tinycc_corpus_compiles_to_the_same_bytes() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/testcases/tinycc")
        .canonicalize()
        .expect("tinycc corpus");
    let mut sources: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read corpus")
        .map(|e| e.expect("corpus entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "c"))
        .collect();
    sources.sort();

    let scratch = std::env::temp_dir().join(format!("toyos-cc-determinism-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch dir");

    let mut compiled = 0usize;
    let mut unstable = Vec::new();
    for path in &sources {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let mut objects = Vec::new();
        for run in 0..2 {
            let out = scratch.join(format!("{name}.{run}.o"));
            let status = Command::new(env!("CARGO_BIN_EXE_toyos-cc"))
                .args(["-c", "--target", "x86_64-unknown-toyos"])
                .arg("-I")
                .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../userland/libc/include"))
                .arg("-I")
                .arg(&dir)
                .arg(path)
                .arg("-o")
                .arg(&out)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("run toyos-cc");
            if !status.success() {
                break;
            }
            objects.push(std::fs::read(&out).expect("read object"));
        }
        if objects.len() < 2 {
            continue;
        }
        compiled += 1;
        if objects[0] != objects[1] {
            unstable.push(name);
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        compiled * 2 > sources.len(),
        "only {compiled} of {} corpus cases compiled — this gate has gone vacuous",
        sources.len(),
    );
    assert!(unstable.is_empty(), "{} cases are not reproducible: {unstable:?}", unstable.len());
}

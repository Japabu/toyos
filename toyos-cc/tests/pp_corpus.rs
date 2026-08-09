//! `tests/testcases/pp_tcc/` — 25 preprocessor cases with `.expect` files,
//! committed and attributed since 2026-08-08 and read by nothing until now.
//!
//! Three things about the protocol, because four of the twenty-five verdicts
//! move if any of them is got wrong.
//!
//! **The compared stream is the output and the diagnostics together.** `16`'s
//! entire expected output is a warning; a driver that reads stdout alone reds
//! it for ever and for no defect. That is why this drives the *binary* rather
//! than the library: the diagnostics are `eprintln!` on the process's own
//! stderr, and capturing those in-process needs a file descriptor the library
//! does not offer.
//!
//! **The source is named as its `.expect` names it.** `23.expect` contains
//! `40 "23.S"`, so `__FILE__` has to be the bare basename, which means running
//! from inside the corpus directory.
//!
//! **The normalisation is written down here rather than delegated to a tool.**
//! tcc's own harness pipes through `diff -bB`, and what that means differs
//! between implementations: this host's BSD `diff` reports a moved blank line
//! as a change where GNU diffutils would not, and GNU is not installed here so
//! the two cannot be compared. The rule below is what §5.1's verdicts were
//! taken under and it decides `05`, `16` and `24` by itself.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What a case does that is not "matches its `.expect`".
enum Outcome {
    /// The normalised output differs from the normalised `.expect`, at these
    /// positions, and at no others: `(index, ours, theirs)`.
    Differs(&'static [(usize, &'static str, &'static str)]),
    /// toyos-cc refuses the file by name, and this is what the refusal says.
    Refused(&'static str),
}

/// Every case that does not match its `.expect`, asserted in both directions:
/// one that starts matching has to lose its entry, and one that stops matching
/// reds the run. Same contract as `NOT_RUN` in `tests/toyos.rs`, and for the
/// same reason — a declared failure nothing attempts is not a declaration.
const DECLARED: &[(&str, Outcome, &str)] = &[
    (
        "02",
        Outcome::Differs(&[
            (1, "f(2 * (2+(3,4)-0,1)) | f(2 * (~ 5)) & f(2 * (0,1))^m(0,1);",
                "f(2 * (2 +(3,4)-0,1)) | f(2 * (~ 5)) & f(2 * (0,1))^m(0,1);"),
            (4, "f(2 * (2+(3,4)-0,1)) | f(2 * (~ 5)) & f(2 * (0,1))^m(0,1);",
                "f(2 * (2 +(3,4)-0,1)) | f(2 * (~ 5)) & f(2 * (0,1))^m(0,1);"),
        ]),
        "toyos-cc drops a space a reference preprocessor keeps. The host `cc` matches the \
         `.expect` here, so this is not a tcc idiosyncrasy — our whitespace preservation \
         differs from every reference. Inert for a preprocessor whose output feeds our own \
         lexer, and this corpus is the only thing that ever reads that text",
    ),
    (
        "05",
        Outcome::Differs(&[(1, "10, 11, 12, };", " 10, 11, 12, };")]),
        "a lost leading space on a continued line; the same whitespace question as 02",
    ),
    (
        "12",
        Outcome::Refused("named variadic parameter"),
        "GNU's `#define f(x...)`, refused by name. One occurrence in everything this project \
         compiles — this case — and zero consumers, so the feature is out; the argument it \
         used to drop in silence is what made the silence in",
    ),
    (
        "24",
        Outcome::Differs(&[(2, ", -1);", " , -1);")]),
        "a lost leading space again",
    ),
];

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("tests/testcases/pp_tcc")
}

/// Preprocess `source` from inside the corpus directory, with the output and
/// the diagnostics on one stream in the order the compiler wrote them.
fn preprocess(dir: &Path, source: &str) -> String {
    let merged = std::env::temp_dir()
        .join(format!("toyos-cc-pp-{}-{source}.out", std::process::id()));
    let sink = std::fs::File::create(&merged).unwrap();
    let dup = sink.try_clone().unwrap();
    Command::new(env!("CARGO_BIN_EXE_toyos-cc"))
        .current_dir(dir)
        .args(["-E", "-P", source])
        .stdout(Stdio::from(sink))
        .stderr(Stdio::from(dup))
        .status()
        .unwrap();
    let text = std::fs::read_to_string(&merged).unwrap();
    let _ = std::fs::remove_file(&merged);
    text
}

/// Lines that count, with the whitespace question settled:
///
/// - a line that is empty or all whitespace is dropped, from both sides;
/// - trailing whitespace is dropped, because nothing downstream of a
///   preprocessor can see it — `18` has one space there and its `.expect` has
///   none;
/// - every remaining run of whitespace becomes one space, so a run matches any
///   other run;
/// - a run against nothing is **not** a match, at the start of a line as much
///   as inside it. That is the whole of `02`, `05` and `24`.
fn normalise(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut out = String::new();
            let mut in_space = false;
            for c in line.trim_end().chars() {
                if c.is_whitespace() {
                    in_space = true;
                } else {
                    if in_space {
                        out.push(' ');
                        in_space = false;
                    }
                    out.push(c);
                }
            }
            out
        })
        .collect()
}

fn differences(ours: &[String], theirs: &[String]) -> Vec<(usize, String, String)> {
    let mut out = Vec::new();
    for i in 0..ours.len().max(theirs.len()) {
        let a = ours.get(i).map(String::as_str).unwrap_or("<past the end>");
        let b = theirs.get(i).map(String::as_str).unwrap_or("<past the end>");
        if a != b {
            out.push((i, a.to_string(), b.to_string()));
        }
    }
    out
}

#[test]
fn the_preprocessor_corpus_says_what_it_does() {
    let dir = corpus_dir();
    let mut cases: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.ok()?.file_name().to_str()?.to_string();
            let stem = name.strip_suffix(".c").or_else(|| name.strip_suffix(".S"))?.to_string();
            dir.join(format!("{stem}.expect")).is_file().then_some(name)
        })
        .collect();
    cases.sort();
    assert_eq!(cases.len(), 25, "the corpus changed size: {cases:?}");

    let mut wrong: Vec<String> = Vec::new();
    for source in &cases {
        let stem = source.rsplit_once('.').unwrap().0;
        let expected = std::fs::read_to_string(dir.join(format!("{stem}.expect"))).unwrap();
        let got = normalise(&preprocess(&dir, source));
        let want = normalise(&expected);
        let diff = differences(&got, &want);
        let declared = DECLARED.iter().find(|(case, _, _)| *case == stem);

        match (declared, diff.is_empty()) {
            (None, true) => {}
            (None, false) => wrong.push(format!(
                "{stem}: stopped matching its .expect, and nothing declares that:\n      {}",
                diff.iter()
                    .map(|(i, a, b)| format!("line {i}: {a:?} against {b:?}"))
                    .collect::<Vec<_>>()
                    .join("\n      ")
            )),
            (Some((_, _, why)), true) => wrong.push(format!(
                "{stem}: matches its .expect now — delete its entry. It was declared because \
                 {why}"
            )),
            (Some((_, Outcome::Refused(says), _)), false) => {
                let said = got.join("\n");
                if !said.contains(says) {
                    wrong.push(format!(
                        "{stem}: was declared to be refused with {says:?} and said:\n      {said}"
                    ));
                }
            }
            (Some((_, Outcome::Differs(at), _)), false) => {
                let want: Vec<(usize, String, String)> =
                    at.iter().map(|(i, a, b)| (*i, a.to_string(), b.to_string())).collect();
                if diff != want {
                    wrong.push(format!(
                        "{stem}: differs from its .expect in a way nothing declared.\n      \
                         declared: {want:?}\n      found:    {diff:?}"
                    ));
                }
            }
        }
    }

    for (case, _, _) in DECLARED {
        if !cases.iter().any(|s| s.rsplit_once('.').unwrap().0 == *case) {
            wrong.push(format!("{case}: declared, and there is no such case in the corpus"));
        }
    }

    assert!(
        wrong.is_empty(),
        "the preprocessor corpus no longer does what this file says:\n  {}",
        wrong.join("\n  "),
    );
}

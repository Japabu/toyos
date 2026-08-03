//! `cp`, `mv` and `hexdump` as a user reaches them: spawned from `/bin`, judged
//! on their exit code and on their bytes.
//!
//! The three claims that are not "it works":
//!
//! - **`cp` streams.** The source is larger than `cp`'s own flush interval
//!   twice over and is not a page multiple, so the periodic-flush path and the
//!   partial tail both run. Nothing in a process can prove the *absence* of a
//!   file-sized allocation, but this is the workload that reaches the constants
//!   that rule one out.
//! - **`cp` never leaves a short file under the destination's name.** Every
//!   refusal below is checked for the destination it must not have touched and
//!   for the `.part` sibling it must not have left.
//! - **`mv` does not copy behind the caller's back.** A move between mounts is
//!   refused and both ends survive. If a copy-and-delete fallback is ever
//!   added, this goes red — which is the point: `sys_rename` reports one error
//!   for every cause, so such a fallback would fire on a broken rename too and
//!   hide it.
//!
//! `hexdump`'s expected output is not computed here. It is the byte-for-byte
//! output of the host's own `xxd` over the same 25 bytes, pasted in, so the
//! format is judged by something that is not this project.
//!
//! Every directory this test makes gets a file put in it first. An empty
//! directory does not stat as one — `sys_readdir` answers 0 for "empty" and for
//! "no such path" alike — so a `cp x emptydir/` would silently write a *file*
//! named `emptydir`. That is a kernel defect, recorded in known issues; it is
//! designed around here rather than asserted, because it is not `cp`'s.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

/// Larger than `cp`'s `FLUSH_BYTES` twice over, and not a page multiple.
const BIG: usize = 2 * 1024 * 1024 + 137;
/// `/tmp` and not `/home`, which is where this started. A `rename` on the
/// bcachefs mount reports success and destroys the file — the entry it inserts
/// under the new name is the one its own next step deletes, blocks and all —
/// so `cp`, which renames its working file into place, cannot land anything
/// there. That is a filesystem defect and it is in known issues; asserting it
/// here would be encoding it as behaviour. `/log` is the mount this test's
/// host-verified half uses, and `/tmp` is the one that works in-process.
const BIG_SRC: &str = "/tmp/toybox_cp_src.bin";
const BIG_DST: &str = "/tmp/toybox_cp_dst.bin";

/// The 25 bytes the host's `xxd` was run against.
const FIXTURE: &[u8] = b"Hello, world! ABCDEFGH\x00\x01\xff";
const FIXTURE_PATH: &str = "/tmp/toybox_fixture.bin";

/// `xxd fixture`, verbatim.
const XXD_ALL: &str = "\
00000000: 4865 6c6c 6f2c 2077 6f72 6c64 2120 4142  Hello, world! AB
00000010: 4344 4546 4748 0001 ff                   CDEFGH...
";

/// `xxd -s 4 -l 8 fixture`, verbatim.
const XXD_WINDOW: &str = "\
00000004: 6f2c 2077 6f72 6c64                      o, world
";

fn big() -> Vec<u8> {
    (0..BIG).map(|i| (i.wrapping_mul(31).wrapping_add(i >> 9) ^ 0xA5) as u8).collect()
}

/// Spawn `/bin/<cmd>` with one of its two output streams on a pipe.
///
/// `Command::output()` is not used and cannot be: `sys::process::toyos::output`
/// returns `Vec::new()` for stderr unconditionally, so every refusal below
/// would read as an empty message. `wait_with_output` is the cross-platform
/// path and does read the pipe. One stream at a time, which keeps it off the
/// two-pipe `read2` path — the stream that is not piped is inherited, so if a
/// command says something unexpected it lands on the console rather than
/// nowhere.
fn spawn(cmd: &str, args: &[&str], errors: bool) -> std::process::Output {
    let mut command = Command::new(format!("/bin/{cmd}"));
    command.args(args);
    if errors {
        command.stderr(Stdio::piped());
    } else {
        command.stdout(Stdio::piped());
    }
    command
        .spawn()
        .unwrap_or_else(|e| panic!("spawn /bin/{cmd}: {e}"))
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait for /bin/{cmd}: {e}"))
}

fn must_pass(cmd: &str, args: &[&str]) -> String {
    let out = spawn(cmd, args, false);
    assert!(out.status.success(), "{cmd} {args:?} exited {:?}", out.status.code());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A refusal is an exit code *and* a line naming what was refused. A command
/// that dies silently with a non-zero status satisfies the first half of that
/// and tells the caller nothing.
fn must_refuse(cmd: &str, args: &[&str], needle: &str) -> String {
    let out = spawn(cmd, args, true);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success(), "{cmd} {args:?} succeeded, expected a refusal");
    assert!(
        stderr.contains(cmd) && stderr.contains(needle),
        "{cmd} {args:?} refused without naming {needle:?}: {stderr:?}"
    );
    stderr
}

/// A directory with something in it, which is the only kind this machine can
/// tell from a missing one.
fn make_dir(path: &str) {
    fs::create_dir(path).ok();
    fs::write(format!("{path}/occupied"), b"so that this directory stats as one\n")
        .unwrap_or_else(|e| panic!("occupy {path}: {e}"));
}

/// Every sibling of `path` whose name marks it as a copy in progress.
fn leftovers(path: &str) -> Vec<String> {
    let dir = Path::new(path).parent().expect("a parent directory");
    fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".part"))
        .collect()
}

fn main() {
    fs::write(FIXTURE_PATH, FIXTURE).expect("stage the fixture");
    cp_round_trip();
    cp_refusals();
    mv_within_a_mount();
    mv_across_mounts();
    hexdump_format();
    hexdump_refusals();
    fs::remove_file(FIXTURE_PATH).expect("cleanup the fixture");
    println!("toybox file tools ok");
}

fn cp_round_trip() {
    let data = big();
    fs::write(BIG_SRC, &data).expect("stage the source");

    must_pass("cp", &[BIG_SRC, BIG_DST]);
    let back = fs::read(BIG_DST).expect("read the copy back");
    assert_eq!(back.len(), data.len(), "the copy is {} bytes, the source is {BIG}", back.len());
    let bad = back.iter().zip(&data).position(|(a, b)| a != b);
    assert!(bad.is_none(), "the copy differs at byte {}", bad.unwrap_or(0));
    assert!(leftovers(BIG_DST).is_empty(), "a successful copy left {:?}", leftovers(BIG_DST));
    println!("  PASS cp streamed {BIG} bytes byte-for-byte and left no partial");

    // A destination that is a directory takes the source's own name, which is
    // the only reason `cp x somedir/` means anything.
    make_dir("/tmp/toybox_cp_dir");
    must_pass("cp", &[FIXTURE_PATH, "/tmp/toybox_cp_dir"]);
    let landed = "/tmp/toybox_cp_dir/toybox_fixture.bin";
    assert_eq!(fs::read(landed).expect("cp into a directory"), FIXTURE);
    println!("  PASS cp into a directory keeps the source's name");

    fs::remove_file(BIG_SRC).expect("cleanup");
    fs::remove_file(BIG_DST).expect("cleanup");
    fs::remove_file(landed).expect("cleanup");
    fs::remove_file("/tmp/toybox_cp_dir/occupied").expect("cleanup");
}

fn cp_refusals() {
    fs::write("/tmp/toybox_keepme.bin", b"the destination, untouched\n").expect("stage a victim");

    // A missing source must not open, truncate or otherwise disturb the
    // destination — which is the whole reason the bytes go to a sibling first.
    must_refuse("cp", &["/tmp/toybox_absent.bin", "/tmp/toybox_keepme.bin"], "toybox_absent.bin");
    let kept = fs::read_to_string("/tmp/toybox_keepme.bin").expect("the destination survived");
    assert_eq!(kept, "the destination, untouched\n", "a refused cp changed the destination");
    assert!(
        leftovers("/tmp/toybox_keepme.bin").is_empty(),
        "a refused cp left {:?}",
        leftovers("/tmp/toybox_keepme.bin")
    );
    println!("  PASS cp refuses a missing source, and the destination is unchanged");

    // `/bin` rather than a directory made here: it is the one that is populated
    // without this test having to populate it.
    must_refuse("cp", &["/bin", "/tmp/toybox_fromdir.bin"], "is a directory");
    assert!(fs::read("/tmp/toybox_fromdir.bin").is_err(), "cp of a directory created a file");
    println!("  PASS cp refuses a directory by name");

    must_refuse("cp", &["/tmp/toybox_keepme.bin"], "Usage");
    println!("  PASS cp refuses a one-argument invocation");

    fs::remove_file("/tmp/toybox_keepme.bin").expect("cleanup");
}

fn mv_within_a_mount() {
    let body = b"moved, not copied\n";
    fs::write("/tmp/toybox_mv_a.bin", body).expect("stage");
    must_pass("mv", &["/tmp/toybox_mv_a.bin", "/tmp/toybox_mv_b.bin"]);
    assert!(fs::read("/tmp/toybox_mv_a.bin").is_err(), "mv left the source behind");
    assert_eq!(&fs::read("/tmp/toybox_mv_b.bin").expect("the moved file")[..], body);
    println!("  PASS mv within a mount renames, and the old name is gone");

    // The same directory rule as cp, and literally the same code.
    make_dir("/tmp/toybox_mv_dir");
    must_pass("mv", &["/tmp/toybox_mv_b.bin", "/tmp/toybox_mv_dir"]);
    assert_eq!(
        &fs::read("/tmp/toybox_mv_dir/toybox_mv_b.bin").expect("moved into the directory")[..],
        body
    );
    println!("  PASS mv into a directory keeps the source's name");

    must_refuse("mv", &["/tmp/toybox_absent.bin", "/tmp/toybox_x.bin"], "toybox_absent.bin");
    assert!(fs::read("/tmp/toybox_x.bin").is_err(), "a refused mv created the destination");
    println!("  PASS mv refuses a missing source before it renames anything");

    fs::remove_file("/tmp/toybox_mv_dir/toybox_mv_b.bin").expect("cleanup");
    fs::remove_file("/tmp/toybox_mv_dir/occupied").expect("cleanup");
}

fn mv_across_mounts() {
    let body = b"this file stays on /tmp\n";
    fs::write("/tmp/toybox_mv_cross.bin", body).expect("stage");

    let stderr = must_refuse(
        "mv",
        &["/tmp/toybox_mv_cross.bin", "/home/toybox_mv_cross.bin"],
        "different mounts",
    );
    assert!(stderr.contains("cp then rm"), "the refusal does not say what to do: {stderr:?}");
    assert_eq!(
        &fs::read("/tmp/toybox_mv_cross.bin").expect("the source survived")[..],
        body,
        "a refused mv damaged the source"
    );
    assert!(
        fs::read("/home/toybox_mv_cross.bin").is_err(),
        "a refused mv put the file at the destination anyway — mv is copying behind the \
         caller's back, which is exactly what it must not do while a rename failure has \
         only one error code"
    );
    println!("  PASS mv refuses a move between mounts and leaves both ends alone");

    fs::remove_file("/tmp/toybox_mv_cross.bin").expect("cleanup");
}

fn hexdump_format() {
    let got = must_pass("hexdump", &[FIXTURE_PATH]);
    assert_eq!(got, XXD_ALL, "hexdump does not agree with xxd\ngot:\n{got}want:\n{XXD_ALL}");

    let got = must_pass("hexdump", &["-s", "4", "-l", "8", FIXTURE_PATH]);
    assert_eq!(got, XXD_WINDOW, "hexdump -s -l does not agree with xxd\ngot:\n{got}");

    // The same window asked for in hex, and a length past the end folded into
    // the file rather than read off it.
    let got = must_pass("hexdump", &["-s", "0x4", "-l", "0x8", FIXTURE_PATH]);
    assert_eq!(got, XXD_WINDOW, "0x offsets differ from decimal");
    let got = must_pass("hexdump", &["-l", "4096", FIXTURE_PATH]);
    assert_eq!(got, XXD_ALL, "-l past the end read past the end");
    println!("  PASS hexdump matches xxd byte-for-byte, whole file and window");
}

fn hexdump_refusals() {
    let past = (FIXTURE.len() + 1).to_string();
    must_refuse("hexdump", &["-s", &past, FIXTURE_PATH], "past the end");
    must_refuse("hexdump", &["-s", "twelve", FIXTURE_PATH], "not a number");
    must_refuse("hexdump", &["-s", FIXTURE_PATH], "not a number");
    must_refuse("hexdump", &["-l"], "needs a number");
    must_refuse("hexdump", &["-q", FIXTURE_PATH], "unknown option");
    must_refuse("hexdump", &[FIXTURE_PATH, FIXTURE_PATH], "one file at a time");
    must_refuse("hexdump", &["/tmp/toybox_absent.bin"], "toybox_absent.bin");
    println!("  PASS hexdump refuses seven bad requests, each by name");
}

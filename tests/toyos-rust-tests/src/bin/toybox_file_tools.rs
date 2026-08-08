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
//! - **A rename that reports success moved the file.** Both `cp` and `mv` run
//!   on `/home` here, where a rename used to insert the entry under the new
//!   name and then delete that same entry, freeing the file's blocks on the
//!   way out. Every claim below is about bytes on the other side of the move,
//!   because that failure returned `Ok(())` and left both names absent.
//!
//! `hexdump`'s expected output is not computed here. It is the byte-for-byte
//! output of the host's own `xxd` over the same 25 bytes, pasted in, so the
//! format is judged by something that is not this project.
//!
//! Every directory this test makes is left empty, which is the harder case and
//! until recently the broken one: `cp x emptydir/` wrote a *file* named
//! `emptydir`, because both halves of `is_dir` read "no entries" as "no such
//! path". Both halves are fixed and this is where the pair is judged together —
//! `/bin/cp` asks `fs::metadata`, which asks `readdir`, which asks the kernel.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

/// Larger than `cp`'s `FLUSH_BYTES` twice over, and not a page multiple.
const BIG: usize = 2 * 1024 * 1024 + 137;
/// `/home`, the bcachefs mount, because `cp` lands its bytes on a sibling and
/// renames that onto the destination — and a rename here used to insert the
/// entry under the new name and then delete exactly that entry, blocks and
/// all, reporting success. A copy that arrives byte-for-byte is that path as a
/// user reaches it. `/log` is the mount this test's host-verified half uses.
const BIG_SRC: &str = "/home/toybox_cp_src.bin";
const BIG_DST: &str = "/home/toybox_cp_dst.bin";

/// The three names the rename round trip walks a file through, on the same
/// mount so that every step is one `SYS_RENAME`.
const MV_A: &str = "/home/toybox_mv_home_a.bin";
const MV_B: &str = "/home/toybox_mv_home_b.bin";
const MV_ONTO: &str = "/home/toybox_mv_home_onto.bin";

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
/// One stream at a time rather than `Command::output()`: the stream that is
/// not piped is inherited, so a command that says something unexpected says it
/// on the console instead of into a buffer no assertion reads.
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

/// An empty directory, which is the case `cp x d/` used to get wrong.
fn make_dir(path: &str) {
    fs::create_dir(path).unwrap_or_else(|e| panic!("mkdir {path}: {e}"));
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
    mv_on_home();
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

/// Whether `/home`'s listing holds a name. Other tests write there in the same
/// boot, so this asks about one name and not about the whole directory.
fn home_holds(name: &str) -> bool {
    fs::read_dir("/home")
        .expect("read_dir /home")
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy() == name)
}

/// The rename `/tmp` exercises is not the one that was broken. `/home` keys an
/// entry by the hash of its name and holds the file's extent list inline in
/// the value, so a rename re-encodes that value under a new key and has to
/// remove the old entry *without* freeing what it points at. Byte-exactness
/// after the move is what carries that claim: a name that resolves proves only
/// that some entry exists under it.
fn mv_on_home() {
    let body: Vec<u8> =
        (0..3 * 4096 + 29).map(|i: usize| (i.wrapping_mul(37) ^ 0x3C) as u8).collect();
    fs::write(MV_A, &body).expect("stage a file on /home");

    must_pass("mv", &[MV_A, MV_B]);
    assert!(fs::read(MV_A).is_err(), "mv left the source behind on /home");
    assert!(!home_holds("toybox_mv_home_a.bin"), "the old name is still in /home's listing");
    let moved = fs::read(MV_B).expect("read the moved file");
    assert_eq!(moved.len(), body.len(), "the moved file is {} bytes", moved.len());
    let bad = moved.iter().zip(&body).position(|(a, b)| a != b);
    assert!(bad.is_none(), "the moved file differs from the source at byte {}", bad.unwrap_or(0));
    println!("  PASS mv on /home moves {} bytes and the old name is gone", body.len());

    // Onto a name that already exists. The entry the rename overwrites is the
    // one whose blocks are freed; freeing the other one is the same mistake
    // wearing the destination's name.
    fs::write(MV_ONTO, b"the file that gets replaced\n").expect("stage the destination");
    must_pass("mv", &[MV_B, MV_ONTO]);
    assert!(fs::read(MV_B).is_err(), "mv onto an existing name left the source behind");
    let over = fs::read(MV_ONTO).expect("read the overwritten destination");
    let bad = over.iter().zip(&body).position(|(a, b)| a != b);
    assert!(
        over.len() == body.len() && bad.is_none(),
        "the destination holds {} bytes differing at {:?}, not the {} that were moved onto it",
        over.len(),
        bad,
        body.len(),
    );
    println!("  PASS mv onto an existing name on /home replaces it byte-for-byte");

    fs::remove_file(MV_ONTO).expect("cleanup");
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

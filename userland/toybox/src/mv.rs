use std::fs;
use std::path::Path;
use std::process;

/// Move a file, which here is exactly one `SYS_RENAME` and nothing else.
///
/// There is no copy-and-delete fallback for a move between mounts, and the
/// reason is not that the pieces are missing — `cp` is right there. It is that
/// `sys_rename` in `kernel/src/arch/syscall.rs` collapses all five of
/// `Vfs::rename`'s errors into `NotFound`, and `Stat` carries no mount
/// identity, so this process cannot tell "different mounts" from "the rename
/// is broken". A fallback keyed on *any* rename failure would quietly copy its
/// way past a rename defect and pass every test that a working rename passes.
/// So the refusal names both paths, says what it could not distinguish, and
/// stops.
pub fn main(args: Vec<String>) {
    let [source, dest] = args.as_slice() else {
        eprintln!("Usage: mv <source> <dest>");
        process::exit(1);
    };

    let source = Path::new(source);
    let dest = crate::cp::destination(source, Path::new(dest));

    // Asked before the rename so the commonest cause gets its own answer
    // rather than the one the kernel returns for everything.
    if let Err(e) = fs::metadata(source) {
        eprintln!("mv: {}: {e}", source.display());
        process::exit(1);
    }

    if let Err(e) = fs::rename(source, &dest) {
        eprintln!(
            "mv: {} -> {}: {e} — but the source is there, so it is the rename being refused",
            source.display(),
            dest.display()
        );
        eprintln!(
            "mv: most likely the two paths are on different mounts, which is not one rename. \
             SYS_RENAME reports one error for every cause, so mv cannot tell that from a rename \
             defect and will not copy behind your back — use cp then rm."
        );
        process::exit(1);
    }
}

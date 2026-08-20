use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process;

/// The one read/write buffer. Sized here and never from the file, so a copy
/// allocates the same 64 KiB whatever the source's length.
const BUF_BYTES: usize = 64 * 1024;

/// Bytes written between fsyncs.
///
/// Not a durability knob. `evict_if_needed` in `kernel/src/file_cache.rs`
/// declines to take a dirty page, so the kernel's dirty set is bounded only by
/// what the writer leaves un-flushed — without this the kernel holds the whole
/// copy however small the buffer above is, and a bounded buffer in front of an
/// unbounded one is not a bound. 256 pages, against a file cache whose smallest
/// possible budget is the 2048-page floor in `block::file_cache_pages`. Policy,
/// not physics; reaching it costs a flush, never a refusal.
const FLUSH_BYTES: u64 = 1024 * 1024;

pub fn main(args: Vec<String>) {
    let [source, dest] = args.as_slice() else {
        eprintln!("Usage: cp <source> <dest>");
        process::exit(1);
    };

    let source = Path::new(source);
    let dest = destination(source, Path::new(dest));
    if let Err(refusal) = copy(source, &dest) {
        eprintln!("cp: {refusal}");
        process::exit(1);
    }
}

/// Where a two-argument copy or move actually lands.
///
/// `mv` shares this because the rule is the same one, not a similar one: a
/// destination that is a directory takes the source's own name.
pub fn destination(source: &Path, dest: &Path) -> PathBuf {
    match (is_dir(dest), source.file_name()) {
        (true, Some(name)) => dest.join(name),
        _ => dest.to_path_buf(),
    }
}

fn is_dir(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_dir())
}

/// Copy `source` onto `dest`, or leave `dest` exactly as it was.
///
/// The bytes go to a sibling of the destination and are renamed onto it only
/// once the last one is on the device. The destination is therefore never open
/// during the copy, and the two ways this can stop short both leave it alone:
/// a refused read or write deletes the partial and says so, and a killed `cp`
/// leaves a file named `<dest>.<pid>.part`, which is evidence. Writing
/// straight to the destination would instead leave a short file wearing the
/// name of a complete one, which nothing downstream can tell from the real
/// thing.
fn copy(source: &Path, dest: &Path) -> Result<(), String> {
    let meta = fs::metadata(source).map_err(|e| format!("{}: {e}", source.display()))?;
    if meta.is_dir() {
        return Err(format!("{}: is a directory", source.display()));
    }
    let mut reader = File::open(source).map_err(|e| format!("{}: {e}", source.display()))?;

    let Some(name) = dest.file_name().and_then(|n| n.to_str()) else {
        return Err(format!("{}: no file name to copy onto", dest.display()));
    };
    let partial = dest.with_file_name(format!("{name}.{}.part", process::id()));

    let outcome = stream(&mut reader, source, &partial).and_then(|_| {
        fs::rename(&partial, dest)
            .map_err(|e| format!("{} -> {}: {e}", partial.display(), dest.display()))
    });

    if outcome.is_err() {
        if let Err(e) = fs::remove_file(&partial) {
            eprintln!("cp: {}: {e} — a partial copy is still there", partial.display());
        }
    }
    outcome
}

fn stream(reader: &mut File, source: &Path, partial: &Path) -> Result<(), String> {
    let mut writer = File::create(partial).map_err(|e| format!("{}: {e}", partial.display()))?;
    let mut buf = vec![0u8; BUF_BYTES];
    let mut unflushed = 0u64;

    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(format!("reading {}: {e}", source.display())),
        };
        writer
            .write_all(&buf[..n])
            .map_err(|e| format!("writing {}: {e}", partial.display()))?;
        unflushed += n as u64;
        if unflushed >= FLUSH_BYTES {
            sync(&writer, partial)?;
            unflushed = 0;
        }
    }

    sync(&writer, partial)
}

/// The kernel's close path discards the flush error
/// (`issues/filesystem/close-cannot-report-io-error.md`), so a copy that never
/// asks is a copy that cannot be told the volume was full.
fn sync(writer: &File, partial: &Path) -> Result<(), String> {
    writer.sync_all().map_err(|e| format!("flushing {}: {e}", partial.display()))
}

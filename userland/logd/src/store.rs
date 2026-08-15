//! `/log` as this program's policy: one file per boot, continuations, and
//! retention.
//!
//! Moved out of `kernel/src/log_file.rs` unchanged in behaviour
//! (`specs/log-architecture-spec.md` §5.5) — the constants keep their values
//! and their reasons, the names keep their shape, and [`classify`] keeps being
//! strict. What changed is who runs it: a userland process with an ordinary
//! `File`, so every byte goes through `SYS_WRITE` and `SYS_FSYNC` exactly as
//! any other program's would.
//!
//! **[`classify`] is more necessary here than it was in the kernel, not less.**
//! `/log` is userland-writable and `toybox` writes there; this program is
//! userland too, and the only thing standing between somebody else's file and
//! `delete_file` is that this function does not recognise it.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use toyos_wallclock::is_stem;

/// Where the logs go.
///
/// The root of the log partition, so that plugging the stick into another
/// machine puts them at the top of the window that opens.
pub const DIR: &str = "/log";

/// The name a boot gets when the machine would not say what time it is — or
/// would not say it unambiguously (`wall`).
///
/// A word and not a zero date: `0000-00-00-000000.log` sorts correctly and
/// reads as a real timestamp that happens to be absurd, and the difference
/// matters to whoever finds it on a stick six months from now.
pub const UNDATED_STEM: &str = "unknown";

/// How many of this program's files the volume keeps, including the one this
/// boot is writing.
///
/// Sixteen boots of history, which is the number that makes "it broke after the
/// firmware update" answerable by looking. `create_log_volume` makes the
/// smallest volume there is a FAT32 for and `fsck_msdos` reports 35,098,112
/// free bytes on a fresh one, so sixteen files at [`MAX_LOG_BYTES`] is 16 MiB —
/// under half, with the rest left for anything a later diagnostic wants to drop
/// beside them.
pub const MAX_LOG_FILES: usize = 16;

/// How many continuation files one boot may produce before this gives up.
///
/// The part number is four digits wide and a fifth would sort *before* the
/// fourth, putting retention in the wrong order. At the shipped bound that is
/// 10 GiB from one boot, so nothing but a log loop reaches it.
pub const MAX_LOG_PARTS: u32 = 9999;

/// How large one file may get before the next part starts.
///
/// One mebibyte: a boot that logs a hundred times more than any real one still
/// fits, and sixteen of them fit the volume with room to spare. It also bounds
/// what `/bin/console` reads off USB before it paints anything.
pub const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// The rotate-fast bound, and it is an argument now rather than a kernel
/// actuator (§5.5).
///
/// It exists for the same reason `test-small-caches` does: filling megabytes by
/// logging would take a boot far longer than a test should wait, and the code
/// it drives is the shipped code — only the bound moves. 256 bytes, so one
/// boot's own log crosses it many times over and drives both the continuation
/// and the retention path.
pub const ROTATE_FAST_BYTES: u64 = 256;

/// Where a file sits in the order retention deletes in: lower goes first.
///
/// Undated boots go before dated ones because they cannot be ordered against
/// them — there is no clock to compare — and of the two kinds, the one that can
/// be placed in time is the one worth keeping. Within a kind the name is the
/// order, which is what the timestamp format is for.
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Class {
    Undated,
    Dated,
}

/// Whether `name` is one of this program's files, and which kind.
///
/// Strict on purpose, and the strictness is the safety property: anything this
/// does not recognise exactly is somebody else's file and is never deleted to
/// make room.
pub fn classify(name: &str) -> Option<Class> {
    let stem = name.strip_suffix(".log")?;
    let stem = match stem.split_once('_') {
        Some((head, part)) => {
            if part.len() != 4 || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            head
        }
        None => stem,
    };

    if let Some(index) = stem.strip_prefix(UNDATED_STEM).and_then(|s| s.strip_prefix('-')) {
        let ours = index.len() == 2 && index.bytes().all(|b| b.is_ascii_digit());
        return ours.then_some(Class::Undated);
    }
    is_stem(stem).then_some(Class::Dated)
}

/// The name of one file in this boot's sequence.
///
/// The first part carries the bare stem, because that is what nearly every boot
/// ever writes and a `_0001` on it would be noise on every stick. A
/// continuation takes `_` rather than any other separator for one reason: it is
/// the only legal character that sorts *after* `.`, so `<stem>.log` still comes
/// before `<stem>_0002.log` and retention deletes a boot's parts in the order
/// they were written.
pub fn path(stem: &str, part: u32) -> String {
    match part {
        1 => format!("{DIR}/{stem}.log"),
        n => format!("{DIR}/{stem}_{n:04}.log"),
    }
}

/// Every file on `/log` that this program wrote, oldest first.
pub fn ours() -> Vec<String> {
    let Ok(entries) = fs::read_dir(DIR) else { return Vec::new() };
    let mut ours: Vec<(Class, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            Some((classify(&name)?, name))
        })
        .collect();
    ours.sort();
    ours.into_iter().map(|(_, name)| format!("{DIR}/{name}")).collect()
}

/// Delete this program's oldest files until at most `keep` remain, and return
/// what is left. Every deletion is named on the console.
pub fn sweep(existing: Vec<String>, keep: usize, mut say: impl FnMut(String)) -> Vec<String> {
    let over = existing.len().saturating_sub(keep);
    let mut kept = Vec::with_capacity(existing.len() - over);
    for (i, path) in existing.into_iter().enumerate() {
        if i >= over {
            kept.push(path);
            continue;
        }
        // Named, because a file disappearing off the owner's stick with nothing
        // saying why is indistinguishable from a bug in this program.
        match fs::remove_file(&path) {
            Ok(()) => say(format!(
                "logd: {DIR} holds more than {MAX_LOG_FILES} logs, so {path} was deleted"
            )),
            Err(e) => {
                say(format!(
                    "logd: {path} is past the {MAX_LOG_FILES}-log bound and would not delete: {e}"
                ));
                kept.push(path);
            }
        }
    }
    kept
}

/// The lowest index no undated log on the volume is using.
pub fn undated_stem(kept: &[String]) -> Option<String> {
    (0..MAX_LOG_FILES)
        .map(|i| format!("{UNDATED_STEM}-{i:02}"))
        .find(|stem| !kept.iter().any(|path| path.starts_with(&format!("{DIR}/{stem}"))))
}

/// This boot's file, and everything about where it is in its own sequence.
pub struct Volume {
    file: File,
    stem: String,
    part: u32,
    /// Bytes in the current part so far. Kept here rather than read back from
    /// the filesystem so a disagreement shows up as a wrong length rather than
    /// being silently corrected.
    size: u64,
    rotate_at: u64,
}

impl Volume {
    /// Open this boot's first file, after making room for it.
    ///
    /// `stem` is a wall-clock stamp, or `None` for a boot that could not be
    /// placed in time — which takes the lowest free `unknown-NN`.
    pub fn open(stem: Option<String>, rotate_at: u64, mut say: impl FnMut(String)) -> Option<Self> {
        // One below the bound, because this boot's own file is about to become
        // the sixteenth.
        let kept = sweep(ours(), MAX_LOG_FILES - 1, &mut say);
        let stem = match stem {
            Some(stem) => stem,
            None => match undated_stem(&kept) {
                Some(stem) => stem,
                None => {
                    say(format!(
                        "logd: no free {UNDATED_STEM} name on {DIR}; this boot's log has nowhere \
                         to go"
                    ));
                    return None;
                }
            },
        };
        // The first part number this boot's name does not already have on the
        // volume. Two boots inside one second is a machine nobody has, but a
        // test that stages the wall clock has it every run, and a colliding
        // name would silently write over the older boot.
        let part = (1..=MAX_LOG_PARTS).find(|p| !kept.contains(&path(&stem, *p)))?;
        let full = path(&stem, part);
        let file = match create(&full) {
            Ok(file) => file,
            Err(e) => {
                say(format!("logd: cannot create {full}: {e}"));
                return None;
            }
        };
        Some(Volume { file, stem, part, size: 0, rotate_at })
    }

    /// Where this boot's log is being written.
    pub fn path(&self) -> String {
        path(&self.stem, self.part)
    }

    /// Append one already-rendered line. The caller batches; this does no
    /// buffering of its own beyond what the file cache is.
    pub fn write(&mut self, line: &[u8]) -> std::io::Result<()> {
        self.file.write_all(line)?;
        self.size += line.len() as u64;
        Ok(())
    }

    /// Get everything written so far onto the device, cache flush included —
    /// which is what `SYS_FSYNC` means on this tree since L6 (§12.4).
    pub fn sync(&mut self) -> std::io::Result<()> {
        self.file.sync_all()
    }

    /// Whether this part has reached its bound.
    pub fn full(&self) -> bool {
        self.size >= self.rotate_at
    }

    /// Carry on in the next file of this boot's sequence.
    ///
    /// Neither end of a long boot's log is dropped: the earlier parts stay until
    /// [`MAX_LOG_FILES`] reaches them, and by then they are the oldest files on
    /// the volume by the same rule that governs every other boot's.
    pub fn rotate(&mut self, mut say: impl FnMut(String)) -> std::io::Result<()> {
        let full = self.path();
        let bytes = self.size;
        if self.part >= MAX_LOG_PARTS {
            return Err(std::io::Error::other("this boot has no continuation left"));
        }
        sweep(ours(), MAX_LOG_FILES - 1, &mut say);
        self.part += 1;
        self.size = 0;
        let next = self.path();
        self.file = create(&next)?;
        say(format!("logd: {full} reached {bytes} bytes and this boot continues in {next}"));
        Ok(())
    }
}

fn create(path: &str) -> std::io::Result<File> {
    // Truncating rather than appending: the name is this boot's alone — the
    // part search above is what makes that true — so anything already under it
    // is a name collision and not a log to continue.
    OpenOptions::new().write(true).create(true).truncate(true).open(PathBuf::from(path))
}

//! The kernel's log, as a file on a partition made to be readable elsewhere.
//!
//! A ThinkPad T14 Gen 2 has no serial port. Once the compositor claims the
//! framebuffer the kernel's only remaining channel is the on-screen console,
//! which paints at the six boot checkpoints and on a fatal panic and at no
//! other time — so between `Boot: complete` and a panic the machine says
//! nothing at all, which is why a real boot failure there was undiagnosable.
//! This gives it a channel that survives the power being cut: the stick it
//! booted from.
//!
//! # One file per boot
//!
//! Named for the wall clock at the moment the sink installed, so the directory
//! sorts into the order the boots happened in and the owner can say which file
//! is the boot he is asking about. A boot whose clock never answered is named
//! [`UNDATED_STEM`] and an index instead, because a file that claims a time it
//! does not have is worse than one that says it has none.
//!
//! What this replaced was a single `kernel.log` appended across boots with one
//! `kernel.log.1` beside it. Two boots' output in one file with nothing marking
//! the seam is exactly the diagnostic the owner could not use, and the
//! generation number said only "older", never *when*.
//!
//! [`MAX_LOG_FILES`] is what keeps that from growing without end, and it
//! deletes oldest-first by the same order the names sort in.
//!
//! # Why not the ESP
//!
//! Because the log has to be *read*, and it is read on another machine. macOS
//! never auto-mounts an EFI-typed partition and this host refuses even a manual
//! non-root mount of one, so a log beside `kernel.elf` needed the admin account
//! to get at. The log partition is typed Microsoft Basic Data, which Finder,
//! Windows and Linux all mount on plug-in with nothing configured, and it is
//! labelled `TOYOS-LOG` so the icon says what it is. The kernel is *given* it by
//! unique GUID (`gpt`, `KernelArgs::log_partition_guid`) and never goes looking
//! for the other FAT32 on the stick.
//!
//! # Not a special path
//!
//! Every byte goes through [`vfs`] exactly as a userland `write` would:
//! `create_file`, `file_cache::write_page`, `flush_file`. There is no
//! filesystem call here that `fd.rs` does not make. A private path to the disk
//! would be a second implementation to keep correct, and the first thing it
//! would hide is a VFS bug.
//!
//! # When it writes
//!
//! Continuously, from the idle loop. The failure this exists for is a machine
//! that *stops* — a wedge, a livelock, a hang with no panic and no console —
//! and for that, "the tail is on disk" is the whole requirement. The
//! alternative the owner asked about, a flush at boot-complete plus a periodic
//! one, saves I/O and loses exactly the evidence the feature is for.
//!
//! It costs nothing when nothing is logged: [`has_pending`] is one relaxed load
//! and, at most, a walk of eight shard heads. And it is not on the logging hot
//! path at all — `log!` publishes a record into its own CPU's shard and nothing
//! else, so nothing that logs while holding a kernel lock does I/O.
//!
//! Batching needs no tuning and has no period. A busy machine reaches the idle
//! loop rarely, so each flush carries more; an idle one reaches it constantly
//! and each flush carries a line or two. The same argument the console's drain
//! makes, with a device write in place of a UART byte.
//!
//! # The panic path does not write here, and cannot be made to
//!
//! Stated rather than attempted, and the reason is locks alone. A panic-time
//! flush would need, in order: this module's lock, the VFS lock, the file cache
//! lock, the kernel heap (`toyos-fat32` keeps its sector scratch in a `Vec` and
//! builds a `String` per path component), the log volume's device lock, and the
//! xHCI controller lock. A panicking thread may hold any of them — the VFS lock
//! especially, since every reachable kernel panic from filesystem code holds it
//! — so a panic-time flush would deadlock in precisely the cases the log is for.
//!
//! What is *no longer* an argument, and was: that a torn FAT write costs the
//! machine. It did while the log shared the ESP with `BOOTx64.EFI` and
//! `kernel.elf`, where a write interrupted between allocating a cluster and
//! recording it leaves a volume that may not boot. On its own partition the
//! worst a half-finished write can cost is one boot's file, which is a
//! diagnostic and not the stick. The deadlock is what keeps the panic path out,
//! and it is enough on its own.
//!
//! The panic path keeps the on-screen console, which takes no lock of any kind.
//! What the file gives a panic is everything up to the last idle pass — on any
//! machine that reached the idle loop, everything but the panic itself, and the
//! panic is on the screen.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::clock::{self, Civil};
use crate::file_cache::{self, FileId};
use crate::log::read::{drain_ordered, Published, RecordSink};
use crate::sync::Lock;
use crate::vfs::{self, Vfs};
use toyos_abi::log::LogRecord;
use toyos_abi::syscall::SyscallError;

/// Where the logs go.
///
/// The root of the log partition, so that plugging the stick into another
/// machine puts them at the top of the window that opens. Nothing else is on
/// this volume and nothing else is meant to be — but userland may write here,
/// so [`classify`] is strict about which names are this module's to delete.
const DIR: &str = "/log";

/// The mount [`DIR`] is on, for the per-mount sync each flush ends with.
const MOUNT: &str = "log";

/// The name a boot gets when the machine would not say what time it is.
///
/// A word and not a zero date: `0000-00-00-000000.log` sorts correctly and
/// reads as a real timestamp that happens to be absurd, and the difference
/// matters to whoever finds it on a stick six months from now.
const UNDATED_STEM: &str = "unknown";

/// How many of this module's files the volume keeps, including the one this
/// boot is writing.
///
/// Sixteen boots of history, which is the number that makes "it broke after the
/// firmware update" answerable by looking. Against the volume: `create_log_volume`
/// makes the smallest volume there is a FAT32 for and `fsck_msdos` reports
/// 35,098,112 free bytes on a fresh one, so sixteen files at [`max_log_bytes`]
/// is 16 MiB — under half, with the rest left for anything a later diagnostic
/// wants to drop beside them. In practice a whole metal-sim boot's log measured
/// 7,910 bytes, so sixteen of them are about 128 KiB.
///
/// When it is hit, the oldest file by [`classify`]'s order is deleted and a line
/// naming it goes in the new file.
const MAX_LOG_FILES: usize = 16;

/// How many continuation files one boot may produce before the sink gives up.
///
/// A boot that fills [`max_log_bytes`] carries on in `<stem>_0002.log` rather
/// than dropping either end of its own log, and [`MAX_LOG_FILES`] deletes this
/// boot's earlier parts as it goes. The bound exists because the part number is
/// four digits wide and a fifth would sort before the fourth, putting retention
/// in the wrong order; what a caller sees when it is hit is the sink disabling
/// itself with a line saying so. At the shipped bound that is 10 GiB from one
/// boot, so nothing but a log loop reaches it.
const MAX_LOG_PARTS: u32 = 9999;

/// How large one file may get before the next part starts.
///
/// One mebibyte: a boot that logs a hundred times more than any real one has
/// still fits, and sixteen of them fit the volume with room to spare. It also
/// bounds what `/bin/console` reads off USB before it paints anything, which is
/// the other end of this number — it seeds its scrollback from the newest file
/// at every framebuffer boot.
///
/// The rotate-fast value exists for the same reason `test-small-caches` does:
/// filling megabytes by logging would take a boot far longer than a test should
/// wait, and the code it drives is the shipped code — only the bound moves. 256
/// bytes, so that one boot's own log crosses it many times over and drives both
/// the continuation and the retention path.
fn max_log_bytes() -> u64 {
    if crate::actuator::log_rotate_fast() {
        256
    } else {
        1024 * 1024
    }
}

/// Whether this sink is collecting at all.
///
/// False until a `/log` exists and [`install`] runs, and false again the moment
/// the sink gives up — which is what keeps [`has_pending`] from reporting
/// records owed to a consumer that no longer exists, and the idle loop from
/// declining to sleep on them forever. It was `log_ring::FILE_SINK` and it is
/// the same flag the deleted byte ring had, for the same reason; what it gates
/// is a cursor rather than a second set of byte indices into one buffer.
static COLLECTING: AtomicBool = AtomicBool::new(false);

/// Where this sink has got to in the record stream.
///
/// **Its own position, and that is the whole of what two consumers used to need
/// two cursors into one byte ring for.** The console's is
/// `log::console`'s; neither consumes the other's records, and a reader that
/// stops costs the other nothing. Published words rather than a field in
/// [`Sink`] because [`has_pending`] is read from the pre-`hlt` check with
/// interrupts off and may take no lock.
static POSITION: Published = Published::new();

/// Does the log volume still owe this boot records? Lock-free, for the pre-halt
/// check in `sched::driver::execute` and for `apic::owed`.
pub fn has_pending() -> bool {
    COLLECTING.load(Ordering::Relaxed) && POSITION.any_pending()
}

/// How long the VFS lock may stay held before the sink gives up on it.
///
/// It bounds the one case that never clears — a thread that panicked holding
/// the VFS lock, which `specs/issues/panic-path/panic-holding-process-table-hangs.md`
/// records as live. Without it the ring would
/// report bytes pending forever and the idle loop, which declines to sleep on
/// that, would spin a CPU with nothing to do about it.
///
/// A *duration* and not a count of polls, which is what it was. A count only
/// stands for a duration if you know how fast the idle loop goes round, and
/// nothing here does: 1000 polls elapse in about two milliseconds, while an
/// ordinary `spawn` holds the VFS lock for 13-17 ms reading an ELF. So a
/// healthy machine turned its own log off during a spawn — seen on both an
/// audio boot and a shutdown boot, permanently, and on the machine this
/// feature exists for there is no other channel to notice it on.
///
/// Ten seconds because the thing on the other side of it is a panic, and
/// nothing legitimate holds this lock for anywhere near that. Erring long
/// costs a spinning CPU on a machine that has already lost a thread; erring
/// short costs the log on a machine that is working.
const MAX_BLOCKED_NANOS: u64 = 10_000_000_000;

struct Sink {
    file_id: FileId,
    /// What every file this boot writes is named for: a timestamp, or
    /// [`UNDATED_STEM`] and an index.
    stem: String,
    /// Which continuation this is. One is the file the boot started in.
    part: u32,
    /// Bytes in the current part so far, which is what decides the next page
    /// index. Kept here rather than read back from the file cache so a
    /// disagreement shows up as a wrong offset rather than being silently
    /// corrected.
    size: u64,
    /// When the current run of polls that found the VFS lock held began.
    blocked_since: Option<u64>,
    /// Records this sink has already said were overwritten before it got to
    /// them. The cursor's own `lost` is cumulative, so the line reports the
    /// difference — one line per hole rather than one per flush.
    reported_lost: u64,
}

static SINK: Lock<Option<Sink>> = Lock::new(None);

/// Whether a flush is between taking records out of the shards and getting them
/// on the device.
///
/// [`has_pending`] answers "the shards still hold records this sink has not
/// taken", which goes false as the cursor advances — *before* `flush_file` and
/// `sync_mount` have run. A dying machine waiting on that predicate alone
/// therefore stops waiting in the middle of the write and halts the CPU doing
/// it, which is exactly what `halt_all_cpus`'s drain wait did until this
/// existed: the report left the ring and never reached the stick.
///
/// The pair has no gap. The cursor only advances inside this flag's window, so
/// "nothing pending and no flush running" cannot be observed while records are
/// still owed to the volume.
static IN_FLUSH: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Is a flush still between the ring and the device? For the fatal path, which
/// must not halt the CPU doing the writing.
pub fn flush_in_progress() -> bool {
    IN_FLUSH.load(core::sync::atomic::Ordering::Acquire)
}

/// Start writing the kernel's log to the log volume. Call once, after `/log`
/// is mounted.
pub fn install() {
    let mut vfs = vfs::lock();

    let existing = ours(&mut vfs);
    // One below the bound, because this boot's own file is about to become the
    // sixteenth. Reported into the ring, so the lines land in the file that
    // replaced them.
    let kept = sweep(&mut vfs, existing, MAX_LOG_FILES - 1);

    let stem = match clock::local_secs() {
        Some(secs) => stamp(secs),
        None => match undated_stem(&kept) {
            Some(stem) => stem,
            None => {
                log!("log-file: no free {UNDATED_STEM} name on {DIR}; this boot's log stays in memory");
                return;
            }
        },
    };
    // The first part number this boot's name does not already have on the
    // volume. Two boots inside one second is a machine nobody has, but a test
    // that stages the wall clock has it every run, and a colliding name would
    // silently write over the older boot.
    let Some(part) = (1..=MAX_LOG_PARTS).find(|p| !kept.contains(&path(&stem, *p))) else {
        log!("log-file: {DIR} already holds every part of {stem}; this boot's log stays in memory");
        return;
    };

    let path = path(&stem, part);
    let file_id = match vfs.create_file(&path, clock::nanos_since_boot()) {
        Ok(file_id) => file_id,
        Err(e) => {
            log!("log-file: cannot create {path}: {e}");
            return;
        }
    };
    drop(vfs);

    *SINK.lock() = Some(Sink {
        file_id,
        stem,
        part,
        size: 0,
        blocked_since: None,
        reported_lost: POSITION.lost(),
    });
    // The cursor has never moved, so the file opens with this boot's log from
    // its first record rather than from the moment `/boot` mounted — which is
    // four phases in, and exactly the part a machine that dies early needs.
    // That is what `enable_file_sink`'s seed from `retained` was for, without
    // the 64 KiB window.
    COLLECTING.store(true, Ordering::Relaxed);
    log!("log-file: this boot's kernel log is {path}");
}

/// Where this boot's log can be read, or `None` when no sink installed — no
/// `/log`, or a volume that would not give back the file.
pub fn destination() -> Option<String> {
    let guard = SINK.lock();
    let sink = guard.as_ref()?;
    Some(path(&sink.stem, sink.part))
}

/// Move whatever the shards owe into the file. Called from the idle loop.
pub fn poll() {
    if !has_pending() {
        return;
    }
    // `try_lock` on both: the pass this CPU is about to run matters more than a
    // log line, and the next trip round the loop finds the bytes still pending.
    let Some(mut guard) = SINK.try_lock() else { return };
    let Some(sink) = guard.as_mut() else { return };

    let Some(mut vfs) = vfs::try_lock() else {
        let now = clock::nanos_since_boot();
        let since = *sink.blocked_since.get_or_insert(now);
        if now.saturating_sub(since) < MAX_BLOCKED_NANOS {
            return;
        }
        let stopped = sink.stopped_at();
        *guard = None;
        drop(guard);
        COLLECTING.store(false, Ordering::Relaxed);
        log!(
            "log-file: the VFS lock has been held for {}s — {stopped}",
            MAX_BLOCKED_NANOS / 1_000_000_000
        );
        return;
    };
    sink.blocked_since = None;
    IN_FLUSH.store(true, core::sync::atomic::Ordering::Release);
    let outcome = sink.flush(&mut vfs);
    IN_FLUSH.store(false, core::sync::atomic::Ordering::Release);
    drop(vfs);
    if let Err((step, err)) = outcome {
        let stopped = sink.stopped_at();
        *guard = None;
        drop(guard);
        COLLECTING.store(false, Ordering::Relaxed);
        log!("log-file: {step} was refused ({err}) — {stopped}");
    }
}

/// Write everything out before the machine powers off.
///
/// Ordinary thread context — `SYS_SHUTDOWN` has released the VFS lock by the
/// time this runs — so it blocks on the locks rather than declining. A shutdown
/// that loses its own last lines is the one nobody can diagnose, and on a
/// machine with no serial port those lines exist nowhere else.
pub fn flush_final() {
    let mut guard = SINK.lock();
    let Some(sink) = guard.as_mut() else { return };
    let mut vfs = vfs::lock();
    let outcome = sink.flush(&mut vfs);
    drop(vfs);
    if let Err((step, err)) = outcome {
        drop(guard);
        log!("log-file: the final flush was refused at {step}: {err}");
    }
}

/// Where a flush stopped, and what it was told there.
///
/// The code alone does not identify the fault: `Io` from the append is a page
/// the stick would not give back, and `Io` from the sync is a device that would
/// not commit what it already took. The line naming one of them is the last
/// thing the sink ever writes, and it is written after the sink is disabled —
/// which is why the step has to travel out of here rather than be logged where
/// it happened.
type Refusal = (&'static str, SyscallError);

/// One record, rendered as its line and appended.
///
/// **The renderer is the console's** (`log::console::write_line`), so `/log`
/// and the wire carry the same bytes out of the same implementation — a second
/// one here would be a second thing to keep agreeing with the panel. At L6 this
/// and the file go together and `logd` renders a wall clock instead.
struct Appender<'a> {
    sink: &'a mut Sink,
    moved: usize,
    failed: Option<SyscallError>,
}

impl RecordSink for Appender<'_> {
    fn put(&mut self, record: &LogRecord) -> bool {
        let sink = &mut *self.sink;
        let mut wrote = 0usize;
        let mut failed = None;
        crate::log::console::write_line(record, |bytes| {
            // A line long enough to spill arrives in more than one piece, and
            // once the device has refused one the rest are not attempted.
            if failed.is_some() {
                return;
            }
            match sink.append(bytes) {
                Ok(()) => wrote += bytes.len(),
                Err(e) => failed = Some(e),
            }
        });
        self.moved += wrote;
        self.failed = failed;
        self.failed.is_none()
    }
}

impl Sink {
    fn flush(&mut self, vfs: &mut Vfs) -> Result<(), Refusal> {
        let lost = POSITION.lost();
        if lost > self.reported_lost {
            // Emitted rather than written, so this line is itself a record in
            // the batch below and the hole is described in the file it is a
            // hole in.
            log!(
                "log-file: {} record(s) were overwritten in a shard before they reached the file",
                lost - self.reported_lost
            );
            self.reported_lost = lost;
        }

        // **The cursor is taken and put back around the walk, and the walk is
        // where the appending happens.** `RecordSink::put` answering `false`
        // leaves `drain_ordered` with the cursor *before* the record it could
        // not write, so a refused append loses nothing that a later flush could
        // still have carried — and there is no later flush, because the caller
        // disables the sink on the error this returns.
        let mut cursor = POSITION.take();
        let (moved, failed) = {
            let mut out = Appender { sink: self, moved: 0, failed: None };
            drain_ordered(&mut cursor, &mut out);
            (out.moved, out.failed)
        };
        POSITION.put(&cursor);
        if let Some(e) = failed {
            return Err(("the append", e));
        }
        if moved == 0 {
            return Ok(());
        }

        let path = path(&self.stem, self.part);
        vfs.flush_file(&path, self.file_id, clock::nanos_since_boot())
            .map_err(|e| ("the write-back", e))?;
        // The FAT and the directory entry have reached the device; the device's
        // own write cache has not. A log that survives a wedge has to survive
        // the power being cut with it, which is the whole point, so the flush
        // is not optional and is per batch rather than per line.
        vfs.sync_mount(MOUNT).map_err(|e| ("the volume sync", e))?;

        if self.size >= max_log_bytes() {
            self.continue_in_next_part(vfs).map_err(|e| ("the rotation", e))?;
        }
        Ok(())
    }

    /// Append into the file cache, page by page, exactly as `fd::write` does.
    ///
    /// The error is the one this sink most needs to hear. An append is almost
    /// always a partial write — [`Self::size`] is rarely a multiple of 4096 —
    /// so once the tail page has been evicted, every line goes through a
    /// re-read of it from the stick. If that read fails and the cache merges
    /// into zeros anyway, the flush below writes 4 KiB of zeros over the log
    /// this feature exists to produce, from the idle loop, on the one device in
    /// the machine that can be pulled out. Propagating instead disables the
    /// sink, which is what [`poll`] already does with a flush error.
    fn append(&mut self, data: &[u8]) -> Result<(), SyscallError> {
        let mut done = 0usize;
        while done < data.len() {
            let page = (self.size / 4096) as u32;
            let within = (self.size % 4096) as usize;
            let n = (4096 - within).min(data.len() - done);
            file_cache::write_page(self.file_id, page, within, &data[done..done + n])
                .map_err(|_| SyscallError::Io)?;
            self.size += n as u64;
            done += n;
        }
        Ok(())
    }

    /// Carry on in the next file of this boot's sequence.
    ///
    /// Neither end of a long boot's log is dropped: the earlier parts stay
    /// until [`MAX_LOG_FILES`] reaches them, and by then they are the oldest
    /// files on the volume by the same rule that governs every other boot's.
    fn continue_in_next_part(&mut self, vfs: &mut Vfs) -> Result<(), SyscallError> {
        let full = path(&self.stem, self.part);
        let bytes = self.size;
        if file_cache::release(self.file_id) {
            vfs.close_file(&full, self.file_id);
        }
        if self.part >= MAX_LOG_PARTS {
            return Err(SyscallError::ResourceExhausted);
        }

        let existing = ours(vfs);
        sweep(vfs, existing, MAX_LOG_FILES - 1);

        self.part += 1;
        self.size = 0;
        let next = path(&self.stem, self.part);
        self.file_id = vfs.create_file(&next, clock::nanos_since_boot())?;
        log!("log-file: {full} reached {bytes} bytes and this boot continues in {next}");
        Ok(())
    }

    fn stopped_at(&self) -> String {
        let path = path(&self.stem, self.part);
        alloc::format!("{path} stops at {} bytes", self.size)
    }
}

/// The name of one file in this boot's sequence.
///
/// The first part carries the bare stem, because that is what nearly every boot
/// ever writes and a `_0001` on it would be noise on every stick. A
/// continuation takes `_` rather than any other separator for one reason: it is
/// the only legal character that sorts *after* `.`, so `<stem>.log` still comes
/// before `<stem>_0002.log` and retention deletes a boot's parts in the order
/// they were written.
fn path(stem: &str, part: u32) -> String {
    match part {
        1 => alloc::format!("{DIR}/{stem}.log"),
        n => alloc::format!("{DIR}/{stem}_{n:04}.log"),
    }
}

fn stamp(secs: u64) -> String {
    let t = Civil::from_unix_secs(secs);
    alloc::format!(
        "{:04}-{:02}-{:02}-{:02}{:02}{:02}",
        t.year, t.month, t.day, t.hour, t.min, t.sec
    )
}

/// Where a file sits in the order retention deletes in: lower goes first.
///
/// Undated boots go before dated ones because they cannot be ordered against
/// them — there is no clock to compare — and of the two kinds, the one that can
/// be placed in time is the one worth keeping. Within a kind the name is the
/// order, which is what the timestamp format is for.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    Undated,
    Dated,
}

/// Whether `name` is one of this module's files, and which kind.
///
/// Strict on purpose. `/log` is writable by userland and `toybox` writes there,
/// so anything this does not recognise exactly is somebody else's file and is
/// never deleted to make room.
fn classify(name: &str) -> Option<Class> {
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

    // What `stamp` produces and nothing else: `2101-06-05-040302`.
    let shape = b"dddd-dd-dd-dddddd";
    if stem.len() != shape.len() {
        return None;
    }
    let matches = stem.bytes().zip(shape).all(|(b, want)| match want {
        b'd' => b.is_ascii_digit(),
        c => b == *c,
    });
    matches.then_some(Class::Dated)
}

/// Every file on `/log` that this module wrote, oldest first.
fn ours(vfs: &mut Vfs) -> Vec<String> {
    let entries = match vfs.list("/", DIR) {
        Ok(entries) => entries,
        Err(e) => {
            log!("log-file: cannot list {DIR} ({e:?}), so nothing old can be cleared out");
            return Vec::new();
        }
    };
    let mut ours: Vec<(Class, String)> = entries
        .into_iter()
        .filter_map(|(name, _size)| Some((classify(&name)?, name)))
        .collect();
    ours.sort();
    ours.into_iter().map(|(_, name)| alloc::format!("{DIR}/{name}")).collect()
}

/// Delete this module's oldest files until at most `keep` remain, and return
/// what is left.
fn sweep(vfs: &mut Vfs, existing: Vec<String>, keep: usize) -> Vec<String> {
    let over = existing.len().saturating_sub(keep);
    let mut kept = Vec::with_capacity(existing.len() - over);
    for (i, path) in existing.into_iter().enumerate() {
        if i >= over {
            kept.push(path);
            continue;
        }
        // Named, because a file disappearing off the owner's stick with nothing
        // saying why is indistinguishable from a bug in this module.
        match vfs.delete_file(&path) {
            Ok(()) => log!("log-file: {DIR} holds more than {MAX_LOG_FILES} logs, so {path} was deleted"),
            Err(e) => {
                log!("log-file: {path} is past the {MAX_LOG_FILES}-log bound and would not delete: {e}");
                kept.push(path);
            }
        }
    }
    kept
}

/// The lowest index no undated log on the volume is using.
///
/// `None` cannot happen after a sweep — it leaves fewer files than there are
/// indices — but it is the return type rather than an `expect` because the
/// caller has somewhere to put the answer and this module does not panic.
fn undated_stem(kept: &[String]) -> Option<String> {
    (0..MAX_LOG_FILES)
        .map(|i| alloc::format!("{UNDATED_STEM}-{i:02}"))
        .find(|stem| !kept.iter().any(|path| path.starts_with(&alloc::format!("{DIR}/{stem}"))))
        .map(|stem| stem.to_string())
}

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
//! Continuously, from the idle loop, beside `drain_serial`. The failure this
//! exists for is a machine that *stops* — a wedge, a livelock, a hang with no
//! panic and no console — and for that, "the tail is on disk" is the whole
//! requirement. The alternative the owner asked about, a flush at boot-complete
//! plus a periodic one, saves I/O and loses exactly the evidence the feature is
//! for.
//!
//! It costs nothing when nothing is logged: [`log_ring::file_has_pending`] is
//! one relaxed atomic load. And it is not on the logging hot path at all —
//! `log!` still does nothing but append to the ring under a brief spinlock, as
//! it did before, so nothing that logs while holding a kernel lock does I/O.
//!
//! Batching needs no tuning and has no period. A busy machine reaches the idle
//! loop rarely, so each flush carries more; an idle one reaches it constantly
//! and each flush carries a line or two. The same argument `drain_serial` makes
//! for the console, with a device write in place of a UART byte.
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
//! worst a half-finished write can cost is this file and the one generation
//! beside it, which is a diagnostic and not the stick. The deadlock is what
//! keeps the panic path out, and it is enough on its own.
//!
//! The panic path keeps the on-screen console, which takes no lock of any kind.
//! What the file gives a panic is everything up to the last idle pass — on any
//! machine that reached the idle loop, everything but the panic itself, and the
//! panic is on the screen.

use crate::drivers::log_ring;
use crate::file_cache::{self, FileId};
use crate::sync::Lock;
use crate::vfs::{self, Vfs};

/// Where the log goes.
///
/// The root of the log partition, so that plugging the stick into another
/// machine puts `kernel.log` at the top of the window that opens. Nothing else
/// is on this volume and nothing else is meant to be.
const PATH: &str = "/log/kernel.log";
/// What the previous [`PATH`] becomes when it fills.
const PREVIOUS: &str = "/log/kernel.log.1";

/// The mount [`PATH`] is on, for the per-mount sync each flush ends with.
const MOUNT: &str = "log";

/// How large [`PATH`] may get before it is rotated.
///
/// Derived from the partition, not picked. `create_log_volume` makes the
/// smallest volume there is a FAT32 for, and `fsck_msdos` reports 68,551 free
/// clusters of 512 bytes on a fresh one — 35,098,112 bytes. Two generations at
/// this bound are 8 MiB, under a quarter of that, so a boot that rotates
/// repeatedly cannot fill the volume and there is room left for anything a
/// later diagnostic wants to drop beside it.
///
/// The other end of the bound is that both generations are *read* in full:
/// `/bin/console` seeds its scrollback from them at every framebuffer boot,
/// off USB, before it paints anything. That is what stops this being the
/// quarter-of-the-volume 8 MiB the space alone would allow.
///
/// For scale rather than as a limit: a metal-sim boot's whole log to
/// `Shutting down.` measured 7,910 bytes, so 4 MiB is several hundred boots of
/// history in the current generation alone.
///
/// The rotate-fast value exists for the same reason `test-small-caches` does:
/// filling megabytes by logging would take a boot far longer than a test should
/// wait, and the rotation code it drives is the shipped code — only the bound
/// moves.
///
/// 256 rather than something rounder, and the number is measured. Of such a
/// boot, 7,052 bytes are already in the ring when the sink installs and go out
/// in one flush. At 1 KiB that is a *single* rotation — the remaining 858 bytes
/// never fill a second file — and a single rotation never renames over an
/// existing `kernel.log.1`, which is the half of the path that has to delete
/// first. 256 rotates three or four times on the same boot depending on how the
/// flushes fall, and `kernel_log_file` requires at least two.
const MAX_LOG_BYTES: u64 = if cfg!(feature = "log-rotate-fast") {
    256
} else {
    4 * 1024 * 1024
};

/// Bytes moved per pass of the drain loop, and the size of the stack buffer it
/// goes through.
///
/// 512 because this runs on the 16 KiB per-CPU idle stack, which is a heap
/// allocation with no guard page — an overflow there corrupts the heap silently
/// rather than faulting. Same number and same reason as
/// [`log_ring::DRAIN_CHUNK`].
const CHUNK: usize = 512;

/// How long the VFS lock may stay held before the sink gives up on it.
///
/// It bounds the one case that never clears — a thread that panicked holding
/// the VFS lock, which known issues records as live. Without it the ring would
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
    /// Bytes in [`PATH`] so far, which is what decides the next page index.
    /// Kept here rather than read back from the file cache so a disagreement
    /// shows up as a wrong offset rather than being silently corrected.
    size: u64,
    /// When the current run of polls that found the VFS lock held began.
    blocked_since: Option<u64>,
}

static SINK: Lock<Option<Sink>> = Lock::new(None);

/// Start writing the kernel's log to the log volume. Call once, after `/log`
/// is mounted.
///
/// Appends across boots rather than truncating: a machine that failed on its
/// second boot is diagnosed by having the first one to compare against, and
/// [`MAX_LOG_BYTES`] is what stops that growing without end.
pub fn install() {
    let mtime = crate::clock::nanos_since_boot();
    let opened = {
        let mut vfs = vfs::lock();
        match vfs.open_file(PATH) {
            Some(file_id) => Ok((file_id, file_cache::size(file_id))),
            None => vfs.create_file(PATH, mtime).map(|file_id| (file_id, 0)),
        }
    };
    let (file_id, size) = match opened {
        Ok(pair) => pair,
        Err(e) => {
            log!("log-file: cannot open {PATH}: {e}");
            return;
        }
    };
    *SINK.lock() = Some(Sink { file_id, size, blocked_since: None });
    log_ring::enable_file_sink();
    log!("log-file: this boot's kernel log continues in {PATH}, which holds {size} bytes");
}

/// Where this boot's log can be read, or `None` when no sink installed — no
/// `/log`, or a volume that would not give back the file.
pub fn destination() -> Option<&'static str> {
    SINK.lock().is_some().then_some(PATH)
}

/// Move whatever the ring owes into the file. Called from the idle loop.
pub fn poll() {
    if !log_ring::file_has_pending() {
        return;
    }
    // `try_lock` on both: the pass this CPU is about to run matters more than a
    // log line, and the next trip round the loop finds the bytes still pending.
    let Some(mut guard) = SINK.try_lock() else { return };
    let Some(sink) = guard.as_mut() else { return };

    let Some(mut vfs) = vfs::try_lock() else {
        let now = crate::clock::nanos_since_boot();
        let since = *sink.blocked_since.get_or_insert(now);
        if now.saturating_sub(since) < MAX_BLOCKED_NANOS {
            return;
        }
        let size = sink.size;
        *guard = None;
        drop(guard);
        log_ring::disable_file_sink();
        log!(
            "log-file: the VFS lock has been held for {}s — {PATH} stops at {size} bytes",
            MAX_BLOCKED_NANOS / 1_000_000_000
        );
        return;
    };
    sink.blocked_since = None;
    let outcome = sink.flush(&mut vfs);
    drop(vfs);
    if let Err(e) = outcome {
        let size = sink.size;
        *guard = None;
        drop(guard);
        log_ring::disable_file_sink();
        log!("log-file: {e} — {PATH} stops at {size} bytes");
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
    if let Err(e) = outcome {
        drop(guard);
        log!("log-file: the final flush failed: {e}");
    }
}

impl Sink {
    fn flush(&mut self, vfs: &mut Vfs) -> Result<(), &'static str> {
        let lost = log_ring::take_file_drops();
        if lost > 0 {
            // Into the ring, so this line is itself in the batch below and the
            // hole is described in the file it is a hole in.
            log!("log-file: {lost} bytes were overwritten in the ring before they reached {PATH}");
        }

        let mut buf = [0u8; CHUNK];
        let mut moved = 0usize;
        loop {
            let n = log_ring::drain_to_file(&mut buf);
            if n == 0 {
                break;
            }
            self.append(&buf[..n])?;
            moved += n;
        }
        if moved == 0 {
            return Ok(());
        }

        vfs.flush_file(PATH, self.file_id, crate::clock::nanos_since_boot())?;
        // The FAT and the directory entry have reached the device; the device's
        // own write cache has not. A log that survives a wedge has to survive
        // the power being cut with it, which is the whole point, so the flush
        // is not optional and is per batch rather than per line.
        vfs.sync_mount(MOUNT)?;

        if self.size >= MAX_LOG_BYTES {
            self.rotate(vfs)?;
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
    fn append(&mut self, data: &[u8]) -> Result<(), &'static str> {
        let mut done = 0usize;
        while done < data.len() {
            let page = (self.size / 4096) as u32;
            let within = (self.size % 4096) as usize;
            let n = (4096 - within).min(data.len() - done);
            file_cache::write_page(self.file_id, page, within, &data[done..done + n])
                .map_err(|_| "the log volume would not give back the page being appended to")?;
            self.size += n as u64;
            done += n;
        }
        Ok(())
    }

    /// Make room by moving the full file aside, keeping one generation.
    ///
    /// One and not more: two files is the difference between "the log ends
    /// where it filled up" and "there is a log", while every further generation
    /// costs another [`MAX_LOG_BYTES`] of the volume, and another read of it
    /// every time `/bin/console` seeds a screen, for a boot they are less
    /// likely to care about.
    fn rotate(&mut self, vfs: &mut Vfs) -> Result<(), &'static str> {
        let full = self.file_id;
        let bytes = self.size;
        vfs.rename(PATH, PREVIOUS)?;
        if file_cache::release(full) {
            vfs.close_file(PREVIOUS, full);
        }
        self.file_id = vfs.create_file(PATH, crate::clock::nanos_since_boot())?;
        self.size = 0;
        log!("log-file: {PATH} reached {bytes} bytes and became {PREVIOUS}");
        Ok(())
    }
}

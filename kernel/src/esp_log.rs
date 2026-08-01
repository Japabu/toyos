//! The kernel's log, as a file on the partition it booted from.
//!
//! A ThinkPad T14 Gen 2 has no serial port. Once the compositor claims the
//! framebuffer the kernel's only remaining channel is the on-screen console,
//! which paints at the six boot checkpoints and on a fatal panic and at no
//! other time — so between `Boot: complete` and a panic the machine says
//! nothing at all, which is why a real boot failure there was undiagnosable.
//! This gives it a channel that survives the power being cut: the stick it
//! booted from.
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
//! Stated rather than attempted. A panic-time flush would need, in order: this
//! module's lock, the VFS lock, the file cache lock, the kernel heap
//! (`toyos-fat32` keeps its sector scratch in a `Vec` and builds a `String` per
//! path component), the ESP device lock, and the xHCI controller lock. A
//! panicking thread may hold any of them — the VFS lock especially, since every
//! reachable kernel panic from filesystem code holds it — so a panic-time flush
//! would deadlock in precisely the cases the log is for.
//!
//! And the deadlock is the *good* outcome. This volume holds `BOOTx64.EFI` and
//! `kernel.elf`: a FAT write interrupted between allocating a cluster and
//! recording it leaves a volume that may not boot, so a half-finished
//! panic-time write trades a diagnostic for the machine. The panic path keeps
//! the on-screen console, which takes no lock of any kind. What the file gives
//! a panic is everything up to the last idle pass — on any machine that reached
//! the idle loop, everything but the panic itself, and the panic is on the
//! screen.

use crate::drivers::log_ring;
use crate::file_cache::{self, FileId};
use crate::sync::Lock;
use crate::vfs::{self, Vfs};

/// Where the log goes.
///
/// Beside `kernel.elf` and `initrd.img`, in the directory the bootloader
/// already reads from: it exists on every ToyOS stick by construction, so
/// nothing here creates a directory on somebody's ESP, and a human looking for
/// the log finds it next to the things they flashed.
const PATH: &str = "/boot/toyos/kernel.log";
/// What the previous [`PATH`] becomes when it fills.
const PREVIOUS: &str = "/boot/toyos/kernel.log.1";

/// The mount [`PATH`] is on, for the per-mount sync each flush ends with.
const MOUNT: &str = "boot";

/// How large [`PATH`] may get before it is rotated.
///
/// Derived from the space a ToyOS stick guarantees, not picked.
/// `create_fat_volume` sizes the ESP at `content + 4 MiB`, so 4 MiB is the
/// least slack any image has; two files of 1 MiB take half of it and leave the
/// rest for anything else that ever wants to write there.
///
/// The rotate-fast value exists for the same reason `test-small-caches` does:
/// filling a megabyte by logging would take a boot far longer than a test
/// should wait, and the rotation code it drives is the shipped code — only the
/// bound moves.
///
/// 256 rather than something rounder, and the number is measured. A metal-sim
/// boot's whole log to `Shutting down.` is 7,348 bytes, of which 6,484 are
/// already in the ring when the sink installs and go out in one flush. At 1 KiB
/// that is a *single* rotation — the remaining 864 bytes never fill a second
/// file — and a single rotation never renames over an existing `kernel.log.1`,
/// which is the half of the path that has to delete first. 256 rotates three
/// times on the same boot, which `esp_log_file` asserts.
const MAX_LOG_BYTES: u64 = if cfg!(feature = "esp-log-rotate-fast") {
    256
} else {
    1024 * 1024
};

/// Bytes moved per pass of the drain loop, and the size of the stack buffer it
/// goes through.
///
/// 512 because this runs on the 16 KiB per-CPU idle stack, which is a heap
/// allocation with no guard page — an overflow there corrupts the heap silently
/// rather than faulting. Same number and same reason as
/// [`log_ring::DRAIN_CHUNK`].
const CHUNK: usize = 512;

/// Consecutive polls that may find the VFS lock held before the sink gives up.
///
/// Not a timeout on a busy filesystem: a lock holder runs with preemption
/// disabled and always makes progress, so ordinary contention clears in
/// microseconds. It bounds the one case that never clears — a thread that
/// panicked holding the VFS lock, which known issues records as live. Without
/// it the ring would report bytes pending forever and the idle loop, which
/// declines to sleep on that, would spin a CPU with nothing to do about it.
const MAX_BLOCKED_POLLS: u32 = 1000;

struct Sink {
    file_id: FileId,
    /// Bytes in [`PATH`] so far, which is what decides the next page index.
    /// Kept here rather than read back from the file cache so a disagreement
    /// shows up as a wrong offset rather than being silently corrected.
    size: u64,
    blocked: u32,
}

static SINK: Lock<Option<Sink>> = Lock::new(None);

/// Start writing the kernel's log to the boot volume. Call once, after `/boot`
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
            log!("esp-log: cannot open {PATH}: {e}");
            return;
        }
    };
    *SINK.lock() = Some(Sink { file_id, size, blocked: 0 });
    log_ring::enable_file_sink();
    log!("esp-log: this boot's kernel log continues in {PATH}, which holds {size} bytes");
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
        sink.blocked += 1;
        if sink.blocked < MAX_BLOCKED_POLLS {
            return;
        }
        let size = sink.size;
        *guard = None;
        drop(guard);
        log_ring::disable_file_sink();
        log!(
            "esp-log: the VFS lock has been held for {MAX_BLOCKED_POLLS} consecutive polls — \
             {PATH} stops at {size} bytes"
        );
        return;
    };
    sink.blocked = 0;
    let outcome = sink.flush(&mut vfs);
    drop(vfs);
    if let Err(e) = outcome {
        let size = sink.size;
        *guard = None;
        drop(guard);
        log_ring::disable_file_sink();
        log!("esp-log: {e} — {PATH} stops at {size} bytes");
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
        log!("esp-log: the final flush failed: {e}");
    }
}

impl Sink {
    fn flush(&mut self, vfs: &mut Vfs) -> Result<(), &'static str> {
        let lost = log_ring::take_file_drops();
        if lost > 0 {
            // Into the ring, so this line is itself in the batch below and the
            // hole is described in the file it is a hole in.
            log!("esp-log: {lost} bytes were overwritten in the ring before they reached {PATH}");
        }

        let mut buf = [0u8; CHUNK];
        let mut moved = 0usize;
        loop {
            let n = log_ring::drain_to_file(&mut buf);
            if n == 0 {
                break;
            }
            self.append(&buf[..n]);
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
        vfs.sync_mount(MOUNT);

        if self.size >= MAX_LOG_BYTES {
            self.rotate(vfs)?;
        }
        Ok(())
    }

    /// Append into the file cache, page by page, exactly as `fd::write` does.
    fn append(&mut self, data: &[u8]) {
        let mut done = 0usize;
        while done < data.len() {
            let page = (self.size / 4096) as u32;
            let within = (self.size % 4096) as usize;
            let n = (4096 - within).min(data.len() - done);
            file_cache::write_page(self.file_id, page, within, &data[done..done + n]);
            self.size += n as u64;
            done += n;
        }
    }

    /// Make room by moving the full file aside, keeping one generation.
    ///
    /// One and not more: two files is the difference between "the log ends
    /// where it filled up" and "there is a log", while every further generation
    /// costs another [`MAX_LOG_BYTES`] of somebody's boot stick for a boot they
    /// are less likely to care about.
    fn rotate(&mut self, vfs: &mut Vfs) -> Result<(), &'static str> {
        let full = self.file_id;
        let bytes = self.size;
        vfs.rename(PATH, PREVIOUS)?;
        if file_cache::release(full) {
            vfs.close_file(PREVIOUS, full);
        }
        self.file_id = vfs.create_file(PATH, crate::clock::nanos_since_boot())?;
        self.size = 0;
        log!("esp-log: {PATH} reached {bytes} bytes and became {PREVIOUS}");
        Ok(())
    }
}

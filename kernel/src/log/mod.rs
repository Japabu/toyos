//! The kernel's only log producer.
//!
//! `log!`, `alert!` and `boot_phase!` all expand to [`emit`], which takes
//! `fmt::Arguments` and nothing else. **There is no byte-oriented entry point**,
//! and that is what makes "half a record" untypeable: the smallest thing this
//! module accepts is a whole one.
//!
//! `specs/log-architecture-spec.md` §2.

pub mod shard;

use core::sync::atomic::{AtomicBool, Ordering};

use toyos_abi::log::{LogRecord, FLAG_EARLY, MAX_RECORD_MESSAGE};

pub use shard::Shard;
pub use toyos_abi::log::Level;

/// Set to true by `percpu::init_bsp` once GS base is valid.
/// Before this, reading `gs:` would fault on a garbage GS base.
pub static PERCPU_READY: AtomicBool = AtomicBool::new(false);

/// cpu0's shard, and the boot shard, because they are the same thing.
///
/// A `static` rather than a heap allocation because `log!` runs before the heap
/// exists and before `PERCPU_READY`. **There is no boot-shard-to-cpu0-shard
/// handoff to get wrong**, and today's `boot` label in the prefix becomes
/// [`FLAG_EARLY`] on a record in this same shard.
///
/// Zeroed, so it costs `.bss` and not 128 KiB of kernel image.
pub static BOOT_SHARD: Shard = Shard::new();

/// A message being formatted, on the caller's stack.
///
/// Formatting happens here rather than inside any critical section: today it
/// happens inside `SerialWriter`, which is at least something the code claims
/// to be a lock.
struct MessageBuf {
    msg: [u8; MAX_RECORD_MESSAGE],
    len: usize,
    /// Bytes that did not fit, saturating. **Counted rather than dropped** —
    /// the difference between a bound and a lie.
    elided: usize,
}

impl core::fmt::Write for MessageBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = MAX_RECORD_MESSAGE - self.len;
        let bytes = s.as_bytes();
        if bytes.len() <= room {
            self.msg[self.len..self.len + bytes.len()].copy_from_slice(bytes);
            self.len += bytes.len();
            return Ok(());
        }
        // Split on a character boundary: a record whose tail is half a UTF-8
        // sequence renders as a truncated line for every consumer, and the one
        // that matters paints a panel.
        let mut fit = room;
        while fit > 0 && !s.is_char_boundary(fit) {
            fit -= 1;
        }
        self.msg[self.len..self.len + fit].copy_from_slice(&bytes[..fit]);
        self.len += fit;
        self.elided = self.elided.saturating_add(bytes.len() - fit);
        Ok(())
    }
}

/// Where this record goes and who is writing it.
struct Origin {
    shard: &'static Shard,
    cpu: u16,
    tid: u32,
    pid: u32,
    flags: u8,
}

/// This CPU's shard, its identity, and the sequence number this record owns.
///
/// **The bracket is not optional and it is exactly three instructions wide.**
/// A non-`lock`-prefixed `xadd` is atomic against an interrupt on its own CPU
/// and not against another CPU, so the design is sound only while the CPU
/// executing it owns the shard — and work stealing is on, so a kernel context
/// at preempt depth 0 with `IF` set can be moved between reading `gs:` and
/// performing the add. Two CPUs then read-modify-write one `head`, lose an
/// update, and two records share a slot.
///
/// A preemption *after* the add is harmless, which is why the bracket ends
/// there: the sequence number is already exclusively ours, and the body goes
/// into the shard this returned rather than into whatever `gs:` says by then.
///
/// `preempt::disable` would buy the same property for two locked
/// read-modify-writes per record, which is the cost this whole design exists to
/// avoid — one `fetch_add` per line cost 350 ms of boot under TCG. On the
/// dominant path the `popfq` restores a flag that was already clear, because
/// `IF` is clear for the whole of every syscall.
fn reserve() -> (Origin, u64) {
    if !PERCPU_READY.load(Ordering::Relaxed) {
        // One CPU, no scheduler, no GS base to read. An interrupt can still
        // land, and the `xadd` is still what makes that safe.
        //
        // SAFETY: nothing else is running, so this CPU owns the boot shard.
        let seq = unsafe { BOOT_SHARD.reserve() };
        let origin = Origin {
            shard: &BOOT_SHARD,
            cpu: 0,
            tid: u32::MAX,
            pid: u32::MAX,
            flags: FLAG_EARLY,
        };
        return (origin, seq);
    }

    let (shard, seq, cpu, tid, pid) = crate::arch::percpu::reserve_log_slot();
    // SAFETY: `reserve_log_slot` read this pointer out of this CPU's own
    // `PerCpu`, with interrupts masked across the read and the add, and
    // `alloc_percpu` gives every CPU a shard before that CPU executes an
    // instruction — so this is a live `Shard` and it is ours.
    let shard: &'static Shard = unsafe { &*shard };
    (Origin { shard, cpu: cpu as u16, tid, pid, flags: 0 }, seq)
}

/// The only producer.
///
/// Steps, in order, and only the second is uninterruptible: format on the
/// stack, reserve, fill the record, publish it.
pub fn emit(level: Level, args: core::fmt::Arguments) {
    let at_ns = crate::clock::nanos_since_boot();

    let mut buf = MessageBuf { msg: [0; MAX_RECORD_MESSAGE], len: 0, elided: 0 };
    let _ = core::fmt::Write::write_fmt(&mut buf, args);

    let (origin, seq) = reserve();

    let record = LogRecord {
        seq,
        at_ns,
        pid: origin.pid,
        tid: origin.tid,
        cpu: origin.cpu,
        len: buf.len as u16,
        elided: buf.elided.min(u16::MAX as usize) as u16,
        level: level as u8,
        flags: origin.flags,
        msg: buf.msg,
    };

    // SAFETY: `seq` came from this shard's own `reserve`, on this CPU, and is
    // published exactly once.
    unsafe { origin.shard.commit(seq, &record) };

    to_byte_ring(&record);
}

/// The old byte ring, still fed, because this chunk changes nothing anybody can
/// observe.
///
/// Every consumer — the serial drain, the file sink, the panel, `Ctrl+Alt+D` —
/// still reads bytes; L2 re-points them at records and L3 deletes this. Keeping
/// both live for two chunks is what lets the suite answer whether the record
/// ring costs anything, against a tree whose output is byte-identical.
fn to_byte_ring(record: &LogRecord) {
    use core::fmt::Write;
    let mut w = crate::drivers::serial::SerialWriter::lock();
    let secs = record.at_ns / 1_000_000_000;
    let millis = record.at_ns % 1_000_000_000 / 1_000_000;
    if record.is_early() {
        let _ = write!(w, "[kernel {secs}.{millis:03} boot] ");
    } else if record.tid == u32::MAX {
        let _ = write!(w, "[kernel {secs}.{millis:03} cpu{}] ", record.cpu);
    } else {
        let _ = write!(w, "[kernel {secs}.{millis:03} cpu{} tid={}] ", record.cpu, record.tid);
    }
    let _ = w.write_str(record.message());
    if record.elided != 0 {
        let _ = write!(w, " …[{} bytes elided]", record.elided);
    }
    let _ = w.write_str("\n");
}

/// A line of ordinary kernel log. 658 sites.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Info, format_args!($($arg)*))
    };
}

/// A refusal, a corruption, or a fault. **The panel paints the row red**, and
/// it does so because of this rather than because the message happens to
/// contain three exclamation marks.
#[macro_export]
macro_rules! alert {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Alert, format_args!($($arg)*))
    };
}

/// Announce a boot phase boundary: log how long it took, and repaint the
/// on-screen console so the last completed phase stays visible.
///
/// The two belong together. A machine that wedges without panicking calls
/// nothing, so the only thing that can distinguish "hung in xHCI" from "black
/// screen, no idea" is a checkpoint painted before it hung — and a checkpoint
/// nobody can see is not a checkpoint.
///
/// `$since` is the phase's start timestamp; pass 0 to measure from boot.
#[macro_export]
macro_rules! boot_phase {
    ($name:literal, $since:expr) => {{
        $crate::log::emit(
            $crate::log::Level::Phase,
            format_args!(
                "Boot: {} ({}ms)",
                $name,
                ($crate::clock::nanos_since_boot() - $since) / 1_000_000
            ),
        );
        $crate::drivers::panic_console::boot_checkpoint();
    }};
}

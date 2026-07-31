use core::sync::atomic::AtomicBool;

/// Set to true by `percpu::init_bsp` once GS base is valid.
/// Before this, reading gs:[8] or gs:[136] would fault on garbage GS base.
pub static PERCPU_READY: AtomicBool = AtomicBool::new(false);

/// Logs a formatted message to serial with timestamp, CPU, and TID context.
///
/// Before percpu init: `[kernel 0.000 boot] message`
/// Idle (no thread):   `[kernel 1.042 cpu0] message`
/// With thread:        `[kernel 1.042 cpu0 tid=3] message`
///
/// Acquires the serial lock once for the whole line so the prefix and body
/// can't interleave with output from another CPU.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let ts = $crate::clock::nanos_since_boot();
        let secs = ts / 1_000_000_000;
        let millis = (ts % 1_000_000_000) / 1_000_000;
        let mut __w = $crate::drivers::serial::SerialWriter::lock();
        if $crate::log::PERCPU_READY.load(core::sync::atomic::Ordering::Relaxed) {
            let cpu: u32;
            let tid: u32;
            unsafe {
                core::arch::asm!("mov {:e}, gs:[8]", out(reg) cpu, options(nomem, nostack, preserves_flags));
                core::arch::asm!("mov {:e}, gs:[136]", out(reg) tid, options(nomem, nostack, preserves_flags));
            }
            if tid == u32::MAX {
                let _ = write!(__w, "[kernel {}.{:03} cpu{}] ", secs, millis, cpu);
            } else {
                let _ = write!(__w, "[kernel {}.{:03} cpu{} tid={}] ", secs, millis, cpu, tid);
            }
        } else {
            let _ = write!(__w, "[kernel {}.{:03} boot] ", secs, millis);
        }
        let _ = writeln!(__w, $($arg)*);
    }};
}

/// Announce a boot phase boundary: log how long it took, and repaint the
/// on-screen console so the last completed phase stays visible.
///
/// The two belong together. A machine that wedges without panicking calls
/// nothing, so the only thing that can distinguish "hung in xHCI" from "black
/// screen, no idea" is a checkpoint painted before it hung — and a checkpoint
/// nobody can see is not a checkpoint. Mirroring `log!` itself was rejected:
/// that needs a screen path callable from IRQ context under arbitrary kernel
/// locks, with a cursor, a scroll, and a lock of its own that the panic
/// renderer would then have to take.
///
/// `$since` is the phase's start timestamp; pass 0 to measure from boot.
#[macro_export]
macro_rules! boot_phase {
    ($name:literal, $since:expr) => {{
        $crate::log!(
            "Boot: {} ({}ms)",
            $name,
            ($crate::clock::nanos_since_boot() - $since) / 1_000_000
        );
        $crate::drivers::panic_console::boot_checkpoint();
    }};
}

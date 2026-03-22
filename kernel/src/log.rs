use core::sync::atomic::AtomicBool;

/// Set to true by `percpu::init_bsp` once GS base is valid.
/// Before this, reading gs:[8] or gs:[136] would fault on garbage GS base.
pub static PERCPU_READY: AtomicBool = AtomicBool::new(false);

pub struct LogWriter;

impl core::fmt::Write for LogWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        crate::drivers::serial::write(s.as_bytes());
        Ok(())
    }
}

/// Logs a formatted message to serial with timestamp, CPU, and TID context.
///
/// Before percpu init: `[kernel 0.000 boot] message`
/// Idle (no thread):   `[kernel 1.042 cpu0] message`
/// With thread:        `[kernel 1.042 cpu0 tid=3] message`
///
/// Does not allocate — all formatting is stack-based via `format_args!`,
/// and output goes directly to the serial port.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let ts = $crate::clock::nanos_since_boot();
        let secs = ts / 1_000_000_000;
        let millis = (ts % 1_000_000_000) / 1_000_000;
        if $crate::log::PERCPU_READY.load(core::sync::atomic::Ordering::Relaxed) {
            let cpu: u32;
            let tid: u32;
            unsafe {
                core::arch::asm!("mov {:e}, gs:[8]", out(reg) cpu, options(nomem, nostack, preserves_flags));
                core::arch::asm!("mov {:e}, gs:[136]", out(reg) tid, options(nomem, nostack, preserves_flags));
            }
            if tid == u32::MAX {
                let _ = write!($crate::log::LogWriter, "[kernel {}.{:03} cpu{}] ", secs, millis, cpu);
            } else {
                let _ = write!($crate::log::LogWriter, "[kernel {}.{:03} cpu{} tid={}] ", secs, millis, cpu, tid);
            }
        } else {
            let _ = write!($crate::log::LogWriter, "[kernel {}.{:03} boot] ", secs, millis);
        }
        let _ = writeln!($crate::log::LogWriter, $($arg)*);
    }};
}

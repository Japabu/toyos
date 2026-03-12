pub struct LogWriter;

impl core::fmt::Write for LogWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        crate::drivers::serial::print(s);
        Ok(())
    }
}

/// Logs a formatted message to serial with a `[kernel]` prefix.
///
/// Does not allocate — all formatting is stack-based via `format_args!`,
/// and output goes directly to the serial port.
///
/// Usage: `log!("booting core {}", core_id);`
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = write!($crate::log::LogWriter, "[kernel] ");
        let _ = writeln!($crate::log::LogWriter, $($arg)*);
    }};
}

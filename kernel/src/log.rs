pub struct LogWriter;

impl core::fmt::Write for LogWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        crate::drivers::serial::print(s);
        Ok(())
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = writeln!($crate::log::LogWriter, $($arg)*);
    }};
}

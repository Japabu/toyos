pub struct LogWriter;

#[cfg(not(test))]
impl core::fmt::Write for LogWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        crate::drivers::serial::print(s);
        Ok(())
    }
}

#[cfg(test)]
impl core::fmt::Write for LogWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        std::print!("{}", s);
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

#[cfg(not(test))]
pub fn println(s: &str) {
    crate::drivers::serial::println(s);
}

#[cfg(test)]
pub fn println(s: &str) {
    std::println!("{}", s);
}

// ToyOS platform module (replaces kibi's unix.rs/windows.rs/wasi.rs)
//
// ToyOS keyboard is already raw (no line discipline), so enable_raw_mode is a
// no-op. Window size comes from a syscall. No signals exist.

use std::io::{self, BufRead, BufReader};

use crate::Error;

#[derive(Copy, Clone)]
pub struct TermMode;

// Syscall numbers (must match kernel)
const SYS_SCREEN_SIZE: u64 = 7;

#[inline]
fn syscall(num: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rdi") num,
            in("rsi") 0u64,
            in("rdx") 0u64,
            in("r8") 0u64,
            in("r9") 0u64,
            out("rax") ret,
            out("rcx") _,
            out("r11") _,
        );
    }
    ret
}

pub fn get_window_size() -> Result<(usize, usize), Error> {
    let v = syscall(SYS_SCREEN_SIZE);
    let cols = (v & 0xFFFF_FFFF) as usize;
    let rows = (v >> 32) as usize;
    if cols == 0 || rows == 0 {
        Err(Error::InvalidWindowSize)
    } else {
        Ok((rows, cols))
    }
}

pub const fn register_winsize_change_signal_handler() -> io::Result<()> { Ok(()) }

pub const fn has_window_size_changed() -> bool { false }

pub fn set_term_mode(_term: &TermMode) -> io::Result<()> { Ok(()) }

pub fn enable_raw_mode() -> io::Result<TermMode> { Ok(TermMode) }

pub fn stdin() -> io::Result<impl BufRead> { Ok(BufReader::new(io::stdin())) }

pub fn path(filename: &str) -> std::path::PathBuf { std::path::PathBuf::from(filename) }

pub fn conf_dirs() -> Vec<String> { Vec::new() }

pub fn data_dirs() -> Vec<String> { Vec::new() }

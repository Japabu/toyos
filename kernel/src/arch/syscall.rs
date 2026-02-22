use core::arch::{asm, naked_asm};

use alloc::vec::Vec;
use super::{cpu, gdt};
use crate::drivers::{acpi, serial};
use crate::sync::SyncCell;
use crate::{console, keyboard, user_heap, vfs};

// MSR addresses
const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_FMASK: u32 = 0xC000_0084;

// Syscall numbers (must match toolchain patches)
const SYS_WRITE: u64 = 0;
const SYS_READ: u64 = 1;
const SYS_ALLOC: u64 = 2;
const SYS_FREE: u64 = 3;
const SYS_REALLOC: u64 = 4;
const SYS_EXIT: u64 = 5;
const SYS_RANDOM: u64 = 6;
const SYS_SCREEN_SIZE: u64 = 7;
const SYS_CLOCK: u64 = 8;
const SYS_OPEN: u64 = 9;
const SYS_CLOSE: u64 = 10;
const SYS_READ_FILE: u64 = 11;
const SYS_WRITE_FILE: u64 = 12;
const SYS_SEEK: u64 = 13;
const SYS_FSTAT: u64 = 14;
const SYS_FSYNC: u64 = 15;
const SYS_EXEC: u64 = 16;
const SYS_READDIR: u64 = 17;
const SYS_DELETE: u64 = 18;
const SYS_SHUTDOWN: u64 = 19;
const SYS_CHDIR: u64 = 20;
const SYS_GETCWD: u64 = 21;
const SYS_SET_STDIN_MODE: u64 = 22;
const SYS_SET_KEYBOARD_LAYOUT: u64 = 23;

// Global stdin mode: false=canonical (line-buffered), true=raw (byte-at-a-time)
static STDIN_RAW: SyncCell<bool> = SyncCell::new(false);

// Output capture buffer for SYS_EXEC (redirects SYS_WRITE to buffer instead of console)
static CAPTURE_BUF: SyncCell<Option<Vec<u8>>> = SyncCell::new(None);

// Kernel/user RSP storage for stack switching (single-core, no swapgs needed)
#[no_mangle]
pub static SYSCALL_KERNEL_RSP: SyncCell<u64> = SyncCell::new(0);
#[no_mangle]
static SYSCALL_USER_RSP: SyncCell<u64> = SyncCell::new(0);

pub fn init() {
    // Enable syscall/sysret in EFER (set SCE bit 0)
    let efer = cpu::rdmsr(MSR_EFER);
    cpu::wrmsr(MSR_EFER, efer | 1);

    // STAR: bits 47:32 = kernel CS (for syscall), bits 63:48 = user base (for sysret)
    // syscall:  CS = STAR[47:32] = 0x08, SS = STAR[47:32]+8 = 0x10
    // sysretq:  CS = STAR[63:48]+16 = 0x20 (RPL=3 → 0x23), SS = STAR[63:48]+8 = 0x18 (RPL=3 → 0x1B)
    let star = (0x10u64 << 48) | ((gdt::KERNEL_CS as u64) << 32);
    cpu::wrmsr(MSR_STAR, star);

    // LSTAR: syscall entry point
    cpu::wrmsr(MSR_LSTAR, syscall_entry as u64);

    // FMASK: mask IF (bit 9) on syscall entry to disable interrupts
    cpu::wrmsr(MSR_FMASK, 0x200);
}

// Low-level syscall entry point (called by `syscall` instruction from ring 3)
// Syscall ABI matches SysV with RCX skipped (hardware clobbers it):
//   RDI=num, RSI=a1, RDX=a2, R8=a3, R9=a4
//   RCX=return RIP (set by hardware), R11=return RFLAGS (set by hardware)
//   RSP=user stack (CPU does NOT switch stacks on syscall)
#[unsafe(naked)]
extern "C" fn syscall_entry() {
    // Save user RSP, switch to kernel stack, call handler, restore, sysretq.
    // Must preserve all registers except RAX (return value), RCX, R11 (clobbered
    // by syscall hardware). The SysV handler clobbers RDI/RSI/RDX/R8/R9/R10,
    // so we save and restore them here.
    naked_asm!(
        "mov [rip + SYSCALL_USER_RSP], rsp",
        "mov rsp, [rip + SYSCALL_KERNEL_RSP]",
        "push rcx",
        "push r11",
        "push rdi",
        "push rsi",
        "push rdx",
        "push r8",
        "push r9",
        "push r10",
        "call {handler}",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop r11",
        "pop rcx",
        "mov rsp, [rip + SYSCALL_USER_RSP]",
        "sysretq",
        handler = sym syscall_handler,
    );
}

// Whether a userspace process is active (checked by sys_exit)
pub static PROCESS_ACTIVE: SyncCell<bool> = SyncCell::new(false);

extern "C" fn syscall_handler(num: u64, a1: u64, a2: u64, _: u64, a3: u64, a4: u64) -> u64 {
    syscall_dispatch(num, a1, a2, a3, a4)
}

fn syscall_dispatch(num: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    match num {
        SYS_WRITE => sys_write(a1 as *const u8, a2 as usize),
        SYS_READ => sys_read(a1 as *mut u8, a2 as usize),
        SYS_ALLOC => user_heap::alloc(a1 as usize, a2 as usize),
        SYS_FREE => { user_heap::free(a1 as *mut u8, a2 as usize); 0 }
        SYS_REALLOC => user_heap::realloc(a1 as *mut u8, a2 as usize, a3 as usize, a4 as usize),
        SYS_EXIT => sys_exit(a1 as i32),
        SYS_RANDOM => { sys_random(a1 as *mut u8, a2 as usize); 0 }
        SYS_SCREEN_SIZE => {
            let (cols, rows) = console::screen_size();
            ((rows as u64) << 32) | (cols as u64)
        }
        SYS_CLOCK => crate::clock::nanos_since_boot(),
        SYS_OPEN => {
            let slice = unsafe { core::slice::from_raw_parts(a1 as *const u8, a2 as usize) };
            let Ok(path) = core::str::from_utf8(slice) else { return u64::MAX };
            crate::fd::open(vfs::global(), path, a3)
        }
        SYS_CLOSE => crate::fd::close(vfs::global(), a1),
        SYS_READ_FILE => {
            let buf = unsafe { core::slice::from_raw_parts_mut(a2 as *mut u8, a3 as usize) };
            crate::fd::read(a1, buf)
        }
        SYS_WRITE_FILE => {
            let buf = unsafe { core::slice::from_raw_parts(a2 as *const u8, a3 as usize) };
            crate::fd::write(a1, buf)
        }
        SYS_SEEK => crate::fd::seek(a1, a2 as i64, a3),
        SYS_FSTAT => crate::fd::fstat(a1),
        SYS_FSYNC => crate::fd::fsync(vfs::global(), a1),
        SYS_EXEC => sys_exec(a1, a2, a3, a4),
        SYS_READDIR => sys_readdir(a1, a2, a3, a4),
        SYS_DELETE => sys_delete(a1, a2),
        SYS_SHUTDOWN => { acpi::shutdown(); }
        SYS_CHDIR => sys_chdir(a1, a2),
        SYS_GETCWD => sys_getcwd(a1, a2),
        SYS_SET_STDIN_MODE => { *STDIN_RAW.get_mut() = a1 != 0; 0 }
        SYS_SET_KEYBOARD_LAYOUT => sys_set_keyboard_layout(a1, a2),
        _ => u64::MAX, // unknown syscall
    }
}

fn sys_write(buf: *const u8, len: usize) -> u64 {
    let bytes = unsafe { core::slice::from_raw_parts(buf, len) };
    // Send to serial with ANSI escape sequences stripped
    serial_write_plain(bytes);
    if let Some(ref mut capture) = *CAPTURE_BUF.get_mut() {
        capture.extend_from_slice(bytes);
    } else {
        console::write_bytes(bytes);
    }
    len as u64
}

/// Write bytes to serial, skipping ANSI escape sequences (ESC [ ... final_byte).
fn serial_write_plain(bytes: &[u8]) {
    let mut i = 0;
    let mut start = 0; // start of current plain-text run
    while i < bytes.len() {
        if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            // Flush plain text before the escape
            if start < i { serial::write_bytes(&bytes[start..i]); }
            // Skip ESC [ params until final byte (0x40..=0x7E)
            i += 2;
            while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() { i += 1; } // skip final byte
            start = i;
        } else {
            i += 1;
        }
    }
    if start < bytes.len() { serial::write_bytes(&bytes[start..]); }
}

fn sys_read(buf: *mut u8, len: usize) -> u64 {
    if *STDIN_RAW.get() {
        return sys_read_raw(buf, len);
    }
    // Line-buffered read with readline editing.
    // Supports left/right arrows, Home/End, Delete, insert mode.
    let mut line = [0u8; 1024];
    let mut line_len: usize = 0;
    let mut cursor: usize = 0;
    let max_len = len.min(line.len());

    console::show_cursor();

    loop {
        crate::drivers::xhci::poll_global();
        if let Some(ch) = keyboard::try_read_char() {
            match ch {
                b'\r' => {
                    console::hide_cursor();
                    // Move visual cursor to end, then newline
                    let mut echo = [0u8; 1025];
                    let mut n = 0;
                    for i in cursor..line_len {
                        echo[n] = line[i]; n += 1;
                    }
                    echo[n] = b'\n'; n += 1;
                    console::write_bytes(&echo[..n]);
                    // Copy to user buffer
                    let copy = line_len.min(len.saturating_sub(1));
                    unsafe {
                        core::ptr::copy_nonoverlapping(line.as_ptr(), buf, copy);
                        *buf.add(copy) = b'\n';
                    }
                    return (copy + 1) as u64;
                }
                0x08 | 0x7F => {
                    if cursor > 0 {
                        line.copy_within(cursor..line_len, cursor - 1);
                        line_len -= 1;
                        cursor -= 1;
                        readline_redraw(&line, line_len, cursor, true);
                    }
                }
                0x1B => readline_escape(&mut line, &mut line_len, &mut cursor),
                ch if ch >= 0x20 => {
                    if line_len < max_len {
                        line.copy_within(cursor..line_len, cursor + 1);
                        line[cursor] = ch;
                        line_len += 1;
                        cursor += 1;
                        readline_redraw(&line, line_len, cursor, false);
                    }
                }
                _ => {}
            }
        } else {
            core::hint::spin_loop();
        }
    }
}

/// Redraw the line from an edit point and reposition the console cursor.
/// `backspace`: true if a char was deleted (need to move left first and clear trailing).
fn readline_redraw(line: &[u8], line_len: usize, cursor: usize, backspace: bool) {
    let mut echo = [0u8; 2048];
    let mut n = 0;
    let start = if backspace {
        // Move console cursor left one position
        echo[n] = 0x08; n += 1;
        cursor
    } else {
        // After insert, cursor already advanced; redraw from insert point
        cursor - 1
    };
    for i in start..line_len {
        echo[n] = line[i]; n += 1;
    }
    if backspace {
        echo[n] = b' '; n += 1; // clear old trailing char
    }
    // Move console cursor back to cursor position
    let back = line_len - cursor + if backspace { 1 } else { 0 };
    for _ in 0..back {
        echo[n] = 0x08; n += 1;
    }
    console::write_bytes(&echo[..n]);
}

/// Handle escape sequences from the keyboard in sys_read.
fn readline_escape(line: &mut [u8], line_len: &mut usize, cursor: &mut usize) {
    let Some(b'[') = keyboard::try_read_char() else { return };
    match keyboard::try_read_char() {
        Some(b'A') | Some(b'B') => {} // up/down — ignore
        Some(b'C') => { // right
            if *cursor < *line_len {
                console::putchar(line[*cursor]);
                *cursor += 1;
            }
        }
        Some(b'D') => { // left
            if *cursor > 0 {
                *cursor -= 1;
                console::putchar(0x08);
            }
        }
        Some(b'H') => { // Home
            let mut echo = [0u8; 1024];
            let mut n = 0;
            for _ in 0..*cursor {
                echo[n] = 0x08; n += 1;
            }
            if n > 0 { console::write_bytes(&echo[..n]); }
            *cursor = 0;
        }
        Some(b'F') => { // End
            let mut echo = [0u8; 1024];
            let mut n = 0;
            for i in *cursor..*line_len {
                echo[n] = line[i]; n += 1;
            }
            if n > 0 { console::write_bytes(&echo[..n]); }
            *cursor = *line_len;
        }
        Some(b'3') => { // Delete: ESC[3~
            if keyboard::try_read_char() == Some(b'~') && *cursor < *line_len {
                line.copy_within(*cursor + 1..*line_len, *cursor);
                *line_len -= 1;
                // Redraw from cursor without moving left first
                let mut echo = [0u8; 2048];
                let mut n = 0;
                for i in *cursor..*line_len {
                    echo[n] = line[i]; n += 1;
                }
                echo[n] = b' '; n += 1;
                let back = *line_len - *cursor + 1;
                for _ in 0..back {
                    echo[n] = 0x08; n += 1;
                }
                console::write_bytes(&echo[..n]);
            }
        }
        Some(b'5') | Some(b'6') => { // Page Up/Down: ESC[5~ / ESC[6~
            keyboard::try_read_char(); // consume '~'
        }
        _ => {}
    }
}

/// Raw read: block until at least 1 byte, return all available bytes (no echo, no line editing).
fn sys_read_raw(buf: *mut u8, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    console::show_cursor();
    loop {
        crate::drivers::xhci::poll_global();
        if let Some(ch) = keyboard::try_read_char() {
            unsafe { *buf = ch; }
            let mut count = 1usize;
            while count < len {
                if let Some(ch) = keyboard::try_read_char() {
                    unsafe { *buf.add(count) = ch; }
                    count += 1;
                } else {
                    break;
                }
            }
            return count as u64;
        }
        core::hint::spin_loop();
    }
}

fn sys_exit(code: i32) -> u64 {
    let active = *PROCESS_ACTIVE.get();
    if !active {
        cpu::halt();
    }
    kill_process(code)
}

/// Terminate the current userspace process and return to execute()'s landing label.
/// Used by sys_exit and exception handlers.
pub fn kill_process(code: i32) -> ! {
    *PROCESS_ACTIVE.get_mut() = false;
    *STDIN_RAW.get_mut() = false;
    let krsp = *SYSCALL_KERNEL_RSP.get();
    unsafe {
        asm!(
            "mov rsp, {krsp}",
            "mov rax, {code}",
            "ret",
            krsp = in(reg) krsp,
            code = in(reg) code as u64,
            options(noreturn),
        );
    }
}

fn sys_random(buf: *mut u8, len: usize) {
    for i in 0..len {
        unsafe { *buf.add(i) = cpu::rdrand() as u8; }
    }
}

fn sys_exec(argv_ptr: u64, argv_len: u64, out_buf_ptr: u64, out_buf_max: u64) -> u64 {
    let buf = unsafe { core::slice::from_raw_parts(argv_ptr as *const u8, argv_len as usize) };
    let Ok(text) = core::str::from_utf8(buf) else { return u64::MAX };

    // Parse null-separated argv: "path\0arg1\0arg2"
    let args: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();
    let Some(path) = args.first() else { return u64::MAX };

    // Load binary from VFS
    let binary = match vfs::global().read_file(path) {
        Some(data) => data,
        None => return u64::MAX,
    };

    // Save parent process state
    let saved_heap = user_heap::save();
    let saved_user_rsp = *SYSCALL_USER_RSP.get();
    let saved_kernel_rsp = *SYSCALL_KERNEL_RSP.get();
    let saved_active = *PROCESS_ACTIVE.get();
    let saved_tss_rsp0 = unsafe { *gdt::tss_rsp0_ptr() };
    let saved_stdin_raw = *STDIN_RAW.get();

    // Enable output capture only when caller provides a buffer
    let capture = out_buf_max > 0;
    if capture {
        *CAPTURE_BUF.get_mut() = Some(Vec::new());
    }

    // Init fresh heap for child and run
    user_heap::init();
    let exit_code = crate::process::run(&binary, &args);

    // Close any file descriptors the child left open
    crate::fd::close_all(vfs::global());

    // Collect captured output
    let captured = core::mem::replace(CAPTURE_BUF.get_mut(), None).unwrap_or_default();

    // Restore parent process state
    *PROCESS_ACTIVE.get_mut() = saved_active;
    *SYSCALL_USER_RSP.get_mut() = saved_user_rsp;
    *SYSCALL_KERNEL_RSP.get_mut() = saved_kernel_rsp;
    unsafe { *gdt::tss_rsp0_ptr() = saved_tss_rsp0; }
    *STDIN_RAW.get_mut() = saved_stdin_raw;
    user_heap::restore(saved_heap);

    // Copy captured output to parent's buffer
    let copy_len = captured.len().min(out_buf_max as usize);
    if copy_len > 0 && out_buf_ptr != 0 {
        unsafe {
            core::ptr::copy_nonoverlapping(
                captured.as_ptr(),
                out_buf_ptr as *mut u8,
                copy_len,
            );
        }
    }

    ((exit_code as u64) << 32) | (copy_len as u64)
}

fn sys_readdir(path_ptr: u64, path_len: u64, buf_ptr: u64, buf_len: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(slice) else { return u64::MAX };

    let entries = match vfs::global().list(path) {
        Ok(e) => e,
        Err(_) => return u64::MAX,
    };

    // Serialize entries into buffer: type_u8 name_bytes \0 size_u64_le per entry
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_len as usize) };
    let mut pos = 0;
    for (name, size) in &entries {
        let is_dir = name.ends_with('/');
        let clean_name = if is_dir { &name[..name.len() - 1] } else { name.as_str() };
        let needed = 1 + clean_name.len() + 1 + 8; // type + name + \0 + size
        if pos + needed > buf.len() {
            break;
        }
        buf[pos] = if is_dir { 2 } else { 1 };
        pos += 1;
        buf[pos..pos + clean_name.len()].copy_from_slice(clean_name.as_bytes());
        pos += clean_name.len();
        buf[pos] = 0;
        pos += 1;
        buf[pos..pos + 8].copy_from_slice(&size.to_le_bytes());
        pos += 8;
    }
    pos as u64
}

fn sys_delete(path_ptr: u64, path_len: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(slice) else { return u64::MAX };
    if vfs::global().delete(path) { 0 } else { u64::MAX }
}

fn sys_chdir(path_ptr: u64, path_len: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(slice) else { return u64::MAX };
    if vfs::global().cd(path) { 0 } else { u64::MAX }
}

fn sys_getcwd(buf_ptr: u64, buf_len: u64) -> u64 {
    let cwd = vfs::global().cwd();
    let len = cwd.len().min(buf_len as usize);
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, len) };
    buf.copy_from_slice(&cwd.as_bytes()[..len]);
    len as u64
}

fn sys_set_keyboard_layout(name_ptr: u64, name_len: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, name_len as usize) };
    let Ok(name) = core::str::from_utf8(slice) else { return u64::MAX };
    if keyboard::set_layout(name) {
        // Persist the choice to config
        vfs::global().write_file("/nvme/config/keyboard_layout", name.as_bytes());
        0
    } else {
        u64::MAX
    }
}

// Miscellaneous POSIX/C functions: environment, process, signals, sysconf.

use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use toyos_abi::syscall;

// ---------------------------------------------------------------------------
// errno (shared with other modules)
// ---------------------------------------------------------------------------

const ENOSYS: i32 = 38;
const ECHILD: i32 = 10;

// ---------------------------------------------------------------------------
// Environment variables
// ---------------------------------------------------------------------------

// Cached environment block from kernel. Format: "KEY=VALUE\0KEY=VALUE\0\0"
static mut ENV_BUF: [u8; 8192] = [0; 8192];
static mut ENV_INITED: bool = false;

unsafe fn ensure_env() {
    let inited = ptr::addr_of!(ENV_INITED).read_volatile();
    if !inited {
        let buf = ptr::addr_of_mut!(ENV_BUF).as_mut().unwrap();
        let n = syscall::get_env(buf);
        if n < buf.len() {
            buf[n] = 0;
        }
        ptr::addr_of_mut!(ENV_INITED).write_volatile(true);
    }
}

#[no_mangle]
pub unsafe extern "C" fn getenv(name: *const u8) -> *mut u8 {
    if name.is_null() { return ptr::null_mut(); }
    ensure_env();

    let name_len = super::string::strlen(name);
    let name_slice = core::slice::from_raw_parts(name, name_len);

    let buf = &*ptr::addr_of!(ENV_BUF);
    let buf_mut = ptr::addr_of_mut!(ENV_BUF).cast::<u8>();
    let mut i = 0;
    while i < buf.len() && buf[i] != 0 {
        // Find end of this entry
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        let entry = &buf[start..i];
        // Check if entry starts with name=
        if entry.len() > name_len && entry[name_len] == b'='
            && entry[..name_len] == *name_slice
        {
            return buf_mut.add(start + name_len + 1);
        }
        i += 1; // skip null terminator
    }
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn setenv(name: *const u8, value: *const u8, _overwrite: i32) -> i32 {
    // Not implemented — env is read-only from kernel
    let _ = (name, value);
    0
}

#[no_mangle]
pub unsafe extern "C" fn unsetenv(_name: *const u8) -> i32 {
    0
}

// ---------------------------------------------------------------------------
// Process
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn getpid() -> i32 {
    syscall::getpid().0 as i32
}

#[no_mangle]
pub unsafe extern "C" fn getppid() -> i32 {
    0 // not tracked
}

#[no_mangle]
pub unsafe extern "C" fn getuid() -> u32 { 0 }

#[no_mangle]
pub unsafe extern "C" fn geteuid() -> u32 { 0 }

#[no_mangle]
pub unsafe extern "C" fn getgid() -> u32 { 0 }

#[no_mangle]
pub unsafe extern "C" fn getegid() -> u32 { 0 }

#[no_mangle]
pub unsafe extern "C" fn fork() -> i32 {
    super::stdio::errno = ENOSYS;
    -1
}

#[no_mangle]
pub unsafe extern "C" fn execvp(_file: *const u8, _argv: *const *const u8) -> i32 {
    super::stdio::errno = ENOSYS;
    -1
}

#[no_mangle]
pub unsafe extern "C" fn waitpid(pid: i32, status: *mut i32, _options: i32) -> i32 {
    if pid <= 0 {
        super::stdio::errno = ECHILD;
        return -1;
    }
    let code = syscall::waitpid(syscall::Pid(pid as u32));
    if !status.is_null() {
        // Encode exit code in wait status format (exit_code << 8)
        *status = (code as i32) << 8;
    }
    pid
}

// ---------------------------------------------------------------------------
// Exit / abort / atexit
// ---------------------------------------------------------------------------

const MAX_ATEXIT: usize = 32;
static mut ATEXIT_FNS: [Option<unsafe extern "C" fn()>; MAX_ATEXIT] = [None; MAX_ATEXIT];
static mut ATEXIT_COUNT: usize = 0;

#[no_mangle]
pub unsafe extern "C" fn atexit(func: unsafe extern "C" fn()) -> i32 {
    let count = ptr::addr_of!(ATEXIT_COUNT).read();
    if count >= MAX_ATEXIT {
        return -1;
    }
    let fns = ptr::addr_of_mut!(ATEXIT_FNS).as_mut().unwrap();
    fns[count] = Some(func);
    ptr::addr_of_mut!(ATEXIT_COUNT).write(count + 1);
    0
}

unsafe fn run_atexit() {
    // Run in reverse order
    let fns = ptr::addr_of_mut!(ATEXIT_FNS).as_mut().unwrap();
    let mut count = ptr::addr_of!(ATEXIT_COUNT).read();
    while count > 0 {
        count -= 1;
        if let Some(f) = fns[count] {
            f();
        }
    }
    ptr::addr_of_mut!(ATEXIT_COUNT).write(0);
}

#[no_mangle]
pub unsafe extern "C" fn exit(status: i32) -> ! {
    run_atexit();
    super::stdio::fflush(ptr::null_mut());
    syscall::exit(status)
}

#[no_mangle]
pub unsafe extern "C" fn _exit(status: i32) -> ! {
    syscall::exit(status)
}

#[no_mangle]
pub unsafe extern "C" fn _Exit(status: i32) -> ! {
    syscall::exit(status)
}

#[no_mangle]
pub unsafe extern "C" fn abort() -> ! {
    syscall::exit(134) // SIGABRT
}

// ---------------------------------------------------------------------------
// Signal (stubs — ToyOS has no signals)
// ---------------------------------------------------------------------------

type SigHandlerT = unsafe extern "C" fn(i32);

#[no_mangle]
pub unsafe extern "C" fn signal(_signum: i32, handler: SigHandlerT) -> SigHandlerT {
    handler // return the handler as "previous", effectively a no-op
}

#[no_mangle]
pub unsafe extern "C" fn sigaction(
    _signum: i32, _act: *const u8, _oldact: *mut u8,
) -> i32 {
    0 // success
}

#[no_mangle]
pub unsafe extern "C" fn sigprocmask(
    _how: i32, _set: *const u64, _oldset: *mut u64,
) -> i32 {
    0
}

#[no_mangle]
pub unsafe extern "C" fn raise(_sig: i32) -> i32 {
    0
}

#[no_mangle]
pub unsafe extern "C" fn kill(_pid: i32, _sig: i32) -> i32 {
    0
}

// ---------------------------------------------------------------------------
// sysconf
// ---------------------------------------------------------------------------

const _SC_PAGESIZE: i32 = 30;
const _SC_NPROCESSORS_ONLN: i32 = 84;
const _SC_CLK_TCK: i32 = 2;

#[no_mangle]
pub unsafe extern "C" fn sysconf(name: i32) -> i64 {
    match name {
        _SC_PAGESIZE => 4096,
        _SC_NPROCESSORS_ONLN => syscall::cpu_count() as i64,
        _SC_CLK_TCK => 100,
        _ => -1,
    }
}

// ---------------------------------------------------------------------------
// Random
// ---------------------------------------------------------------------------

static RAND_STATE: AtomicU32 = AtomicU32::new(1);

#[no_mangle]
pub unsafe extern "C" fn srand(seed: u32) {
    RAND_STATE.store(seed, Ordering::Relaxed);
}

#[no_mangle]
pub unsafe extern "C" fn rand() -> i32 {
    // LCG — same as glibc
    let mut s = RAND_STATE.load(Ordering::Relaxed);
    s = s.wrapping_mul(1103515245).wrapping_add(12345);
    RAND_STATE.store(s, Ordering::Relaxed);
    ((s >> 16) & 0x7fff) as i32
}

// ---------------------------------------------------------------------------
// Sorting / searching
// ---------------------------------------------------------------------------

type CmpFn = unsafe extern "C" fn(*const u8, *const u8) -> i32;

#[no_mangle]
pub unsafe extern "C" fn qsort(
    base: *mut u8, nmemb: usize, size: usize, compar: CmpFn,
) {
    if nmemb <= 1 || size == 0 { return; }
    // Simple insertion sort — good enough for small arrays, correct for all
    let mut tmp = alloc::vec![0u8; size];
    for i in 1..nmemb {
        let mut j = i;
        while j > 0 {
            let a = base.add(j * size);
            let b = base.add((j - 1) * size);
            if compar(a, b) < 0 {
                ptr::copy_nonoverlapping(a, tmp.as_mut_ptr(), size);
                ptr::copy(b, a, size);
                ptr::copy_nonoverlapping(tmp.as_ptr(), b, size);
                j -= 1;
            } else {
                break;
            }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn bsearch(
    key: *const u8, base: *const u8, nmemb: usize, size: usize, compar: CmpFn,
) -> *mut u8 {
    let mut lo = 0usize;
    let mut hi = nmemb;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let elem = base.add(mid * size);
        let cmp = compar(key, elem);
        if cmp < 0 {
            hi = mid;
        } else if cmp > 0 {
            lo = mid + 1;
        } else {
            return elem as *mut u8;
        }
    }
    ptr::null_mut()
}

// ---------------------------------------------------------------------------
// String-to-number conversions
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn atoi(s: *const u8) -> i32 {
    strtol(s, ptr::null_mut(), 10) as i32
}

#[no_mangle]
pub unsafe extern "C" fn atol(s: *const u8) -> i64 {
    strtol(s, ptr::null_mut(), 10)
}

#[no_mangle]
pub unsafe extern "C" fn strtol(s: *const u8, endptr: *mut *mut u8, base: i32) -> i64 {
    if s.is_null() { return 0; }
    let mut p = s;
    // Skip whitespace
    while *p == b' ' || *p == b'\t' || *p == b'\n' || *p == b'\r' { p = p.add(1); }
    // Sign
    let neg = *p == b'-';
    if *p == b'-' || *p == b'+' { p = p.add(1); }
    // Detect base
    let mut base = base as u32;
    if base == 0 {
        if *p == b'0' {
            p = p.add(1);
            if *p == b'x' || *p == b'X' {
                base = 16;
                p = p.add(1);
            } else {
                base = 8;
            }
        } else {
            base = 10;
        }
    } else if base == 16 && *p == b'0' && (*p.add(1) == b'x' || *p.add(1) == b'X') {
        p = p.add(2);
    }
    let mut val: i64 = 0;
    loop {
        let c = *p;
        let digit = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'z' => c - b'a' + 10,
            b'A'..=b'Z' => c - b'A' + 10,
            _ => break,
        };
        if digit as u32 >= base { break; }
        val = val.wrapping_mul(base as i64).wrapping_add(digit as i64);
        p = p.add(1);
    }
    if !endptr.is_null() { *endptr = p as *mut u8; }
    if neg { -val } else { val }
}

#[no_mangle]
pub unsafe extern "C" fn strtoul(s: *const u8, endptr: *mut *mut u8, base: i32) -> u64 {
    strtol(s, endptr, base) as u64
}

#[no_mangle]
pub unsafe extern "C" fn strtoll(s: *const u8, endptr: *mut *mut u8, base: i32) -> i64 {
    strtol(s, endptr, base)
}

#[no_mangle]
pub unsafe extern "C" fn strtoull(s: *const u8, endptr: *mut *mut u8, base: i32) -> u64 {
    strtol(s, endptr, base) as u64
}

#[no_mangle]
pub unsafe extern "C" fn strtod(s: *const u8, endptr: *mut *mut u8) -> f64 {
    if s.is_null() { return 0.0; }
    let mut p = s;
    while *p == b' ' || *p == b'\t' || *p == b'\n' || *p == b'\r' { p = p.add(1); }
    let neg = *p == b'-';
    if *p == b'-' || *p == b'+' { p = p.add(1); }

    let mut val: f64 = 0.0;
    while *p >= b'0' && *p <= b'9' {
        val = val * 10.0 + (*p - b'0') as f64;
        p = p.add(1);
    }
    if *p == b'.' {
        p = p.add(1);
        let mut frac = 0.1;
        while *p >= b'0' && *p <= b'9' {
            val += (*p - b'0') as f64 * frac;
            frac *= 0.1;
            p = p.add(1);
        }
    }
    if *p == b'e' || *p == b'E' {
        p = p.add(1);
        let exp_neg = *p == b'-';
        if *p == b'-' || *p == b'+' { p = p.add(1); }
        let mut exp: i32 = 0;
        while *p >= b'0' && *p <= b'9' {
            exp = exp * 10 + (*p - b'0') as i32;
            p = p.add(1);
        }
        if exp_neg { exp = -exp; }
        val *= super::math::pow(10.0, exp as f64);
    }

    if !endptr.is_null() { *endptr = p as *mut u8; }
    if neg { -val } else { val }
}

// ---------------------------------------------------------------------------
// abs
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn abs(j: i32) -> i32 {
    if j < 0 { -j } else { j }
}

#[no_mangle]
pub unsafe extern "C" fn labs(j: i64) -> i64 {
    if j < 0 { -j } else { j }
}

// ---------------------------------------------------------------------------
// setjmp/longjmp (minimal stub — used by some C code)
// ---------------------------------------------------------------------------

// jmp_buf is 8 * 8 bytes on x86_64 (enough for callee-saved regs + rsp + rip)
// Real implementation would need assembly; this is a panic stub.
#[no_mangle]
pub unsafe extern "C" fn setjmp(_env: *mut u8) -> i32 {
    0
}

#[no_mangle]
pub unsafe extern "C" fn longjmp(_env: *mut u8, _val: i32) -> ! {
    panic!("longjmp not implemented")
}

// ---------------------------------------------------------------------------
// dlopen/dlsym/dlclose
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn dlopen(path: *const u8, _flags: i32) -> *mut u8 {
    if path.is_null() { return ptr::null_mut(); }
    let path_bytes = super::posix_io::c_str_to_bytes(path);
    match syscall::dl_open(path_bytes) {
        Ok(handle) => handle as *mut u8,
        Err(_) => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn dlsym(handle: *mut u8, symbol: *const u8) -> *mut u8 {
    if symbol.is_null() { return ptr::null_mut(); }
    let name = super::posix_io::c_str_to_bytes(symbol);
    // SAFETY: handle is from a prior dlopen, name is a valid C string
    match unsafe { syscall::dl_sym(handle as u64, name) } {
        Ok(addr) => addr as *mut u8,
        Err(_) => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn dlclose(handle: *mut u8) -> i32 {
    syscall::dl_close(handle as u64) as i32
}

#[no_mangle]
pub unsafe extern "C" fn dlerror() -> *const u8 {
    ptr::null()
}

//! `SYS_QUERY_MODULES` has to be able to say how big an answer is.
//!
//! Its ABI doc claimed a too-small buffer came back as
//! `Err(InvalidArgument)` "with the required buffer size encoded". Nothing
//! encoded anything: `SyscallError` is a fixed set of codes, the handler
//! returned a bare one, and a caller had no way to size a retry and no way to
//! learn that was why it failed. A doc comment is a claim to verify.
//!
//! The contract now is `sys_getcwd`'s and `sys_readdir`'s: the return is the
//! length in bytes either way, and nothing is written unless all of it fits.

use core::mem::size_of;

use toyos_abi::syscall::{self, ModuleInfo};

/// Not zero, so a handler that wrote a prefix would be caught doing it.
const POISON: u8 = 0xCD;

fn main() {
    let mut empty: [u8; 0] = [];
    let need = syscall::query_modules(&mut empty).expect("an empty buffer is a size query");
    assert!(
        need >= size_of::<ModuleInfo>(),
        "the required size is {need} bytes, which is less than one record"
    );
    println!("  PASS: an empty buffer reports {need} bytes required");

    // One byte short. The answer must still be the size, and the buffer must
    // come back exactly as it went in — a partial description is worse than
    // none, because every offset in it points into a record that is not there.
    let mut short = vec![POISON; need - 1];
    let again = syscall::query_modules(&mut short).expect("a short buffer is a size query too");
    assert_eq!(again, need, "a short buffer reported {again}, not the required {need}");
    assert!(
        short.iter().all(|&b| b == POISON),
        "a buffer one byte too small was written to"
    );
    println!("  PASS: one byte short reports {need} and writes nothing");

    let mut buf = vec![POISON; need];
    let got = syscall::query_modules(&mut buf).expect("a buffer of the reported size");
    assert_eq!(got, need, "the exact buffer reported {got}, not {need}");

    // The records end where the first path begins, which is what makes the
    // buffer self-describing without a count in the return value.
    let exe = unsafe { (buf.as_ptr() as *const ModuleInfo).read_unaligned() };
    let records = exe.path_offset as usize;
    assert_eq!(
        records % size_of::<ModuleInfo>(),
        0,
        "the record array is {records} bytes, not a whole number of records"
    );
    let count = records / size_of::<ModuleInfo>();
    assert!(count >= 1, "no modules at all");
    assert!(exe.base > 0, "the executable has no load base");

    let start = exe.path_offset as usize;
    let path = core::str::from_utf8(&buf[start..start + exe.path_len as usize])
        .expect("the executable's path is not UTF-8");
    assert!(
        path.contains("query_modules_size"),
        "the first module is {path:?}, which is not this program"
    );
    println!("  PASS: {count} module(s), this one at {path}");

    println!("all query_modules sizing tests passed");
}

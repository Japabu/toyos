//! Releasing the last reference to a shared region must give the pages back.
//!
//! `SYS_RELEASE_SHARED` unmapped the caller and dropped it from the region's
//! `allowed` list, and stopped there. The "is anyone left?" test lived only in
//! `cleanup_process`, so nothing freed a region at close time — soundd's
//! per-client audio ring stayed resident until some process happened to exit
//! and the sweep ran over it.
//!
//! Two directions, because a reclaim rule that is too eager is worse than one
//! that never fires: a region whose owner has released but which is still
//! granted to someone must survive, and that someone must still be able to map
//! it. Run as `... donor` this binary is that owner.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use toyos_abi::syscall;
use toyos_abi::Pid;

const SELF_PATH: &str = "/bin/test_rs_shm_release_reclaims";
const PAYLOAD: &[u8] = b"granted-before-the-owner-let-go";
/// Each region rounds up to one 2 MiB page, so the loop moves 32 MiB — far
/// enough above the megabyte or so the rest of the boot moves under it that a
/// leak cannot hide in the noise.
const ROUNDS: usize = 16;
const REGION: usize = 4096;

fn free_bytes() -> u64 {
    let mut buf = [0u8; 48];
    let n = syscall::sysinfo(&mut buf);
    assert!(n >= 48, "sysinfo returned {n} bytes");
    let total = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let used = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    total - used
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("donor") {
        return donor(std::env::args().nth(2).expect("donor needs the target pid").parse().unwrap());
    }

    let start = free_bytes();

    let mut tokens = Vec::new();
    for _ in 0..ROUNDS {
        let token = syscall::alloc_shared(REGION);
        let ptr = unsafe { syscall::try_map_shared(token) }.expect("map own region");
        unsafe { ptr.write_volatile(0xA5) };
        tokens.push(token);
    }
    let held = free_bytes();

    // Non-vacuity: if the instrument could not see 32 MiB leave, it cannot see
    // it come back either, and the reclaim assertion below would pass on a
    // kernel that frees nothing.
    let taken = start.saturating_sub(held);
    assert!(
        taken >= 24 * 1024 * 1024,
        "{ROUNDS} regions were allocated but free memory only moved {taken} bytes — \
         the measurement is not seeing the allocation"
    );

    for token in tokens {
        syscall::release_shared(token);
    }
    let after = free_bytes();

    let leaked = start.saturating_sub(after);
    assert!(
        leaked < 8 * 1024 * 1024,
        "{ROUNDS} regions ({taken} bytes) were allocated, mapped and released, \
         and {leaked} bytes never came back"
    );

    // The other direction. The donor allocates, grants to us, then releases
    // its own reference — nobody has the region mapped at that moment, but we
    // are still entitled to it.
    let me = syscall::getpid();
    let mut donor = Command::new(SELF_PATH)
        .arg("donor")
        .arg(me.0.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn donor");
    let mut out = BufReader::new(donor.stdout.take().expect("donor stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("donor token line");
    let token: u32 = line.trim().parse().expect("donor token");

    let ptr = unsafe { syscall::try_map_shared(token) }
        .expect("a region still granted to us was reclaimed out from under the grant");
    let seen = unsafe { core::slice::from_raw_parts(ptr, PAYLOAD.len()) };
    assert_eq!(seen, PAYLOAD, "the granted region no longer holds the donor's payload");
    syscall::release_shared(token);

    let mut donor_in = donor.stdin.take().expect("donor stdin");
    writeln!(donor_in, "quit").expect("tell the donor to quit");
    drop(donor_in);
    assert!(donor.wait().expect("wait donor").success(), "donor exited nonzero");

    println!("released regions reclaimed ({taken} bytes out, {leaked} bytes unreturned); granted region survived its owner's release");
}

fn donor(target: u32) {
    let token = syscall::alloc_shared(REGION);
    let ptr = unsafe { syscall::try_map_shared(token) }.expect("donor: map own region");
    unsafe { core::ptr::copy_nonoverlapping(PAYLOAD.as_ptr(), ptr, PAYLOAD.len()) };
    syscall::grant_shared(token, Pid(target)).expect("donor: grant");

    // Owner lets go while the grantee has not mapped yet: `mapped_in` is empty
    // and `allowed` is not, which is exactly the state a too-eager reclaim
    // would free.
    syscall::release_shared(token);

    println!("{token}");
    std::io::stdout().flush().expect("donor: flush token");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

//! Dropping the last handle to a shared region must give the pages back.
//!
//! `SYS_RELEASE_SHARED` unmapped the caller and dropped it from the region's
//! `allowed` list, and stopped there: the "is anyone left?" test lived only in
//! `cleanup_process`, so nothing freed a region at close time and soundd's
//! per-client ring stayed resident until some unrelated process exited. There
//! is no release call now — a region's life is its handle count, and the
//! zero-handle hook is where the mappings go.
//!
//! Two directions, because a reclaim rule that is too eager is worse than one
//! that never fires: a region whose maker has dropped its handle but which it
//! **sent** to somebody must survive, and that somebody must still be able to
//! read it. Run as `... donor` this binary is that maker.

use std::io::{BufRead, BufReader, Write};
use std::os::toyos::process::CommandExt;
use std::process::{Command, Stdio};

use toyos::shm::SharedMemory;
use toyos::{namespace, port, AsHandle};
use toyos_abi::syscall::{self, SVC_LABEL};

const SELF_PATH: &str = "/bin/test_rs_shm_release_reclaims";
const PAYLOAD: &[u8] = b"sent-before-the-maker-let-go";
/// Each region rounds up to one 2 MiB page, so the loop moves 32 MiB — far
/// enough above the megabyte or so the rest of the boot moves under it that a
/// leak cannot hide in the noise.
const ROUNDS: usize = 16;
const REGION: usize = 4096;
const SERVICE: &str = "region";

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
        return donor();
    }

    let start = free_bytes();

    let mut regions = Vec::new();
    for _ in 0..ROUNDS {
        let mut region = SharedMemory::create(REGION).expect("a region of our own");
        region.as_mut_slice()[0] = 0xA5;
        regions.push(region);
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

    drop(regions);
    let after = free_bytes();

    let leaked = start.saturating_sub(after);
    assert!(
        leaked < 8 * 1024 * 1024,
        "{ROUNDS} regions ({taken} bytes) were allocated, mapped and dropped, \
         and {leaked} bytes never came back"
    );

    // The other direction. The donor makes a region, sends it here, and drops
    // its own handle before this process has mapped anything — nobody has the
    // region mapped at that moment, and it is still this process's.
    let (acceptor, connector) = port::create().expect("the kernel refused a port");
    let ns = namespace::build()
        .add(SERVICE, &connector)
        .finish()
        .expect("the kernel refused a namespace");
    let mut donor = Command::new(SELF_PATH)
        .arg("donor")
        .endow(SVC_LABEL, ns.into_raw().0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn donor");

    let conn = acceptor.accept().expect("the donor connects");
    conn.recv_header().expect("the donor announces its region");
    let [sent] = conn
        .recv_handles_exact::<1>()
        .expect("the donor sent the region ahead of the frame");

    // Only now, after the donor has let go — its line says so.
    let mut out = BufReader::new(donor.stdout.take().expect("donor stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("donor release line");
    assert_eq!(line.trim(), "released", "the donor did not report letting go");

    let region = SharedMemory::adopt(sent, REGION)
        .expect("a region this process holds a handle to was reclaimed under it");
    assert_eq!(
        &region.as_slice()[..PAYLOAD.len()],
        PAYLOAD,
        "the sent region no longer holds the donor's payload"
    );
    drop(region);

    let mut donor_in = donor.stdin.take().expect("donor stdin");
    writeln!(donor_in, "quit").expect("tell the donor to quit");
    drop(donor_in);
    assert!(donor.wait().expect("wait donor").success(), "donor exited nonzero");

    println!(
        "dropped regions reclaimed ({taken} bytes out, {leaked} bytes unreturned); \
         a sent region survived its maker letting go"
    );
}

fn donor() {
    let conn = toyos::endow::service(SERVICE).expect("donor: the port it was given");
    let mut region = SharedMemory::create(REGION).expect("donor: a region of its own");
    region.as_mut_slice()[..PAYLOAD.len()].copy_from_slice(PAYLOAD);

    let shared = region.share().expect("donor: a second handle");
    syscall::handle_send(conn.as_handle(), &[shared]).expect("donor: send the region");
    conn.signal(1).expect("donor: announce it");

    // The maker lets go while the peer has not mapped yet: no process has this
    // region mapped, and the in-flight handle is the only thing keeping it —
    // exactly the state a too-eager reclaim would free.
    drop(region);
    println!("released");
    std::io::stdout().flush().expect("donor: flush");

    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

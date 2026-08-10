//! netd's piped-connection cap, from the client side.
//!
//! Needs netd with a NIC in front of it, which only `tests/netcase` provides —
//! it is in `RUST_SKIP` and `netd_connection_caps` runs it there.
//!
//! Every request goes to a destination that cannot answer, so each one netd
//! accepts stays in `pending_piped_connects` for the whole burst. That is
//! deliberate: the cap counts pending connects alongside established ones, so
//! a burst of connects is the shortest path to the boundary, and it needs no
//! peer at all. The requests are issued without reading any reply — `NetdConn`
//! splits send from receive — because reading would block on a connect that is
//! working exactly as intended.
//!
//! The argument is the burst size, not the answer. The host passes the cap
//! netd announced so this does not have to guess how many requests are enough;
//! where the boundary actually falls is measured here and compared there.

use toyos::net::{
    MsgType, NetError, NetdConn, PendingResponse, TcpConnectPipedRequest, TcpConnectResponse,
    DATA_FROM_CLIENT, DATA_HANDLES, DATA_TO_CLIENT,
};
use toyos_abi::syscall;
use toyos_abi::RawHandle;

/// TEST-NET-1 (RFC 5737), reserved for documentation and guaranteed not to be
/// a real host. What matters is only that it does not complete a handshake.
const BLACK_HOLE: [u8; 4] = [192, 0, 2, 1];
const PORT: u16 = 80;

/// Long enough that netd is still holding every accepted connect when it
/// reaches the end of the burst — if the first ones expired on the way, the
/// count would never reach the cap and no refusal would ever be sent.
const TIMEOUT_MS: u32 = 4000;

/// How far past the announced cap to keep asking. Small: the point is to cross
/// the boundary, and every request costs netd an IPC connection.
const MARGIN: usize = 4;

/// How many times one connect may be refused by the *kernel* before this gives
/// up on it.
///
/// **A count of attempts, not a span of time**, and it is measuring something
/// other than netd's cap: the port's queue holds `MAX_PENDING_CONNECTIONS`
/// connections netd has not accepted yet, and a burst issued as fast as a
/// single-threaded client can issue it outruns one accept per event-loop pass.
/// That is backpressure from the kernel and is retryable against the same peer;
/// netd's own cap is the `ResourceExhausted` in a *response*, which is what
/// this file is about and what it must not be confused with.
///
/// The retry used to be invisible: `NetdConn::connect_blocking` spun a hundred
/// times at ten milliseconds over any error at all, so this test never saw the
/// queue at the same time as it never saw a boot race. That loop is deleted
/// because a boot race is unrepresentable now; this is the part of it that was
/// doing a different job.
const CONNECT_ATTEMPTS: usize = 200;

/// One netd event-loop pass, which is what an attempt is waiting for.
///
/// netd polls at 1 ms while it holds piped connections and accepts one
/// connection per pass, so this paces the client to the rate the queue drains
/// at. **The bound is [`CONNECT_ATTEMPTS`], not this** — a slow host makes the
/// attempts cheaper, never fewer.
const PASS_NANOS: u64 = 1_000_000;

fn main() {
    let announced: usize = std::env::args()
        .nth(1)
        .expect("netd_caps needs the announced cap as its argument")
        .parse()
        .expect("the announced cap must be a number");
    let burst = announced + MARGIN;

    let request = TcpConnectPipedRequest {
        addr: BLACK_HOLE,
        port: PORT,
        _pad: 0,
        timeout_ms: TIMEOUT_MS,
    };

    let mut sent: Vec<PendingResponse> = Vec::with_capacity(burst);
    for i in 0..burst {
        let conn = connect_past_the_queue(i);
        sent.push(
            conn.request_with_handles(&data_path(), MsgType::TcpConnectPiped, &request)
                .unwrap_or_else(|e| panic!("request {i}: netd would not take it: {e:?}")),
        );
    }

    let outcomes: Vec<Option<NetError>> = sent
        .into_iter()
        .map(|p| p.response::<TcpConnectResponse>().err())
        .collect();

    let Some(granted) = outcomes
        .iter()
        .position(|o| *o == Some(NetError::ResourceExhausted))
    else {
        panic!(
            "{burst} connects and netd never reported ResourceExhausted; outcomes: {}",
            summarise(&outcomes)
        );
    };

    // Both sides of the boundary, because "a refusal happened" is also true of
    // a netd that refused everything, and of one that refused at random.
    for (i, outcome) in outcomes.iter().enumerate() {
        if i < granted {
            assert!(
                outcome.is_some(),
                "connect {i} to a black hole succeeded — the burst never filled netd"
            );
            assert_ne!(
                *outcome,
                Some(NetError::ResourceExhausted),
                "connect {i} was refused before the boundary at {granted}"
            );
        } else {
            assert_eq!(
                *outcome,
                Some(NetError::ResourceExhausted),
                "connect {i} past the boundary at {granted} was not a capacity refusal"
            );
        }
    }
    assert!(
        granted >= 2,
        "only {granted} connects were accepted; netd is refusing, not bounding"
    );

    println!(
        "netd caps: {granted} connections accepted then refused (accepted ones ended as {})",
        summarise(&outcomes[..granted])
    );
}

/// The two ends one request hands netd.
///
/// **A fresh pair per request, where one pair used to serve the whole burst.**
/// The ends travel with the request now rather than being named by an id netd
/// would reopen later, and a handle that has been moved cannot be moved again.
/// This side's own two ends are dropped where they are made: nothing here will
/// read or write them, and each pipe stays alive on the end netd holds — which
/// is exactly where the cap is counting it.
fn data_path() -> [RawHandle; DATA_HANDLES] {
    let (to_client_read, to_client_write) = toyos::pipe_pair().expect("the pipe netd writes into");
    let (from_client_read, from_client_write) =
        toyos::pipe_pair().expect("the pipe netd reads from");
    let mut handles = [toyos_abi::HANDLE_INVALID; DATA_HANDLES];
    handles[DATA_TO_CLIENT] = to_client_write.into_fd();
    handles[DATA_FROM_CLIENT] = from_client_read.into_fd();
    drop(to_client_read);
    drop(from_client_write);
    handles
}

/// The distinct outcomes, in first-seen order. Printed so the log says what
/// the accepted connects actually died of rather than only how many there were.
fn summarise(outcomes: &[Option<NetError>]) -> String {
    let mut kinds: Vec<String> = Vec::new();
    for outcome in outcomes {
        let kind = format!("{outcome:?}");
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    kinds.join(", ")
}

/// One connection, retrying only the kernel's queue-full answer.
///
/// Every other refusal ends the test where it happens: "netd is not reachable"
/// and "netd has not drained its accept queue yet" are different facts and only
/// the second one is worth another attempt.
fn connect_past_the_queue(request: usize) -> NetdConn {
    for attempt in 0..CONNECT_ATTEMPTS {
        match NetdConn::connect() {
            Ok(conn) => return conn,
            Err(NetError::ResourceExhausted) => {
                let _ = attempt;
                syscall::nanosleep(PASS_NANOS);
            }
            Err(e) => panic!("request {request}: could not reach netd: {e:?}"),
        }
    }
    panic!(
        "request {request}: the port queue stayed full for {CONNECT_ATTEMPTS} attempts — \
         netd is not accepting"
    )
}

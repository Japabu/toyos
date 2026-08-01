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
};
use toyos_abi::syscall;

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

fn main() {
    let announced: usize = std::env::args()
        .nth(1)
        .expect("netd_caps needs the announced cap as its argument")
        .parse()
        .expect("the announced cap must be a number");
    let burst = announced + MARGIN;

    // One pair of pipes for every request. netd stores the ids with the
    // pending connect and only opens them if the connect completes, which none
    // of these will — so sharing them costs nothing and saves the client 4 MiB
    // and four fds per request.
    let rx = syscall::pipe();
    let tx = syscall::pipe();
    let request = TcpConnectPipedRequest {
        addr: BLACK_HOLE,
        port: PORT,
        _pad: 0,
        timeout_ms: TIMEOUT_MS,
        _pad2: 0,
        rx_pipe_id: syscall::pipe_id(rx.write).expect("rx pipe id"),
        tx_pipe_id: syscall::pipe_id(tx.read).expect("tx pipe id"),
    };

    let mut sent: Vec<PendingResponse> = Vec::with_capacity(burst);
    for i in 0..burst {
        let conn = NetdConn::connect_blocking()
            .unwrap_or_else(|e| panic!("request {i}: could not reach netd: {e:?}"));
        sent.push(
            conn.request(MsgType::TcpConnectPiped, &request)
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

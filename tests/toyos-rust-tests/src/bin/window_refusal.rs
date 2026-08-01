//! A client must survive a compositor that says no.
//!
//! The window protocol had no refusal message at all: the only answer to
//! `MSG_CREATE_WINDOW` was `MSG_WINDOW_CREATED`, so a compositor that could not
//! afford another window had no move except to serve it or drop the connection,
//! and `Window::create` met anything else with `assert_eq!` — a client killed
//! by an answer it should have been able to read.
//!
//! This binary plays the compositor. Nothing runs on that service name in the
//! test boot, so `listen("compositor")` claims it and the four answers a client
//! can get are all reachable from one process: two known refusal reasons, one
//! this build does not know, and a reply that is neither.

use std::thread;

use toyos::{ipc, services, Listener};
use window::{CreateError, Window};

/// The reply, and the `CreateError` the client must turn it into. `None` is
/// the "not an answer to this request at all" case.
const CASES: &[(Option<u32>, CreateError)] = &[
    (Some(window::REFUSED_AT_CAPACITY), CreateError::AtCapacity),
    (Some(window::REFUSED_TOO_LARGE), CreateError::TooLarge),
    // A reason from a newer compositor than this client. It must arrive as a
    // refusal carrying the raw value, not as a protocol error and not as a
    // window.
    (Some(4242), CreateError::Refused(4242)),
    (None, CreateError::Protocol(window::MSG_FRAME)),
];

fn main() {
    let listener = services::listen("compositor").expect("claim the compositor service");

    let server = thread::spawn(move || {
        for (reply, _) in CASES {
            serve_one(&listener, *reply);
        }
    });

    for (reply, expected) in CASES {
        let outcome = Window::create(100, 100);
        let got = match outcome {
            Ok(_) => panic!("reply {reply:?} produced a window"),
            Err(e) => e,
        };
        assert_eq!(got, *expected, "reply {reply:?} decoded wrongly");
        // The message has to survive being read after the sender let go: the
        // compositor drops the connection the moment it has answered.
        assert_eq!(got.to_string().is_empty(), false, "{got:?} has no message");
    }

    server.join().expect("server thread");
    println!("{} refusal outcomes decoded, none panicked the client", CASES.len());
}

/// Answer one `MSG_CREATE_WINDOW`, then drop the connection — which is what the
/// compositor does after a refusal, and the reason the reply has to still be
/// readable once the writer is gone.
fn serve_one(listener: &Listener, reply: Option<u32>) {
    let accepted = services::accept(listener).expect("accept a client");
    let fd = accepted.conn.fd();
    let header = ipc::recv_header(fd).expect("request header");
    assert_eq!(header.msg_type, window::MSG_CREATE_WINDOW, "client sent the wrong request");
    let _req: window::CreateWindowRequest =
        ipc::recv_payload(fd, &header).expect("request payload");
    match reply {
        Some(reason) => {
            ipc::send(fd, window::MSG_WINDOW_REFUSED, &window::WindowRefused { reason })
                .expect("send the refusal");
        }
        None => {
            ipc::signal(fd, window::MSG_FRAME).expect("send a reply that answers nothing");
        }
    }
}

use std::os::toyos::message::{self, Message};
use toyos_abi::services;

fn compositor_pid() -> u32 {
    services::find("compositor").expect("compositor not running").0
}

pub fn main(args: Vec<String>) {
    if args.is_empty() {
        // Query current resolution
        message::send(compositor_pid(), Message::signal(window::MSG_GET_RESOLUTION))
            .expect("failed to send message");
        let reply = message::recv();
        assert_eq!(reply.msg_type(), window::MSG_RESOLUTION_CHANGED);
        let info: window::ResolutionInfo = reply.take_payload();
        println!("{}x{}", info.width, info.height);
        return;
    }

    // Parse "WIDTHxHEIGHT" or "WIDTH HEIGHT"
    let (width, height) = if args.len() == 1 {
        let parts: Vec<&str> = args[0].split('x').collect();
        if parts.len() != 2 {
            eprintln!("Usage: screen [WIDTHxHEIGHT]");
            return;
        }
        let w: u32 = parts[0].parse().unwrap_or_else(|_| { eprintln!("invalid width"); std::process::exit(1); });
        let h: u32 = parts[1].parse().unwrap_or_else(|_| { eprintln!("invalid height"); std::process::exit(1); });
        (w, h)
    } else {
        let w: u32 = args[0].parse().unwrap_or_else(|_| { eprintln!("invalid width"); std::process::exit(1); });
        let h: u32 = args[1].parse().unwrap_or_else(|_| { eprintln!("invalid height"); std::process::exit(1); });
        (w, h)
    };

    let req = window::ResolutionRequest { width, height };
    message::send(compositor_pid(), Message::new(window::MSG_SET_RESOLUTION, req))
        .expect("failed to send message");
    let reply = message::recv();
    assert_eq!(reply.msg_type(), window::MSG_RESOLUTION_CHANGED);
    let info: window::ResolutionInfo = reply.take_payload();
    println!("{}x{}", info.width, info.height);
}

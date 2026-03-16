use toyos_abi::message;
use toyos_abi::services;
use toyos_abi::Pid;

fn compositor_pid() -> u32 {
    services::find("compositor").expect("compositor not running").0
}

pub fn main(args: Vec<String>) {
    if args.is_empty() {
        // Query current resolution
        message::signal(Pid(compositor_pid()), window::MSG_GET_RESOLUTION);
        let reply = message::recv();
        assert_eq!(reply.msg_type, window::MSG_RESOLUTION_CHANGED);
        let info: window::ResolutionInfo = reply.payload();
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
    message::send(Pid(compositor_pid()), window::MSG_SET_RESOLUTION, &req);
    let reply = message::recv();
    assert_eq!(reply.msg_type, window::MSG_RESOLUTION_CHANGED);
    let info: window::ResolutionInfo = reply.payload();
    println!("{}x{}", info.width, info.height);
}

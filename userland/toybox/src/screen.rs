use toyos::services;

pub fn main(args: Vec<String>) {
    let conn = services::connect("compositor").expect("compositor not running");

    if args.is_empty() {
        conn.signal(window::MSG_GET_RESOLUTION).ok();
        let (msg_type, info): (u32, window::ResolutionInfo) = conn.recv().expect("compositor disconnected");
        assert_eq!(msg_type, window::MSG_RESOLUTION_CHANGED);
        println!("{}x{}", info.width, info.height);
    } else {
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

        conn.send(window::MSG_SET_RESOLUTION, &window::ResolutionRequest { width, height }).ok();
        let (msg_type, info): (u32, window::ResolutionInfo) = conn.recv().expect("compositor disconnected");
        assert_eq!(msg_type, window::MSG_RESOLUTION_CHANGED);
        println!("{}x{}", info.width, info.height);
    }
}

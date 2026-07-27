use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub fn main(args: Vec<String>) {
    let (host, port, path) = if args.len() > 1 {
        parse_url(&args[1])
    } else {
        ("example.com".into(), 80, "/".into())
    };

    println!("Connecting to {host}:{port}...");

    let mut stream = match TcpStream::connect((&*host, port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("net: connection failed: {e}");
            return;
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();

    println!("Connected! Sending HTTP request...");

    let request = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n");
    if let Err(e) = stream.write_all(request.as_bytes()) {
        eprintln!("net: write failed: {e}");
        return;
    }

    let mut response = Vec::new();
    if let Err(e) = stream.read_to_end(&mut response) {
        eprintln!("net: read failed: {e}");
        return;
    }

    let text = String::from_utf8_lossy(&response);
    let preview = if text.len() > 1000 { &text[..1000] } else { &text };
    println!("{preview}");
    if text.len() > 1000 {
        println!("... ({} bytes total)", text.len());
    }
}

fn parse_url(url: &str) -> (String, u16, String) {
    // Simple URL parser: "http://host:port/path" or just "host"
    let url = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = match url.find('/') {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, "/"),
    };
    let (host, port) = match host_port.rfind(':') {
        Some(i) => (&host_port[..i], host_port[i+1..].parse().unwrap_or(80)),
        None => (host_port, 80),
    };
    (host.to_string(), port, path.to_string())
}

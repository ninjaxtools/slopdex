//! Dependency-free subprocess used by harness tests. Compiled into a TempDir.
use std::{io::{Read, Write}, net::TcpStream, time::Duration};

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    match args[0].as_str() {
        "echo" => print!("{input}"),
        "emit" => print!("{}", args[1]),
        "fail" => { eprintln!("secret-bearing diagnostics"); print!("secret-bearing output"); std::process::exit(7); }
        "sleep" => std::thread::sleep(Duration::from_millis(args[1].parse().unwrap())),
        "flood" => { let bytes = vec![b'x'; args[1].parse().unwrap()]; std::io::stdout().write_all(&bytes).unwrap(); }
        "http" => {
            let mut stream = TcpStream::connect(&args[1]).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
            write!(stream, "POST /v1/harness HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer local-integration-placeholder\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", input.len(), input).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            let (headers, body) = response.split_once("\r\n\r\n").unwrap();
            if !headers.starts_with("HTTP/1.1 200") { std::process::exit(1); }
            print!("{body}");
        }
        _ => panic!("unknown fixture mode"),
    }
}

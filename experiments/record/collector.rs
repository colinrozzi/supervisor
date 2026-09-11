// Minimal std-only HTTP collector for the record E2E: appends each request body
// (one per line) to a capture file and returns 200. Sequential, good enough.
use std::io::{Read, Write};
use std::net::TcpListener;

fn main() {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:8899".into());
    let out = std::env::args().nth(2).unwrap_or_else(|| "/tmp/standup/record-capture.log".into());
    let listener = TcpListener::bind(&addr).expect("bind");
    eprintln!("collector listening on {addr}, capturing to {out}");
    for stream in listener.incoming() {
        let mut s = match stream { Ok(s) => s, Err(_) => continue };
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        // read headers
        loop {
            match s.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(hdr_end) = find(&buf, b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..hdr_end]).to_string();
                        let clen = head.lines()
                            .find_map(|l| {
                                let l = l.to_ascii_lowercase();
                                l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        let body_start = hdr_end + 4;
                        while buf.len() < body_start + clen {
                            match s.read(&mut tmp) { Ok(0) => break, Ok(n) => buf.extend_from_slice(&tmp[..n]), Err(_) => break }
                        }
                        let body = &buf[body_start..(body_start + clen).min(buf.len())];
                        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&out).unwrap();
                        f.write_all(body).unwrap();
                        f.write_all(b"\n").unwrap();
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = s.flush();
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

//! Deterministic Redis protocol fixture: a legal SSCAN duplicate across batches.
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::Command,
    time::Duration,
};

#[test]
fn paginated_listing_counts_duplicate_out_of_page_keys_once() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut scans = 0;
        let mut hashes = 0;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let count: usize = line.trim().strip_prefix('*').unwrap().parse().unwrap();
            let mut args = Vec::new();
            for _ in 0..count {
                line.clear();
                reader.read_line(&mut line).unwrap();
                let len: usize = line.trim().strip_prefix('$').unwrap().parse().unwrap();
                let mut bytes = vec![0; len + 2];
                reader.read_exact(&mut bytes).unwrap();
                args.push(String::from_utf8(bytes[..len].to_vec()).unwrap());
            }
            let response = match args[0].as_str() {
                "SSCAN" => {
                    scans += 1;
                    if args[2] == "0" {
                        "*2\r\n$1\r\n1\r\n*2\r\n$1\r\na\r\n$1\r\nb\r\n"
                    } else {
                        "*2\r\n$1\r\n0\r\n*1\r\n$1\r\nb\r\n"
                    }
                }
                "HGETALL" => {
                    hashes += 1;
                    "*4\r\n$5\r\nowner\r\n$5\r\nalice\r\n$6\r\nactive\r\n$4\r\ntrue\r\n"
                }
                _ => "+OK\r\n",
            };
            socket.write_all(response.as_bytes()).unwrap();
        }
        (scans, hashes)
    });
    let output = Command::new(env!("CARGO_BIN_EXE_rpc-admin"))
        .args([
            "--redis-url",
            &format!("redis://{addr}"),
            "list",
            "--limit",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("Found 2 key(s):"), "{text}");
    assert!(
        text.contains("Showing 1-1 of 2\nNext page: --offset 1"),
        "{text}"
    );
    assert_eq!(
        server.join().unwrap(),
        (2, 2),
        "duplicate keys should not be fetched again"
    );
}

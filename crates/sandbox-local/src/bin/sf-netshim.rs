//! In-sandbox egress shim (SBX-1): `sf-netshim <unix-socket> -- <program> [args...]`.
//! Listens on 127.0.0.1:<free port>, forwards each connection to the proxy's Unix socket, runs the
//! program with proxy env vars and exits with its status (128+signal if killed).

use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::{exit, Command};
use std::thread;

fn bridge(tcp: TcpStream, sock: &str) {
    let Ok(unix) = UnixStream::connect(sock) else { return };
    let (Ok(mut t2), Ok(mut u2)) = (tcp.try_clone(), unix.try_clone()) else { return };
    let h = thread::spawn(move || {
        let _ = std::io::copy(&mut t2, &mut u2);
        let _ = u2.shutdown(Shutdown::Both);
        let _ = t2.shutdown(Shutdown::Both);
    });
    let (mut tcp, mut unix) = (tcp, unix);
    let _ = std::io::copy(&mut unix, &mut tcp);
    let _ = tcp.shutdown(Shutdown::Both);
    let _ = unix.shutdown(Shutdown::Both);
    let _ = h.join();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 || args[1] != "--" {
        eprintln!("usage: sf-netshim <socket> -- <program> [args...]");
        exit(2);
    }
    let sock = args[0].clone();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| {
        eprintln!("sf-netshim: bind: {e}");
        exit(126)
    });
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    thread::spawn(move || {
        for c in listener.incoming().flatten() {
            let s = sock.clone();
            thread::spawn(move || bridge(c, &s));
        }
    });
    let url = format!("http://127.0.0.1:{port}");
    let mut cmd = Command::new(&args[2]);
    cmd.args(&args[3..]);
    for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "ALL_PROXY", "all_proxy"] {
        cmd.env(k, &url);
    }
    match cmd.status() {
        Ok(st) => exit(st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0))),
        Err(e) => {
            eprintln!("sf-netshim: spawn {}: {e}", args[2]);
            exit(127)
        }
    }
}

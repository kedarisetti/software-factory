//! Per-call egress allowlist proxy (SBX-1). Listens on a Unix socket that the in-sandbox shim
//! bridges to loopback; handles `CONNECT host:port` and absolute-form `GET http://host/path`.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use sf_core::{EgressAttempt, HostRule};

const DENIED: &str = "HTTP/1.1 403 Forbidden\r\nX-SF-Egress: denied\r\nContent-Length: 0\r\n\r\n";
const BAD: &str = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
const BAD_GATEWAY: &str = "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n";
const MAX_HEAD: usize = 64 * 1024;
static SEQ: AtomicU32 = AtomicU32::new(0);

/// A running proxy; `finish` stops it and returns the recorded attempts in order.
pub struct Proxy {
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    attempts: Arc<Mutex<Vec<EgressAttempt>>>,
    accept: JoinHandle<()>,
}

impl Proxy {
    /// Starts the proxy on `<fresh short dir>/p.sock`.
    pub fn start(rules: Vec<HostRule>) -> std::io::Result<Proxy> {
        let dir = PathBuf::from(format!("/tmp/sfp-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let listener = UnixListener::bind(dir.join("p.sock")).and_then(|l| l.set_nonblocking(true).map(|_| l));
        let listener = match listener {
            Ok(l) => l,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        let (stop, attempts) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
        let (s, a, rules) = (stop.clone(), attempts.clone(), Arc::new(rules));
        let accept = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((c, _)) => {
                        let (a, rules) = (a.clone(), rules.clone());
                        thread::spawn(move || {
                            let _ = c.set_nonblocking(false);
                            let _ = handle(c, &rules, &a);
                        });
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Ok(Proxy { dir, stop, attempts, accept })
    }

    /// Directory holding `p.sock`, to bind into the sandbox.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stops accepting, removes the socket dir and returns every attempt seen.
    pub fn finish(self) -> Vec<EgressAttempt> {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.accept.join();
        let _ = std::fs::remove_dir_all(&self.dir);
        let v = self.attempts.lock().map(|g| g.clone()).unwrap_or_default();
        v
    }
}

/// Reads up to the end of the request head; returns (head, bytes read past it).
fn read_head(c: &mut UnixStream) -> Option<(String, Vec<u8>)> {
    c.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    let (mut buf, mut chunk) = (Vec::new(), [0u8; 4096]);
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(i + 4);
            let head = String::from_utf8(buf).ok()?;
            c.set_read_timeout(None).ok()?;
            return Some((head, rest));
        }
        let n = c.read(&mut chunk).ok()?;
        if n == 0 || buf.len() > MAX_HEAD {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Splits `host[:port]` (including `[v6]:port`), defaulting to `default`.
fn split_authority(a: &str, default: u16) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = a.strip_prefix('[') {
        let (h, p) = rest.split_once(']')?;
        (h, p.strip_prefix(':'))
    } else {
        match a.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (a, None),
        }
    };
    let port = match port {
        Some(p) => p.parse().ok()?,
        None => default,
    };
    (!host.is_empty()).then(|| (host.to_ascii_lowercase(), port))
}

/// True for addresses an allowlisted *name* must never resolve to (SSRF guard).
fn internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => internal_v4(v),
        IpAddr::V6(v) => internal_v6(v),
    }
}

fn internal_v4(v: Ipv4Addr) -> bool {
    let o = v.octets();
    v.is_loopback() || v.is_private() || v.is_link_local() || v.is_unspecified() || v.is_broadcast()
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
}

fn internal_v6(v: Ipv6Addr) -> bool {
    if let Some(m) = v.to_ipv4_mapped() {
        return internal_v4(m);
    }
    let s = v.segments()[0];
    v.is_loopback() || v.is_unspecified() || (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80
}

/// Applies the allowlist and SSRF guard; returns the addresses to connect to when allowed.
fn authorize(rules: &[HostRule], host: &str, port: u16) -> Option<Vec<SocketAddr>> {
    rules.iter().find(|r| r.allows(host, port))?;
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        return Some(vec![SocketAddr::new(ip, port)]);
    }
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs().ok()?.collect();
    (!addrs.is_empty() && !addrs.iter().any(|a| internal(a.ip()))).then_some(addrs)
}

fn dial(addrs: &[SocketAddr]) -> Option<TcpStream> {
    addrs.iter().find_map(|a| TcpStream::connect_timeout(a, Duration::from_secs(10)).ok())
}

/// Rewrites an absolute-form request head to origin-form with `Connection: close`.
fn rewrite(head: &str, method: &str, path: &str, version: &str) -> String {
    let mut out = format!("{method} {path} {version}\r\n");
    for line in head.split("\r\n").skip(1).filter(|l| !l.is_empty()) {
        let name = line.split(':').next().unwrap_or("").to_ascii_lowercase();
        if !matches!(name.as_str(), "connection" | "proxy-connection" | "proxy-authorization") {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    out.push_str("Connection: close\r\n\r\n");
    out
}

fn handle(mut c: UnixStream, rules: &[HostRule], log: &Mutex<Vec<EgressAttempt>>) -> std::io::Result<()> {
    let Some((head, rest)) = read_head(&mut c) else { return c.write_all(BAD.as_bytes()) };
    let mut parts = head.lines().next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
        return c.write_all(BAD.as_bytes());
    };
    let connect = method.eq_ignore_ascii_case("CONNECT");
    let (authority, path) = if connect {
        (target, String::new())
    } else if target.len() > 7 && target[..7].eq_ignore_ascii_case("http://") {
        let r = &target[7..];
        match r.find('/') {
            Some(i) => (&r[..i], r[i..].to_string()),
            None => (r, "/".to_string()),
        }
    } else {
        return c.write_all(BAD.as_bytes());
    };
    let Some((host, port)) = split_authority(authority, if connect { 443 } else { 80 }) else {
        return c.write_all(BAD.as_bytes());
    };
    let addrs = authorize(rules, &host, port);
    if let Ok(mut g) = log.lock() {
        g.push(EgressAttempt { host: host.clone(), port, allowed: addrs.is_some() });
    }
    let Some(addrs) = addrs else { return c.write_all(DENIED.as_bytes()) };
    let Some(mut up) = dial(&addrs) else { return c.write_all(BAD_GATEWAY.as_bytes()) };
    if connect {
        c.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    } else {
        up.write_all(rewrite(&head, method, &path, version).as_bytes())?;
    }
    up.write_all(&rest)?;
    pipe(c, up)
}

/// Copies both directions until either side ends, then shuts both down.
fn pipe(c: UnixStream, up: TcpStream) -> std::io::Result<()> {
    let (mut c2, mut up2) = (c.try_clone()?, up.try_clone()?);
    let t = thread::spawn(move || {
        let _ = std::io::copy(&mut c2, &mut up2);
        let _ = up2.shutdown(std::net::Shutdown::Both);
        let _ = c2.shutdown(std::net::Shutdown::Both);
    });
    let (mut c, mut up) = (c, up);
    let _ = std::io::copy(&mut up, &mut c);
    let _ = c.shutdown(std::net::Shutdown::Both);
    let _ = up.shutdown(std::net::Shutdown::Both);
    let _ = t.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_parsing() {
        assert_eq!(split_authority("Example.com", 80), Some(("example.com".into(), 80)));
        assert_eq!(split_authority("a.b:8443", 80), Some(("a.b".into(), 8443)));
        assert_eq!(split_authority("[::1]:9", 80), Some(("::1".into(), 9)));
        assert_eq!(split_authority("a:x", 80), None);
        assert_eq!(split_authority(":80", 80), None);
    }

    #[test]
    fn ssrf_guard_classification() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.0.1", "169.254.169.254", "0.0.0.0", "100.64.0.1", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1"] {
            assert!(internal(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "93.184.216.34", "2606:4700::1"] {
            assert!(!internal(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn authorize_checks_rules_and_literals() {
        let rules = vec![HostRule { host: "127.0.0.1".into(), port: Some(9) }, HostRule { host: "localhost".into(), port: None }];
        assert!(authorize(&rules, "127.0.0.1", 9).is_some());
        assert!(authorize(&rules, "127.0.0.1", 10).is_none());
        // Allowlisted name that resolves to loopback is refused.
        assert!(authorize(&rules, "localhost", 80).is_none());
    }

    #[test]
    fn rewrite_to_origin_form_closes_connection() {
        let h = "GET http://a/b HTTP/1.1\r\nHost: a\r\nProxy-Connection: keep-alive\r\nConnection: keep-alive\r\nX: y\r\n\r\n";
        assert_eq!(rewrite(h, "GET", "/b", "HTTP/1.1"), "GET /b HTTP/1.1\r\nHost: a\r\nX: y\r\nConnection: close\r\n\r\n");
    }
}

//! SBX-1 egress allowlist integration tests: a std listener in this process plays the remote server.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use sf_core::{EgressAttempt, Error, HostRule, KillSwitch, Limits, Sandbox, ToolCall, ToolOutput};
use sf_sandbox_local::LocalSandbox;

/// A server answering every connection with a fixed HTTP 200 body and counting connections.
struct Remote {
    port: u16,
    hits: Arc<AtomicUsize>,
}

fn remote() -> Remote {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let (port, hits) = (l.local_addr().unwrap().port(), Arc::new(AtomicUsize::new(0)));
    let h = hits.clone();
    thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            h.fetch_add(1, Ordering::SeqCst);
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = c.read(&mut buf);
                let _ = c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello");
            });
        }
    });
    Remote { port, hits }
}

fn sandbox() -> (tempfile::TempDir, LocalSandbox) {
    let d = tempfile::tempdir().unwrap();
    let s = LocalSandbox::new(d.path().join("root")).unwrap().with_shim(env!("CARGO_BIN_EXE_sf-netshim"));
    (d, s)
}

fn rule(port: u16) -> HostRule {
    HostRule { host: "127.0.0.1".into(), port: Some(port) }
}

fn run(s: &LocalSandbox, egress: Vec<HostRule>, script: &str) -> ToolOutput {
    let c = ToolCall {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        workspace: "w".into(),
        egress,
        ..Default::default()
    };
    s.run(&c, &Limits::default(), &KillSwitch::new()).unwrap()
}

fn attempt(port: u16, allowed: bool) -> EgressAttempt {
    EgressAttempt { host: "127.0.0.1".into(), port, allowed }
}

#[test]
fn allowlisted_host_reachable_and_recorded() {
    let (_d, s) = sandbox();
    let r = remote();
    let o = run(&s, vec![rule(r.port)], &format!("curl -s -m 5 http://127.0.0.1:{}/x", r.port));
    assert_eq!((o.stdout.as_str(), o.exit_code), ("hello", Some(0)), "{}", o.stderr);
    assert_eq!(o.egress, vec![attempt(r.port, true)]);
    assert_eq!(r.hits.load(Ordering::SeqCst), 1);
}

#[test]
fn allowlisted_connect_tunnel_works() {
    let (_d, s) = sandbox();
    let r = remote();
    // CONNECT to an http:// URL via -p (tunnel mode).
    let o = run(&s, vec![rule(r.port)], &format!("curl -s -m 5 -p http://127.0.0.1:{}/", r.port));
    assert_eq!(o.stdout, "hello", "{}", o.stderr);
    assert_eq!(o.egress, vec![attempt(r.port, true)]);
}

#[test]
fn non_allowlisted_port_denied_and_never_connects() {
    let (_d, s) = sandbox();
    let (ok, bad) = (remote(), remote());
    let o = run(&s, vec![rule(ok.port)], &format!("curl -s -i -m 5 http://127.0.0.1:{}/", bad.port));
    assert!(o.stdout.contains("403") && !o.stdout.contains("hello"), "{}", o.stdout);
    assert_eq!(o.egress, vec![attempt(bad.port, false)]);
    assert_eq!(bad.hits.load(Ordering::SeqCst), 0);
    assert_eq!(ok.hits.load(Ordering::SeqCst), 0);
}

#[test]
fn connect_to_denied_host_denied() {
    let (_d, s) = sandbox();
    let r = remote();
    let o = run(&s, vec![rule(r.port)], "curl -s -m 5 -p https://evil.example.com/ ; echo rc=$?");
    assert!(!o.stdout.contains("hello"));
    assert_eq!(o.egress, vec![EgressAttempt { host: "evil.example.com".into(), port: 443, allowed: false }]);
}

#[test]
fn raw_tcp_ignoring_proxy_cannot_reach_host() {
    let (_d, s) = sandbox();
    let r = remote();
    let p = r.port;
    let o = run(
        &s,
        vec![rule(p)],
        &format!("curl -s -m 3 --noproxy '*' http://127.0.0.1:{p}/; echo curl=$?; bash -c 'exec 3<>/dev/tcp/127.0.0.1/{p}' 2>&1; echo bash=$?"),
    );
    assert!(!o.stdout.contains("hello"), "{}", o.stdout);
    assert!(!o.stdout.contains("curl=0") && !o.stdout.contains("bash=0"), "{}", o.stdout);
    assert_eq!(r.hits.load(Ordering::SeqCst), 0);
}

#[test]
fn no_network_with_empty_egress() {
    let (_d, s) = sandbox();
    let r = remote();
    let o = run(&s, vec![], &format!("curl -s -m 3 http://127.0.0.1:{}/", r.port));
    assert_ne!(o.exit_code, Some(0));
    assert_eq!(r.hits.load(Ordering::SeqCst), 0);
    assert!(o.egress.is_empty());
}

#[test]
fn shim_propagates_exit_code_and_missing_program() {
    let (_d, s) = sandbox();
    let r = remote();
    assert_eq!(run(&s, vec![rule(r.port)], "exit 7").exit_code, Some(7));
    let c = ToolCall { program: "no-such-prog-xyz".into(), egress: vec![rule(r.port)], ..Default::default() };
    let Err(Error::Invalid(m)) = s.run(&c, &Limits::default(), &KillSwitch::new()) else { panic!() };
    assert!(m.contains("no-such-prog-xyz"));
}

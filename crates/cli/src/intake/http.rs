//! Minimal std-only HTTP/1.1 server for webhooks (TRG-1): thread per connection, Content-Length
//! bodies only, size and time limits. One JSON log line per request, never payload text or secrets.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sf_core::{Clock, SystemClock};
use sf_intake::slack::SlackOutcome;
use sf_intake::{github, slack, ConnectorError, Headers, Outcome};

use super::config::Loaded;

pub const MAX_BODY: usize = 1 << 20;
const MAX_HEAD: usize = 16 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

struct Request {
    method: String,
    path: String,
    headers: Headers,
    body: Vec<u8>,
}

struct Reply {
    status: u16,
    ctype: &'static str,
    body: String,
    /// Short non-sensitive result for the log line.
    result: String,
    item_id: Option<String>,
}

impl Reply {
    fn json(status: u16, result: &str, body: Value) -> Self {
        Reply { status, ctype: "application/json", body: body.to_string(), result: result.into(), item_id: None }
    }
}

pub fn serve(listener: TcpListener, cfg: Arc<Loaded>) -> ! {
    loop {
        let Ok((stream, _)) = listener.accept() else { continue };
        let cfg = cfg.clone();
        std::thread::spawn(move || handle(stream, &cfg));
    }
}

fn handle(mut stream: TcpStream, cfg: &Loaded) {
    let t0 = Instant::now();
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let (method, path, reply, retry) = match read_request(&mut stream) {
        Ok(req) => {
            let retry = req.headers.get("X-Slack-Retry-Num").map(String::from);
            let reply = route(cfg, &req);
            (req.method, req.path, reply, retry)
        }
        Err((status, why)) => ("?".into(), "?".into(), Reply::json(status, why, json!({"error": why})), None),
    };
    let _ = write_reply(&mut stream, &reply);
    let line = json!({
        "ts": SystemClock.now(), "method": method, "path": path, "status": reply.status,
        "result": reply.result, "item_id": reply.item_id, "slack_retry": retry,
        "ms": t0.elapsed().as_millis() as u64,
    });
    eprintln!("{line}");
}

fn read_request(stream: &mut TcpStream) -> Result<Request, (u16, &'static str)> {
    let mut r = BufReader::new(stream.try_clone().map_err(|_| (500, "io"))?.take((MAX_HEAD + MAX_BODY) as u64));
    let mut line = String::new();
    r.read_line(&mut line).map_err(|_| (400, "bad request line"))?;
    let mut parts = line.split_whitespace();
    let (method, path) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(p), Some(v)) if v.starts_with("HTTP/1.") => (m.to_string(), p.split('?').next().unwrap_or(p).to_string()),
        _ => return Err((400, "bad request line")),
    };
    let (mut pairs, mut head_bytes) = (Vec::new(), line.len());
    loop {
        let mut h = String::new();
        let n = r.read_line(&mut h).map_err(|_| (400, "bad header"))?;
        head_bytes += n;
        if n == 0 || head_bytes > MAX_HEAD {
            return Err((431, "headers too large"));
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':').ok_or((400, "bad header"))?;
        pairs.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    let get = |n: &str| pairs.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
    if get("transfer-encoding").is_some() {
        return Err((411, "chunked bodies are not supported"));
    }
    let len: usize = match get("content-length") {
        Some(v) => v.parse().map_err(|_| (400, "bad content-length"))?,
        None => 0,
    };
    if len > MAX_BODY {
        return Err((413, "body too large"));
    }
    if get("expect").is_some_and(|e| e.eq_ignore_ascii_case("100-continue")) {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).map_err(|_| (400, "short body"))?;
    let headers = Headers::from_pairs(&pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>());
    Ok(Request { method, path, headers, body })
}

fn write_reply(stream: &mut TcpStream, r: &Reply) -> std::io::Result<()> {
    let reason = match r.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        r.status, r.ctype, r.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(r.body.as_bytes())?;
    stream.flush()
}

fn route(cfg: &Loaded, req: &Request) -> Reply {
    let post = req.method == "POST";
    match (req.path.as_str(), post) {
        ("/healthz", _) if req.method == "GET" => Reply::json(200, "ok", json!({"ok": true})),
        ("/slack/events", true) => slack_events(cfg, req),
        ("/github/webhook", true) => github_webhook(cfg, req),
        ("/healthz" | "/slack/events" | "/github/webhook", false) => Reply::json(405, "method not allowed", json!({"error": "method not allowed"})),
        _ => Reply::json(404, "not found", json!({"error": "not found"})),
    }
}

fn slack_events(cfg: &Loaded, req: &Request) -> Reply {
    let Some(sc) = &cfg.slack else { return Reply::json(404, "slack not configured", json!({"error": "not configured"})) };
    match slack::handle(sc, &req.headers, &req.body, SystemClock.now()) {
        Ok(SlackOutcome::Challenge(c)) => Reply { status: 200, ctype: "text/plain", body: c, result: "challenge".into(), item_id: None },
        Ok(SlackOutcome::Event(ev)) => accepted(cfg.intake.accept(&ev)),
        Err(e) => rejected(&e),
    }
}

fn github_webhook(cfg: &Loaded, req: &Request) -> Reply {
    let Some(gc) = &cfg.github else { return Reply::json(404, "github not configured", json!({"error": "not configured"})) };
    match github::handle(gc, &req.headers, &req.body, SystemClock.now()) {
        Ok(ev) => accepted(cfg.intake.accept(&ev)),
        Err(e) => rejected(&e),
    }
}

fn accepted(r: sf_core::Result<Outcome>) -> Reply {
    match r {
        Ok(o) => {
            let status = serde_json::to_value(o.status).unwrap_or(Value::Null);
            let mut reply = Reply::json(
                200,
                status.as_str().unwrap_or("?"),
                json!({"item_id": o.item.item_id, "status": status, "inserted": o.inserted, "duplicate_of": o.duplicate_of}),
            );
            reply.item_id = Some(o.item.item_id);
            reply
        }
        Err(_) => Reply::json(500, "store error", json!({"error": "internal error"})),
    }
}

/// Unsupported events answer 200 so the sender does not retry (TRG-1).
fn rejected(e: &ConnectorError) -> Reply {
    let result = match e {
        ConnectorError::BadSignature => "bad signature",
        ConnectorError::StaleTimestamp { .. } => "stale timestamp",
        ConnectorError::MissingHeader(_) => "missing header",
        ConnectorError::Unsupported(_) => "ignored",
        ConnectorError::Malformed(_) => "malformed",
    };
    Reply::json(e.http_status(), result, json!({"error": result}))
}

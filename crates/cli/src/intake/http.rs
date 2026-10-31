//! Minimal std-only HTTP/1.1 server for webhook sources (TRG-1): thread per connection, Content-Length
//! bodies only, size and time limits. One JSON log line per request, never payload text or secrets.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sf_core::{Clock, SystemClock};
use sf_intake::webhook::{self, WebhookOutcome};
use sf_intake::{ConnectorError, Headers, Mapped};

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
        Reply {
            status,
            ctype: "application/json",
            body: body.to_string(),
            result: result.into(),
            item_id: None,
        }
    }
}

pub fn serve(listener: TcpListener, cfg: Arc<Loaded>) -> ! {
    loop {
        let Ok((stream, _)) = listener.accept() else {
            continue;
        };
        let cfg = cfg.clone();
        std::thread::spawn(move || handle(stream, &cfg));
    }
}

fn handle(mut stream: TcpStream, cfg: &Loaded) {
    let t0 = Instant::now();
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let (method, path, reply) = match read_request(&mut stream) {
        Ok(req) => {
            let reply = route(cfg, &req);
            (req.method, req.path, reply)
        }
        Err((status, why)) => (
            "?".into(),
            "?".into(),
            Reply::json(status, why, json!({"error": why})),
        ),
    };
    let _ = write_reply(&mut stream, &reply);
    let line = json!({
        "ts": SystemClock.now(), "method": method, "path": path, "status": reply.status,
        "result": reply.result, "item_id": reply.item_id,
        "ms": t0.elapsed().as_millis() as u64,
    });
    eprintln!("{line}");
}

fn read_request(stream: &mut TcpStream) -> Result<Request, (u16, &'static str)> {
    let mut r = BufReader::new(
        stream
            .try_clone()
            .map_err(|_| (500, "io"))?
            .take((MAX_HEAD + MAX_BODY) as u64),
    );
    let mut line = String::new();
    r.read_line(&mut line)
        .map_err(|_| (400, "bad request line"))?;
    let mut parts = line.split_whitespace();
    let (method, path) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(p), Some(v)) if v.starts_with("HTTP/1.") => {
            (m.to_string(), p.split('?').next().unwrap_or(p).to_string())
        }
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
    let headers = Headers::from_pairs(
        &pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect::<Vec<_>>(),
    );
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
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

/// `POST /hooks/<source-name>` for a webhook source; `GET /healthz`. Anything else is 404.
fn route(cfg: &Loaded, req: &Request) -> Reply {
    let post = req.method == "POST";
    if req.path == "/healthz" {
        return if req.method == "GET" {
            Reply::json(200, "ok", json!({"ok": true}))
        } else {
            Reply::json(
                405,
                "method not allowed",
                json!({"error": "method not allowed"}),
            )
        };
    }
    let Some(hook) = req
        .path
        .strip_prefix("/hooks/")
        .and_then(|n| cfg.webhooks.get(n))
    else {
        return Reply::json(404, "not found", json!({"error": "not found"}));
    };
    if !post {
        return Reply::json(
            405,
            "method not allowed",
            json!({"error": "method not allowed"}),
        );
    }
    match webhook::handle(hook, &req.headers, &req.body, SystemClock.now()) {
        Ok(WebhookOutcome::Challenge(c)) => Reply {
            status: 200,
            ctype: "text/plain",
            body: c,
            result: "challenge".into(),
            item_id: None,
        },
        Ok(WebhookOutcome::Events(m)) => accepted(cfg, m),
        Err(e) => rejected(&e),
    }
}

/// Accepts every event. The body lists them under `items`; the first one's fields are repeated at the top level.
fn accepted(cfg: &Loaded, m: Mapped) -> Reply {
    let mut items = Vec::new();
    for ev in &m.events {
        match cfg.intake.accept(ev) {
            Ok(o) => items.push(json!({"item_id": o.item.item_id, "status": o.status, "inserted": o.inserted, "duplicate_of": o.duplicate_of})),
            Err(_) => return Reply::json(500, "store error", json!({"error": "internal error"})),
        }
    }
    let first = items.first().cloned().unwrap_or(Value::Null);
    let mut body = first.clone();
    body["items"] = json!(items);
    let mut reply = Reply::json(200, first["status"].as_str().unwrap_or("?"), body);
    reply.item_id = first["item_id"].as_str().map(String::from);
    reply
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

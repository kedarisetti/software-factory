//! Minimal MCP client over stdio (newline-delimited JSON-RPC 2.0), std only. One short session per
//! poll: spawn, initialize, optionally tools/list, tools/call, close stdin, reap. The server runs in
//! its own process group with a cleared env (PATH plus the configured vars) and is killed, group
//! and all, on timeout. Everything it returns is untrusted data.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_LINE: u64 = 8 << 20;
const STDERR_KEEP: usize = 2048;
const REAP_GRACE: Duration = Duration::from_secs(2);

pub struct Call<'a> {
    pub command: &'a [String],
    pub env: Vec<(String, String)>,
    pub tool: &'a str,
    pub arguments: Value,
    pub timeout: Duration,
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Result<String, String>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    timeout: Duration,
}

/// Runs the whole session and returns the parsed tool result (`structuredContent`, or the first text
/// content parsed as JSON). The process is always reaped before this returns.
pub fn call_tool(c: &Call) -> Result<Value, String> {
    let mut s = Session::spawn(c)?;
    let r = s.run(c);
    s.finish(r.is_err());
    r
}

impl Session {
    fn spawn(c: &Call) -> Result<Session, String> {
        let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
        let mut child = Command::new(&c.command[0])
            .args(&c.command[1..])
            .env_clear()
            .env("PATH", path)
            .envs(c.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| format!("cannot start MCP server {:?}: {e}", c.command[0]))?;
        let stdin = child.stdin.take();
        let out = child.stdout.take().ok_or("no stdout")?;
        let mut err = child.stderr.take().ok_or("no stderr")?;
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(out);
            loop {
                let mut buf = String::new();
                match r.by_ref().take(MAX_LINE).read_line(&mut buf) {
                    Ok(0) => return drop(tx.send(Err("server closed stdout".into()))),
                    Ok(_) if !buf.ends_with('\n') && buf.len() as u64 >= MAX_LINE => return drop(tx.send(Err("server line too long".into()))),
                    Ok(_) => {
                        if tx.send(Ok(buf)).is_err() {
                            return;
                        }
                    }
                    Err(e) => return drop(tx.send(Err(format!("read error: {e}")))),
                }
            }
        });
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let keep = stderr.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 1024];
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut k) = keep.lock() {
                    let room = STDERR_KEEP.saturating_sub(k.len());
                    k.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        });
        Ok(Session { child, stdin, lines, stderr, timeout: c.timeout })
    }

    fn stderr_tail(&self) -> String {
        let b = self.stderr.lock().map(|b| b.clone()).unwrap_or_default();
        let t = String::from_utf8_lossy(&b).trim().replace('\n', " | ");
        if t.is_empty() { String::new() } else { format!(" (server stderr: {t})") }
    }

    fn send(&mut self, v: &Value) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("stdin closed")?;
        let mut line = v.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush()).map_err(|e| format!("write to MCP server failed: {e}{}", self.stderr_tail()))
    }

    /// Waits for the response with `id`, skipping notifications and anything else the server says.
    fn response(&mut self, id: u64, what: &str) -> Result<Value, String> {
        let deadline = Instant::now() + self.timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(left) {
                Ok(Ok(l)) => l,
                Ok(Err(e)) => return Err(format!("{e} while waiting for {what}{}", self.stderr_tail())),
                Err(RecvTimeoutError::Timeout) => return Err(format!("MCP server timed out after {} ms during {what}; process group killed", self.timeout.as_millis())),
                Err(RecvTimeoutError::Disconnected) => return Err(format!("MCP server went away during {what}{}", self.stderr_tail())),
            };
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
            if v.get("id") != Some(&json!(id)) || v.get("method").is_some() {
                continue;
            }
            if let Some(e) = v.get("error") {
                return Err(format!("MCP error during {what}: {} {}", e["code"], e["message"].as_str().unwrap_or("")));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn run(&mut self, c: &Call) -> Result<Value, String> {
        self.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "sf-intake", "version": "0.0.0"}}}))?;
        self.response(1, "initialize")?;
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;

        // Check the tool exists. A server without tools/list (an error reply) is not a failure.
        let (mut id, mut page) = (2u64, None::<String>);
        for _ in 0..10 {
            let params = page.as_ref().map_or(json!({}), |p| json!({"cursor": p}));
            self.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/list", "params": params}))?;
            let Ok(list) = self.response(id, "tools/list") else { break };
            id += 1;
            let tools = list["tools"].as_array().cloned().unwrap_or_default();
            if tools.iter().any(|t| t["name"] == c.tool) {
                break;
            }
            match list["nextCursor"].as_str() {
                Some(n) => page = Some(n.to_string()),
                None => {
                    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).take(20).collect();
                    return Err(format!("MCP server does not offer tool {:?}; it offers: {}", c.tool, names.join(", ")));
                }
            }
        }

        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": c.tool, "arguments": c.arguments}}))?;
        let res = self.response(id, &format!("tools/call {}", c.tool))?;
        tool_value(&res)
    }

    /// Close stdin, give the server a moment to exit, then kill its group; always reap.
    fn finish(&mut self, force: bool) {
        drop(self.stdin.take());
        let end = Instant::now() + if force { Duration::ZERO } else { REAP_GRACE };
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(10)),
                _ => {
                    self.kill_group();
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        // Stragglers the server left behind in its group.
        self.kill_group();
    }

    fn kill_group(&self) {
        let _ = Command::new("kill").args(["-KILL", "--", &format!("-{}", self.child.id())]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}

fn text_of(res: &Value) -> String {
    let parts: Vec<&str> = res["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect();
    parts.join("\n")
}

/// `isError` becomes an error; else `structuredContent`, else the text content parsed as JSON.
pub fn tool_value(res: &Value) -> Result<Value, String> {
    if res["isError"] == true {
        let t: String = text_of(res).chars().take(500).collect();
        return Err(format!("MCP tool returned isError: {t}"));
    }
    if let Some(sc) = res.get("structuredContent").filter(|v| !v.is_null()) {
        return Ok(sc.clone());
    }
    let text = text_of(res);
    serde_json::from_str(&text).map_err(|e| format!("MCP tool result is not JSON text ({e}); the first 80 chars: {:?}", text.chars().take(80).collect::<String>()))
}

/// Replaces `{cursor}` in every string of `v`.
pub fn substitute(v: &Value, cursor: &str) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace("{cursor}", cursor)),
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute(x, cursor)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), substitute(x, cursor))).collect()),
        other => other.clone(),
    }
}

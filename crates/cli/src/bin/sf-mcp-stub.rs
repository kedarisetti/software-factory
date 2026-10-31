//! Test double for an MCP server over stdio: serves `tools/call` with the JSON in `--result <file>`
//! and appends every request it receives to `--log <file>` (one JSON line each). Not shipped logic.
//!
//! `--command` makes it a one-shot `command` source instead: log argv and env, print the result file to
//! stdout and exit; `--mode fail` exits 3 with stderr text that includes `$STUB_TOKEN`, `--mode hang` sleeps,
//! `--mode bad-json` prints text.
//!
//! `--tool <name>` (default list_tickets), `--structured` (return structuredContent instead of text),
//! `--mode ok|error|hang|bad-json`. `hang` spawns a `sleep` grandchild then never answers tools/call.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1)).cloned()
}

fn log(path: &Option<String>, v: Value) {
    if let Some(p) = path {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{v}");
        }
    }
}

fn main() {
    let (tool, mode, logp) = (arg("--tool").unwrap_or_else(|| "list_tickets".into()), arg("--mode").unwrap_or_else(|| "ok".into()), arg("--log"));
    let structured = std::env::args().any(|a| a == "--structured");
    if std::env::args().any(|a| a == "--command") {
        let argv: Vec<String> = std::env::args().collect();
        let env: serde_json::Map<String, Value> = std::env::vars().map(|(k, v)| (k, json!(v))).collect();
        log(&logp, json!({"event": "command", "argv": argv, "env": env}));
        match mode.as_str() {
            "fail" => {
                eprintln!("boom: upstream refused token {}", std::env::var("STUB_TOKEN").unwrap_or_default());
                std::process::exit(3);
            }
            "hang" => std::thread::sleep(std::time::Duration::from_secs(120)),
            "bad-json" => println!("not json at all"),
            _ => print!("{}", arg("--result").and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_else(|| "{}".into())),
        }
        return;
    }
    let env: serde_json::Map<String, Value> = std::env::vars().map(|(k, v)| (k, json!(v))).collect();
    log(&logp, json!({"event": "start", "pid": std::process::id(), "env": env}));
    let out = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let method = req["method"].as_str().unwrap_or("").to_string();
        log(&logp, json!({"event": "request", "method": method, "params": req["params"]}));
        let Some(id) = req.get("id").cloned() else { continue };
        let result = match method.as_str() {
            "initialize" => json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "sf-mcp-stub", "version": "0"}}),
            "tools/list" => json!({"tools": [{"name": tool, "description": "stub", "inputSchema": {"type": "object"}}]}),
            "tools/call" => match mode.as_str() {
                "hang" => {
                    let gc = std::process::Command::new("sleep").arg("60").spawn().map(|c| c.id()).unwrap_or(0);
                    log(&logp, json!({"event": "hang", "grandchild": gc}));
                    std::thread::sleep(std::time::Duration::from_secs(120));
                    return;
                }
                "error" => json!({"isError": true, "content": [{"type": "text", "text": "boom: upstream returned 500"}]}),
                "bad-json" => json!({"content": [{"type": "text", "text": "not json at all"}]}),
                _ => {
                    let body = arg("--result").and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_else(|| "{}".into());
                    if structured {
                        json!({"structuredContent": serde_json::from_str::<Value>(&body).unwrap_or(Value::Null), "content": []})
                    } else {
                        json!({"content": [{"type": "text", "text": body}]})
                    }
                }
            },
            _ => {
                let e = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}});
                let _ = writeln!(out.lock(), "{e}");
                continue;
            }
        };
        let mut o = out.lock();
        let _ = writeln!(o, "{}", json!({"jsonrpc": "2.0", "id": id, "result": result}));
        let _ = o.flush();
    }
}

//! MCP server: newline-delimited JSON-RPC 2.0 over stdio (KIF-3). Only protocol bytes go to `out`.

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};
use sf_kb::{Caller, Knowledge};

use crate::dispatch::{dispatch, op_for_tool, tools};

const DEFAULT_PROTOCOL: &str = "2025-06-18";

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

struct Server<K, O> {
    agent: Option<String>,
    session: String,
    client: Option<String>,
    open: O,
    kb: Option<K>,
}

impl<K: Knowledge, O: FnMut() -> sf_kb::Result<K>> Server<K, O> {
    fn caller(&self) -> Caller {
        let agent = self.agent.clone().unwrap_or_else(|| format!("mcp:{}", self.client.as_deref().unwrap_or("unknown")));
        Caller { agent, session: self.session.clone() }
    }

    /// Open the store on first use so `initialize` and `tools/list` never touch disk.
    fn call_tool(&mut self, params: &Value) -> Result<Value, (i32, String)> {
        let name = params["name"].as_str().unwrap_or_default();
        let op = op_for_tool(name).ok_or((-32602, format!("unknown tool: {name}")))?;
        let result = match self.kb.as_ref() {
            Some(_) => Ok(()),
            None => (self.open)().map(|k| self.kb = Some(k)).map_err(|e| (e.code().to_string(), e.to_string())),
        }
        .and_then(|()| dispatch(self.kb.as_ref().unwrap(), &self.caller(), op, params["arguments"].clone()));
        let (value, is_err) = match result {
            Ok(v) => (v, false),
            Err((code, message)) => (json!({ "code": code, "message": message }), true),
        };
        Ok(json!({ "content": [{ "type": "text", "text": value.to_string() }], "isError": is_err }))
    }

    /// One request; `None` for notifications.
    fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id")?.clone();
        let params = &msg["params"];
        let result = match msg["method"].as_str().unwrap_or_default() {
            "initialize" => {
                self.client = params["clientInfo"]["name"].as_str().map(str::to_string);
                Ok(json!({
                    "protocolVersion": params["protocolVersion"].as_str().unwrap_or(DEFAULT_PROTOCOL),
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "sf-kb", "version": env!("CARGO_PKG_VERSION") },
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => self.call_tool(params),
            m => Err((-32601, format!("method not found: {m}"))),
        };
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, m)) => error(id, code, &m),
        })
    }
}

/// Serve until `input` closes. `agent` of `None` means `mcp:<clientInfo.name>`.
pub fn serve<K: Knowledge>(
    input: impl BufRead,
    mut out: impl Write,
    agent: Option<String>,
    session: String,
    open: impl FnMut() -> sf_kb::Result<K>,
) -> io::Result<()> {
    let mut server = Server { agent, session, client: None, open, kb: None };
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => server.handle(&msg),
            Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(r) = reply {
            writeln!(out, "{r}")?;
            out.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::tests::Fake;

    fn run(lines: &[Value], agent: Option<&str>) -> (Vec<Value>, std::sync::Arc<Fake>) {
        let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let fake = std::sync::Arc::new(Fake::default());
        let mut out = vec![];
        serve(input.as_bytes(), &mut out, agent.map(str::to_string), "s".into(), || Ok(Shared(fake.clone()))).unwrap();
        let replies = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        (replies, fake)
    }

    /// Lets the test keep a handle on the fake while the server owns its `Knowledge`.
    struct Shared(std::sync::Arc<Fake>);
    impl Knowledge for Shared {
        fn search(&self, c: &Caller, q: &str, t: Option<&str>, k: usize) -> sf_kb::Result<Vec<sf_kb::Hit>> { self.0.search(c, q, t, k) }
        fn get(&self, c: &Caller, id: &str) -> sf_kb::Result<sf_kb::Entry> { self.0.get(c, id) }
        fn add(&self, c: &Caller, e: sf_kb::NewEntry) -> sf_kb::Result<sf_kb::Entry> { self.0.add(c, e) }
        fn learn(&self, c: &Caller, r: sf_kb::LearningRecord) -> sf_kb::Result<sf_kb::Learned> { self.0.learn(c, r) }
        fn feedback(&self, c: &Caller, id: &str, v: sf_kb::Verdict, n: Option<&str>) -> sf_kb::Result<sf_kb::Entry> { self.0.feedback(c, id, v, n) }
        fn check_anchors(&self, c: &Caller) -> sf_kb::Result<sf_kb::AnchorReport> { self.0.check_anchors(c) }
    }

    #[test]
    fn handshake_list_and_unknown() {
        let (r, _) = run(&[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","clientInfo":{"name":"c"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"ping"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":4,"method":"bogus"}),
        ], None);
        assert_eq!(r.len(), 4, "no reply to the notification");
        assert_eq!(r[0]["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r[0]["result"]["serverInfo"]["name"], "sf-kb");
        assert_eq!(r[2]["result"]["tools"].as_array().unwrap().len(), 6);
        assert_eq!(r[3]["error"]["code"], -32601);
    }

    #[test]
    fn tool_calls_and_caller() {
        let (r, fake) = run(&[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"zed"}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"kb_search","arguments":{"query":"q"}}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"kb_get","arguments":{"id":"missing"}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"kb_nope"}}),
        ], None);
        assert_eq!(r[0]["result"]["protocolVersion"], DEFAULT_PROTOCOL);
        assert_eq!(r[1]["result"]["isError"], false);
        let text: Value = serde_json::from_str(r[1]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text[0]["id"], "a");
        assert_eq!(fake.0.lock().unwrap()[0].1, "mcp:zed|q|None|5");
        assert_eq!(r[2]["result"]["isError"], true);
        assert!(r[2]["result"]["content"][0]["text"].as_str().unwrap().contains("not_found"));
        assert_eq!(r[3]["error"]["code"], -32602);
    }

    #[test]
    fn explicit_agent_and_parse_error() {
        let mut out = vec![];
        let input = "{oops\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"kb_check_anchors\"}}\n";
        let fake = std::sync::Arc::new(Fake::default());
        serve(input.as_bytes(), &mut out, Some("claude-code".into()), "s".into(), || Ok(Shared(fake.clone()))).unwrap();
        let lines: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines[0]["error"]["code"], -32700);
        assert_eq!(lines[1]["result"]["isError"], false);
    }
}

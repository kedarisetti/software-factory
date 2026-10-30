//! Black-box tests of the built `sf-kb` binary.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

fn sf(args: &[&str]) -> (i32, Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_sf-kb"))
        .args(args)
        .env_remove("SF_KB_ROOT")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    (out.status.code().unwrap(), serde_json::from_str(&text).unwrap_or_else(|_| panic!("not one JSON doc: {text:?}")))
}

#[test]
fn version_ok_shape() {
    let (code, v) = sf(&["version"]);
    assert_eq!((code, &v["ok"], &v["result"]["contract"]), (0, &json!(true), &json!(1)));
}

#[test]
fn usage_error_is_exit_2_with_json() {
    for args in [&[][..], &["bogus"], &["get"], &["search", "q", "--top-k", "x"]] {
        let (code, v) = sf(args);
        assert_eq!((code, &v["ok"], &v["error"]["code"]), (2, &json!(false), &json!("usage")), "{args:?}");
    }
}

#[test]
fn contract_mismatch_is_version_error() {
    let dir = tempfile::tempdir().unwrap();
    let (code, v) = sf(&["--root", dir.path().to_str().unwrap(), "--contract", "99", "check-anchors"]);
    assert_eq!((code, &v["ok"], &v["error"]["code"]), (1, &json!(false), &json!("version")));
    assert!(v["error"]["message"].as_str().unwrap().contains("99"));
}

fn rpc(lines: &[Value], args: &[&str]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sf-kb"))
        .arg("mcp")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for l in lines {
        writeln!(stdin, "{l}").unwrap();
    }
    drop(stdin);
    let replies = BufReader::new(child.stdout.take().unwrap()).lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
    assert!(child.wait().unwrap().success());
    replies
}

#[test]
fn mcp_session() {
    let r = rpc(
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","clientInfo":{"name":"t","version":"0"},"capabilities":{}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"resources/list"}),
        ],
        &[],
    );
    assert_eq!(r.len(), 3);
    assert_eq!(r[0]["id"], 1);
    assert_eq!(r[0]["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(r[0]["result"]["capabilities"], json!({"tools": {}}));
    let names: Vec<&str> = r[1]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["kb_search", "kb_get", "kb_add", "kb_learn", "kb_feedback", "kb_check_anchors"]);
    assert_eq!(r[2]["error"]["code"], -32601);
}

#[test]
fn mcp_tool_call_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    let call = |id, name, args| json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}});
    let r = rpc(
        &[
            call(1, "kb_add", json!({"title": "Flaky CI retry", "body": "Re-run once"})),
            call(2, "kb_search", json!({"query": "flaky"})),
        ],
        &["--root", root, "--agent", "claude-code"],
    );
    assert_eq!(r[0]["result"]["isError"], false);
    let hits: Value = serde_json::from_str(r[1]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(hits[0]["title"], "Flaky CI retry");
}

#[test]
fn cli_add_search_get() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    let (code, v) = sf(&["--root", root, "add", "--title", "Flaky CI retry", "--body", "Re-run once"]);
    assert_eq!((code, &v["ok"]), (0, &json!(true)));
    let id = v["result"]["meta"]["id"].as_str().unwrap().to_string();
    let (_, v) = sf(&["--root", root, "search", "flaky"]);
    assert_eq!(v["result"][0]["id"], id.as_str());
    let (code, v) = sf(&["--root", root, "get", "nope"]);
    assert_eq!((code, &v["error"]["code"]), (1, &json!("not_found")));
}

//! Configurable intake sources: MCP servers as sources, behind the connector approval gate. Everything
//! goes through the real `sf` binary and the `sf-mcp-stub` test server.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const STUB: &str = env!("CARGO_BIN_EXE_sf-mcp-stub");

struct Env {
    dir: tempfile::TempDir,
    db: PathBuf,
    config: PathBuf,
    tickets: PathBuf,
    calls: PathBuf,
}

fn ticket(key: &str, summary: &str, updated: &str, labels: Value) -> Value {
    json!({"key": key, "summary": summary, "body": format!("details of {key}"), "reporter": {"name": format!("rep-{key}")},
           "project": "PRJ", "labels": labels, "updated": updated, "url": format!("https://tracker.example/{key}")})
}

fn tickets() -> Value {
    json!({"data": {"issues": [
        ticket("PRJ-1", "Add retries", "2025-03-01T10:00:00Z", json!(["bug", {"name": "urgent"}])),
        ticket("PRJ-2", "Fix the parser", "2025-03-02T10:00:00Z", json!(["bug"])),
        {"summary": "has no id"}
    ]}})
}

fn source(e: &Env, name: &str, extra: &[&str], over: Value) -> Value {
    let mut command = vec![STUB.to_string(), "--result".into(), e.tickets.to_str().unwrap().into(), "--log".into(), e.calls.to_str().unwrap().into()];
    command.extend(extra.iter().map(|s| s.to_string()));
    let mut v = json!({
        "name": name, "kind": "mcp", "command": command,
        "env": {"STUB_TOKEN": "${SF_TEST_STUB_TOKEN}"},
        "tool": "list_tickets", "arguments": {"since": "{cursor}", "limit": 50},
        "poll_interval_s": 3600, "timeout_ms": 5000, "chat": false, "default_workflow": "demo",
        "cursor": "max:updated_at",
        "map": {"items": "/data/issues", "id": "/key", "title": "/summary", "text": "/body", "actor": "/reporter/name",
                "channel": "/project", "labels": "/labels", "url": "/url", "updated_at": "/updated"}
    });
    for (k, x) in over.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    v
}

fn cfg(sources: Vec<Value>, rules: Value, deploy: &[&str]) -> Value {
    json!({"version": 2, "sources": sources, "permissions": {"deploy_workflows": deploy, "rules": rules}, "rate": {"per_actor": 50}})
}

fn env_with(build: impl Fn(&Env) -> Value) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().to_path_buf();
    let e = Env { db: p.join("sf.db"), config: p.join("intake.json"), tickets: p.join("tickets.json"), calls: p.join("calls.log"), dir };
    std::fs::write(&e.tickets, tickets().to_string()).unwrap();
    e.write_cfg(&build(&e));
    e
}

fn default_env() -> Env {
    env_with(|e| cfg(vec![source(e, "tickets", &[], json!({}))], json!([{"source": "tickets", "channel": "PRJ", "workflows": ["demo"]}]), &["ship"]))
}

impl Env {
    fn write_cfg(&self, v: &Value) {
        std::fs::write(&self.config, v.to_string()).unwrap();
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_sf"));
        c.arg("--db").arg(&self.db).arg("--sandbox").arg(self.dir.path().join("sbx")).args(args);
        c.env("SF_TEST_STUB_TOKEN", "stub-secret-value").env("SF_TEST_UNRELATED", "must-not-leak").env("SF_DEMO_SLEEP_MS", "0");
        c.env("SF_SHIP_EFFECT_FILE", self.dir.path().join("effects.txt"));
        c
    }

    /// Runs `sf <args> --config c`; Ok(json) on exit 0, Err(error text) otherwise.
    fn try_run(&self, args: &[&str]) -> Result<Value, String> {
        let cfg = self.config.to_str().unwrap();
        let mut a = args.to_vec();
        if args[0] == "intake" {
            a.extend_from_slice(&["--config", cfg]);
        }
        let o = self.cmd(&a).output().unwrap();
        let v: Value = serde_json::from_slice(&o.stdout).unwrap_or_else(|_| panic!("{a:?}: {}", String::from_utf8_lossy(&o.stdout)));
        if o.status.success() { Ok(v) } else { Err(v["error"].as_str().unwrap_or("?").to_string()) }
    }

    fn run(&self, args: &[&str]) -> Value {
        self.try_run(args).unwrap_or_else(|e| panic!("{args:?} failed: {e}"))
    }

    fn items(&self, status: &str) -> Vec<Value> {
        self.run(&["intake", "list", "--status", status]).as_array().unwrap().clone()
    }

    fn approve(&self, name: &str) {
        self.run(&["intake", "connectors", "approve", name, "--as", "alice"]);
    }

    fn poll(&self, name: &str) -> Result<Value, String> {
        let v = self.try_run(&["intake", "poll", "--source", name, "--now"])?;
        Ok(v["polled"][0].clone())
    }

    fn audit(&self) -> Vec<Value> {
        self.run(&["audit"]).as_array().unwrap().clone()
    }

    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.calls).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn tool_calls(&self) -> Vec<Value> {
        self.calls().into_iter().filter(|c| c["method"] == "tools/call").collect()
    }
}

fn alive(pid: u64) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.rsplit(") ").next().is_some_and(|r| !r.starts_with('Z')))
}

fn wait_dead(pid: u64) -> bool {
    let t = Instant::now();
    while alive(pid) {
        if t.elapsed() > Duration::from_secs(3) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

#[test]
fn an_unapproved_source_refuses_and_audits_blocked() {
    let e = default_env();
    let err = e.poll("tickets").unwrap_err();
    assert!(err.contains("disabled") && err.contains("connectors approve tickets"), "{err}");
    assert!(e.calls().is_empty(), "the server must not even start");
    assert!(e.items("queued").is_empty());
    let blocked: Vec<_> = e.audit().into_iter().filter(|r| r["action"] == "new_capability" && r["decision"] == "blocked").collect();
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0]["target"], "intake:tickets");
    // The sweep reports it instead of failing, and does not pile up audit rows.
    let sweep = e.run(&["intake", "poll", "--now"]);
    assert_eq!(sweep["polled"][0]["status"], "blocked", "{sweep}");
    assert_eq!(e.audit().iter().filter(|r| r["decision"] == "blocked").count(), 1);
    let list = e.run(&["intake", "connectors", "list"]);
    assert_eq!((list[0]["enabled"].as_bool(), list[0]["approval"].as_str()), (Some(false), Some("pending")), "{list}");
}

#[test]
fn approve_then_poll_maps_items_dedupes_and_passes_the_cursor() {
    let e = default_env();
    let ap = e.run(&["intake", "connectors", "approve", "tickets", "--as", "alice"]);
    assert_eq!(ap["by"], "alice");
    let a = e.audit().into_iter().find(|r| r["action"] == "new_capability" && r["decision"] == "approved").unwrap();
    assert_eq!((a["target"].as_str(), a["approver"].as_str(), a["digest"].as_str()), (Some("intake:tickets"), Some("alice"), ap["digest"].as_str()));
    assert!(e.run(&["audit-verify"])["ok"].as_bool().unwrap());

    let first = e.poll("tickets").unwrap();
    assert_eq!((first["considered"].as_u64(), first["skipped"].as_u64()), (Some(2), Some(1)), "{first}");
    let queued = e.items("queued");
    assert_eq!(queued.len(), 2, "{queued:?}");
    let one = queued.iter().find(|i| i["source_ref"] == "PRJ-1").unwrap();
    assert_eq!((one["source"].as_str(), one["actor"].as_str(), one["channel"].as_str(), one["workflow"].as_str()), (Some("tickets"), Some("rep-PRJ-1"), Some("PRJ"), Some("demo")));
    assert_eq!(one["dedupe_key"], "tickets:PRJ-1:2025-03-01T10:00:00Z");
    assert_eq!(one["payload"]["title"], "Add retries");
    assert_eq!(one["payload"]["labels"], json!(["bug", "urgent"]));
    assert!(one["payload"]["text"].as_str().unwrap().contains("details of PRJ-1"));
    assert_eq!(one["payload"]["untrusted"], true);
    assert_eq!(one["priority"], 5 + 50, "base for a configured source plus the urgent label rule");

    // Environment: PATH plus exactly the named variable, resolved from the host by reference.
    let start = e.calls().into_iter().find(|c| c["event"] == "start").unwrap();
    let mut names: Vec<_> = start["env"].as_object().unwrap().keys().cloned().collect();
    names.sort();
    assert_eq!(names, ["PATH", "STUB_TOKEN"], "{start}");
    assert_eq!(start["env"]["STUB_TOKEN"], "stub-secret-value");
    let init = e.calls().into_iter().find(|c| c["method"] == "initialize").unwrap();
    assert_eq!((init["params"]["protocolVersion"].as_str(), init["params"]["clientInfo"]["name"].as_str()), (Some("2025-06-18"), Some("sf-intake")));

    // Second poll: same items, nothing new, and the saved cursor goes into the arguments.
    let second = e.poll("tickets").unwrap();
    assert!(second["items"].as_array().unwrap().iter().all(|i| i["inserted"] == false), "{second}");
    assert_eq!(e.run(&["intake", "list"]).as_array().unwrap().len(), 2);
    let calls = e.tool_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["params"]["arguments"], json!({"since": "", "limit": 50}));
    assert_eq!(calls[1]["params"]["arguments"], json!({"since": "2025-03-02T10:00:00Z", "limit": 50}));
    assert_eq!(calls[1]["params"]["name"], "list_tickets");

    // The sweep respects the interval: it just ran, so nothing is due without --now.
    let sweep = e.run(&["intake", "poll"]);
    assert_eq!(sweep["polled"][0]["status"], "not due", "{sweep}");
    assert_eq!(e.tool_calls().len(), 2);
}

#[test]
fn structured_content_is_used_when_present() {
    let e = env_with(|e| cfg(vec![source(e, "tickets", &["--structured"], json!({}))], json!([{"source": "tickets", "channel": "PRJ", "workflows": ["demo"]}]), &[]));
    e.approve("tickets");
    assert_eq!(e.poll("tickets").unwrap()["considered"], 2);
}

#[test]
fn changing_the_config_disables_the_source_until_it_is_approved_again() {
    let e = default_env();
    e.approve("tickets");
    assert!(e.poll("tickets").is_ok());
    let old_digest = e.run(&["intake", "connectors", "list"])[0]["digest"].clone();

    let changed = cfg(vec![source(&e, "tickets", &["--tool", "other_tool"], json!({"tool": "other_tool"}))], json!([{"source": "tickets", "channel": "PRJ", "workflows": ["demo"]}]), &["ship"]);
    e.write_cfg(&changed);
    let err = e.poll("tickets").unwrap_err();
    assert!(err.contains("disabled"), "{err}");
    let list = e.run(&["intake", "connectors", "list"]);
    assert_eq!((list[0]["enabled"].as_bool(), list[0]["approval"].as_str()), (Some(false), Some("pending")));
    assert_ne!(list[0]["digest"], old_digest);

    // Env var names are part of the digest too (a different variable is a different capability).
    e.approve("tickets");
    assert!(e.poll("tickets").is_ok());
    let renamed = cfg(vec![source(&e, "tickets", &["--tool", "other_tool"], json!({"tool": "other_tool", "env": {"STUB_TOKEN": "${SF_TEST_UNRELATED}"}}))], json!([{"source": "tickets", "channel": "PRJ", "workflows": ["demo"]}]), &["ship"]);
    e.write_cfg(&renamed);
    assert!(e.poll("tickets").unwrap_err().contains("disabled"));
}

#[test]
fn revoke_disables_the_source_again() {
    let e = default_env();
    e.approve("tickets");
    assert!(e.poll("tickets").is_ok());
    let r = e.run(&["intake", "connectors", "revoke", "tickets", "--as", "bob"]);
    assert_eq!(r["approval"], "revoked");
    let err = e.poll("tickets").unwrap_err();
    assert!(err.contains("revoked by bob"), "{err}");
    let list = e.run(&["intake", "connectors", "list"]);
    assert_eq!(list[0]["approval"], "revoked");
    let a = e.audit();
    let rev = a.iter().find(|r| r["decision"] == "denied").unwrap();
    assert_eq!((rev["action"].as_str(), rev["approver"].as_str()), (Some("new_capability"), Some("bob")));
    // Approving again re-enables it.
    e.approve("tickets");
    assert!(e.poll("tickets").is_ok());
    // Approval needs a human name and a known MCP source.
    assert!(e.try_run(&["intake", "connectors", "approve", "tickets"]).is_err());
    assert!(e.try_run(&["intake", "connectors", "approve", "nope", "--as", "a"]).is_err());
}

#[test]
fn a_chat_mcp_source_cannot_be_configured_to_allow_a_deploy_workflow() {
    let e = env_with(|e| cfg(vec![source(e, "bridge", &[], json!({"chat": true}))], json!([{"source": "bridge", "channel": "PRJ", "workflows": ["ship"]}]), &["ship"]));
    let err = e.try_run(&["intake", "connectors", "list"]).unwrap_err();
    assert!(err.contains("chat source bridge may not allow deploy workflow ship"), "{err}");
    // The same rule is fine for a non-chat source.
    let ok = env_with(|e| cfg(vec![source(e, "bridge", &[], json!({"chat": false}))], json!([{"source": "bridge", "channel": "PRJ", "workflows": ["ship"]}]), &["ship"]));
    assert!(ok.try_run(&["intake", "connectors", "list"]).is_ok());
}

#[test]
fn a_chat_mcp_item_that_reaches_a_deploy_step_is_blocked_by_the_engine() {
    // Misconfigured on purpose (ship is not declared deploy), so only the engine's chat rule stands.
    let e = env_with(|e| cfg(vec![source(e, "bridge", &[], json!({"chat": true, "default_workflow": "ship"}))], json!([{"source": "bridge", "channel": "PRJ", "workflows": ["ship"]}]), &[]));
    e.approve("bridge");
    e.poll("bridge").unwrap();
    assert_eq!(e.items("queued").len(), 2);
    let started = e.run(&["intake", "dispatch", "--max", "1"]);
    let id = started["started"][0]["run_id"].as_str().unwrap().to_string();
    let status = |e: &Env| e.run(&["status", &id]);
    let mut state = String::new();
    for _ in 0..8 {
        e.run(&["run"]);
        state = status(&e)["state"].as_str().unwrap().to_string();
        if state != "AwaitingApproval" {
            break;
        }
        e.run(&["approve", &id, "--as", "alice"]);
    }
    let s = status(&e);
    assert_eq!(state, "Escalated", "{s}");
    assert!(s["escalation"].as_str().unwrap().contains("deploy from chat trigger chat:bridge:"), "{s}");
    let effects = std::fs::read_to_string(e.dir.path().join("effects.txt")).unwrap_or_default();
    assert!(!effects.lines().any(|l| l.starts_with("deploy ")), "{effects}");
    let audit = e.run(&["audit", &id]);
    assert!(audit.as_array().unwrap().iter().any(|r| r["action"] == "deploy" && r["decision"] == "blocked"), "{audit}");
}

#[test]
fn a_hung_server_is_killed_with_its_children_and_reported() {
    let e = env_with(|e| cfg(vec![source(e, "tickets", &["--mode", "hang"], json!({"timeout_ms": 600}))], json!([{"source": "tickets", "channel": "PRJ", "workflows": ["demo"]}]), &[]));
    e.approve("tickets");
    let t = Instant::now();
    let err = e.poll("tickets").unwrap_err();
    assert!(t.elapsed() < Duration::from_secs(20));
    assert!(err.contains("timed out") && err.contains("tickets"), "{err}");
    let calls = e.calls();
    let pid = calls.iter().find(|c| c["event"] == "start").unwrap()["pid"].as_u64().unwrap();
    let grandchild = calls.iter().find(|c| c["event"] == "hang").unwrap()["grandchild"].as_u64().unwrap();
    assert!(wait_dead(pid), "server {pid} still alive");
    assert!(wait_dead(grandchild), "its child {grandchild} still alive");
    assert!(e.items("queued").is_empty());
}

#[test]
fn an_is_error_result_is_reported_and_nothing_is_queued() {
    let e = env_with(|e| cfg(vec![source(e, "tickets", &["--mode", "error"], json!({}))], json!([{"source": "tickets", "channel": "PRJ", "workflows": ["demo"]}]), &[]));
    e.approve("tickets");
    let err = e.poll("tickets").unwrap_err();
    assert!(err.contains("isError") && err.contains("boom: upstream returned 500"), "{err}");
    assert!(e.run(&["intake", "list"]).as_array().unwrap().is_empty());
    let bad = env_with(|e| cfg(vec![source(e, "tickets", &["--mode", "bad-json"], json!({}))], json!([]), &[]));
    bad.approve("tickets");
    assert!(bad.poll("tickets").unwrap_err().contains("not JSON"));
    // A server that is not there at all.
    let gone = env_with(|e| cfg(vec![source(e, "tickets", &[], json!({"command": ["/nonexistent/mcp-server"]}))], json!([]), &[]));
    gone.approve("tickets");
    assert!(gone.poll("tickets").unwrap_err().contains("cannot start MCP server"));
    // The tool is not offered.
    let wrong = env_with(|e| cfg(vec![source(e, "tickets", &[], json!({"tool": "missing_tool"}))], json!([]), &[]));
    wrong.approve("tickets");
    assert!(wrong.poll("tickets").unwrap_err().contains("does not offer tool \"missing_tool\""));
}

#[test]
fn literal_secrets_in_the_config_are_refused_at_load() {
    for (what, over) in [
        ("env token", json!({"env": {"TOKEN": concat!("xo", "xb-1234567890-abcdefghijklmnop")}})),
        ("env literal", json!({"env": {"MODE": "production"}})),
        ("arguments token", json!({"arguments": {"api_key": concat!("gh", "p_abcdefghijklmnopqrstuvwxyz0123456789")}})),
        ("command token", json!({"command": ["srv", "--token", concat!("lin", "_api_abcdefghijklmnopqrstuvwxyz")]})),
    ] {
        let e = env_with(|e| cfg(vec![source(e, "tickets", &[], over.clone())], json!([]), &[]));
        let err = e.try_run(&["intake", "connectors", "list"]).unwrap_err();
        assert!(err.contains("source tickets"), "{what}: {err}");
        assert!(!err.contains("abcdefghijklmnop"), "{what}: the error must not echo the secret: {err}");
    }
}

#[test]
fn permission_and_rate_rules_are_keyed_by_source_name() {
    let e = env_with(|e| {
        let mut c = cfg(
            vec![source(e, "alpha", &[], json!({})), source(e, "beta", &[], json!({}))],
            json!([{"source": "alpha", "channel": "PRJ", "workflows": ["demo"]}]),
            &[],
        );
        c["rate"]["per_source"] = json!({"alpha": 1});
        c
    });
    e.approve("alpha");
    e.approve("beta");
    e.poll("alpha").unwrap();
    e.poll("beta").unwrap();
    let (queued, limited, rejected) = (e.items("queued"), e.items("rate_limited"), e.items("rejected"));
    assert_eq!((queued.len(), limited.len(), rejected.len()), (1, 1, 2), "{queued:?} {limited:?} {rejected:?}");
    assert!(queued.iter().chain(&limited).all(|i| i["source"] == "alpha"));
    assert!(rejected.iter().all(|i| i["source"] == "beta" && i["reason"].as_str().unwrap().contains("beta")));
    // A rule that names a source that is not configured is a config error in version 2.
    let typo = env_with(|e| cfg(vec![source(e, "alpha", &[], json!({}))], json!([{"source": "alpah", "channel": "*", "workflows": ["demo"]}]), &[]));
    assert!(typo.try_run(&["intake", "connectors", "list"]).unwrap_err().contains("unknown source"));
}

#[test]
fn several_slack_and_github_sources_and_version_rules() {
    let slack = |n: &str| json!({"name": n, "kind": "slack", "signing_secret_env": format!("SF_{}_SECRET", n.to_uppercase().replace(['-', ':'], "_")), "bot_user_id": "U0"});
    let e = env_with(|_| {
        cfg(vec![slack("slack-eng"), slack("slack-ops"), json!({"name": "gh-shop", "kind": "github", "webhook_secret_env": "SF_GH_SHOP", "poll": {"repos": ["acme/shop"]}})],
            json!([{"source": "slack-eng", "channel": "C1", "workflows": ["demo"]}, {"source": "gh-shop", "channel": "acme/shop", "workflows": ["ship"]}]), &["ship"])
    });
    let list = e.run(&["intake", "connectors", "list"]);
    let rows: Vec<_> = list.as_array().unwrap().iter().map(|r| (r["name"].as_str().unwrap(), r["kind"].as_str().unwrap(), r["approval"].as_str().unwrap(), r["chat"].as_bool().unwrap())).collect();
    assert_eq!(rows, [("slack-eng", "slack", "built-in", true), ("slack-ops", "slack", "built-in", true), ("gh-shop", "github", "built-in", false)]);
    // A chat (slack-kind) source named in a deploy rule fails, whatever its name.
    let bad = env_with(|_| cfg(vec![slack("slack-eng")], json!([{"source": "slack-eng", "channel": "C1", "workflows": ["ship"]}]), &["ship"]));
    assert!(bad.try_run(&["intake", "connectors", "list"]).unwrap_err().contains("chat source slack-eng"));
    // Duplicate names (any case), bad names, unknown versions, v1 shapes in v2.
    let dup = env_with(|_| cfg(vec![slack("a"), slack("A")], json!([]), &[]));
    assert!(dup.try_run(&["intake", "connectors", "list"]).unwrap_err().contains("duplicate source name"));
    let badname = env_with(|_| cfg(vec![slack("has:colon")], json!([]), &[]));
    assert!(badname.try_run(&["intake", "connectors", "list"]).unwrap_err().contains("source name"));
    let v3 = env_with(|_| json!({"version": 3, "permissions": {"rules": []}}));
    assert!(v3.try_run(&["intake", "connectors", "list"]).unwrap_err().contains("unsupported config version 3"));
    let mixed = env_with(|_| json!({"version": 2, "github": {"webhook_secret_env": "X"}, "permissions": {"rules": []}}));
    assert!(mixed.try_run(&["intake", "connectors", "list"]).is_err());
}

#[test]
fn version_1_files_still_load_as_sources_named_slack_and_github() {
    let e = env_with(|_| json!({
        "slack": {"signing_secret_env": "SF_SLACK_SIGNING_SECRET", "bot_user_id": "U0BOT"},
        "github": {"webhook_secret_env": "SF_GITHUB_WEBHOOK_SECRET"},
        "permissions": {"deploy_workflows": ["ship"], "rules": [{"source": "github", "channel": "acme/shop", "workflows": ["ship"]}]}
    }));
    let list = e.run(&["intake", "connectors", "list"]);
    let names: Vec<_> = list.as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["slack", "github"]);
    // v1 chat rule for deploy still refused by name.
    let bad = env_with(|_| json!({"slack": {"signing_secret_env": "X", "bot_user_id": "U"}, "permissions": {"deploy_workflows": ["ship"], "rules": [{"source": "slack", "channel": "C", "workflows": ["ship"]}]}}));
    assert!(bad.try_run(&["intake", "connectors", "list"]).is_err());
    // And the old poll shorthand still works.
    let f = e.dir.path().join("issues.json");
    std::fs::write(&f, json!([{"number": 1, "title": "t", "body": "b", "user": {"login": "u"}, "labels": [{"name": "factory:ship"}], "updated_at": "2025-01-01T00:00:00Z"}]).to_string()).unwrap();
    let out = e.run(&["intake", "poll", "github", "--repo", "acme/shop", "--from-file", f.to_str().unwrap()]);
    assert_eq!(out["items"][0]["status"], "queued", "{out}");
}

#[test]
fn the_example_config_lists_its_sources_with_the_mcp_one_pending() {
    let example: &Path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/intake.example.json"));
    let e = default_env();
    let o = e.cmd(&["intake", "connectors", "list", "--config", example.to_str().unwrap()]).output().unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(o.status.success(), "{v}");
    let mcp = v.as_array().unwrap().iter().find(|r| r["kind"] == "mcp").unwrap();
    assert_eq!((mcp["enabled"].as_bool(), mcp["approval"].as_str()), (Some(false), Some("pending")), "{v}");
    assert!(v.as_array().unwrap().iter().any(|r| r["kind"] == "slack") && v.as_array().unwrap().iter().any(|r| r["kind"] == "github"));
}

#[test]
fn webhooks_route_to_the_named_slack_source_with_its_own_secret_and_name() {
    use std::io::{Read, Write};
    let slack = |n: &str, var: &str| json!({"name": n, "kind": "slack", "signing_secret_env": var, "bot_user_id": "U0BOT"});
    let e = env_with(|_| {
        cfg(vec![slack("slack-eng", "SF_T_ENG"), slack("slack-ops", "SF_T_OPS")],
            json!([{"source": "slack-ops", "channel": "C1", "workflows": ["demo"]}]), &[])
    });
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let addr = format!("127.0.0.1:{port}");
    let mut child = e.cmd(&["intake", "serve", "--addr", &addr, "--config", e.config.to_str().unwrap()])
        .env("SF_T_ENG", "eng-secret").env("SF_T_OPS", "ops-secret").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let post = |path: &str, secret: &str| -> String {
        let body = json!({"type": "event_callback", "event_id": "Ev9", "event": {"type": "app_mention", "user": "U1", "channel": "C1", "ts": "1.1", "text": "<@U0BOT> demo run it"}}).to_string();
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs().to_string();
        let sig = sf_intake::sig::hmac_sha256_hex(secret.as_bytes(), format!("v0:{ts}:{body}").as_bytes());
        let req = format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nX-Slack-Request-Timestamp: {ts}\r\nX-Slack-Signature: v0={sig}\r\n\r\n{body}", body.len());
        let mut s = loop {
            match std::net::TcpStream::connect(&addr) {
                Ok(s) => break s,
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    };
    // Two sources and none named `slack`: the bare path is ambiguous.
    assert!(post("/slack/events", "ops-secret").starts_with("HTTP/1.1 404"));
    assert!(post("/slack/slack-ops/events", "eng-secret").starts_with("HTTP/1.1 401"), "each source has its own secret");
    let ok = post("/slack/slack-ops/events", "ops-secret");
    assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("queued"), "{ok}");
    let _ = child.kill();
    let _ = child.wait();
    let q = e.items("queued");
    assert_eq!((q.len(), q[0]["source"].as_str(), q[0]["dedupe_key"].as_str()), (1, Some("slack-ops"), Some("slack-ops:Ev9")));
}

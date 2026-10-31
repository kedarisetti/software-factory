//! Milestone 5 exit tests: the real `sf` binary serves signed webhooks (built from the presets in
//! docs/presets/) into the work queue (dedupe, rate limit, permission table), dispatches queued items
//! as runs, and the daemon drives the whole loop.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sf_core::{Queue, Store};
use sf_intake::sig::hmac_sha256_hex;
use sf_store_sqlite::SqliteStore;

const SLACK_SECRET: &str = "slack-test-secret";
const GH_SECRET: &str = "github-test-secret";
const REPO: &str = "kedarisetti/software-factory";

fn preset(file: &str) -> Value {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/presets/");
    serde_json::from_str(&std::fs::read_to_string(format!("{p}{file}")).unwrap()).unwrap()
}

/// The chat webhook preset, named `slack` here (a plain name; chat-ness comes from `chat: true`).
fn chat_source() -> Value {
    let t = serde_json::to_string(&preset("slack-events.json"))
        .unwrap()
        .replace("U0FACTORY", "U0BOT")
        .replace("C0FACTORY", "C0DEMO");
    let mut v: Value = serde_json::from_str(&t).unwrap();
    v["name"] = json!("slack");
    v["verify"]["secret_env"] = json!("SF_SLACK_SIGNING_SECRET");
    v
}

fn hook_source() -> Value {
    let mut v = preset("github-issues-webhook.json");
    v["name"] = json!("github");
    v["verify"]["secret_env"] = json!("SF_GITHUB_WEBHOOK_SECRET");
    v
}

/// The poll preset as a `command` source that just `cat`s a file, so tests control the "API" output.
fn poll_source(file: &str) -> Value {
    let mut v = preset("github-issues-poll.json");
    v["name"] = json!("gh-poll");
    v["command"] = json!(["cat", file]);
    v["env"] = json!({});
    v["poll_interval_s"] = json!(1);
    v
}

struct Env {
    _dir: tempfile::TempDir,
    db: PathBuf,
    sandbox: PathBuf,
    config: PathBuf,
    effects: PathBuf,
    issues: PathBuf,
    server: Option<(Child, String)>,
}

impl Drop for Env {
    fn drop(&mut self) {
        self.stop();
    }
}

fn config(
    deploy: &[&str],
    slack_wf: &[&str],
    per_actor: u64,
    daily_tokens: u64,
    issues: &str,
) -> Value {
    json!({
        "version": 2,
        "sources": [chat_source(), hook_source(), poll_source(issues)],
        "permissions": {"deploy_workflows": deploy, "rules": [
            {"source": "slack", "channel": "C0DEMO", "workflows": slack_wf},
            {"source": "github", "channel": REPO, "workflows": ["ship", "demo", "triage"]},
            {"source": "github", "channel": "acme/shop", "workflows": ["demo"]},
            {"source": "gh-poll", "channel": REPO, "workflows": ["demo", "triage"]}
        ]},
        "rate": {"per_actor": per_actor},
        "priority": {"base": {"github": 10, "slack": 5, "gh-poll": 10}, "rules": [
            {"name": "urgent", "labels": ["urgent"], "keywords": ["urgent"], "score": 50}]},
        "budget": {"daily_tokens": daily_tokens}
    })
}

fn env_with(build: impl Fn(&str) -> Value) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().to_path_buf();
    let issues = p.join("issues.json");
    std::fs::write(&issues, "[]").unwrap();
    std::fs::write(
        p.join("intake.json"),
        build(issues.to_str().unwrap()).to_string(),
    )
    .unwrap();
    Env {
        db: p.join("sf.db"),
        sandbox: p.join("sbx"),
        config: p.join("intake.json"),
        effects: p.join("effects.txt"),
        issues,
        _dir: dir,
        server: None,
    }
}

fn env() -> Env {
    env_with(|i| config(&["ship"], &["demo"], 4, 1_000_000, i))
}

fn cmd(e: &Env, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_sf"));
    c.arg("--db")
        .arg(&e.db)
        .arg("--sandbox")
        .arg(&e.sandbox)
        .args(args);
    c.env("SF_SHIP_EFFECT_FILE", &e.effects)
        .env_remove("SF_SHIP_CRASH_AFTER_WRITE")
        .env_remove("SF_DEMO_SIDE_EFFECT_FILE");
    c.env("SF_DEMO_SLEEP_MS", "0");
    c
}

fn json_out(e: &Env, args: &[&str]) -> Value {
    let o = cmd(e, args).output().unwrap();
    assert!(
        o.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&o.stdout)
    );
    serde_json::from_slice(&o.stdout).unwrap()
}

fn intake(e: &Env, args: &[&str]) -> Value {
    let cfg = e.config.to_str().unwrap();
    let mut a = vec!["intake"];
    a.extend_from_slice(args);
    a.extend_from_slice(&["--config", cfg]);
    json_out(e, &a)
}

fn items(e: &Env, status: &str) -> Vec<Value> {
    json_out(e, &["intake", "list", "--status", status])
        .as_array()
        .unwrap()
        .clone()
}

fn total(e: &Env) -> usize {
    json_out(e, &["intake", "list"]).as_array().unwrap().len()
}

impl Env {
    fn start(&mut self) {
        self.spawn(&["serve"]);
    }

    fn spawn(&mut self, sub: &[&str]) {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let addr = format!("127.0.0.1:{port}");
        let cfg = self.config.to_str().unwrap().to_string();
        let mut args = vec!["intake"];
        args.extend_from_slice(sub);
        args.extend_from_slice(&["--addr", &addr, "--config", &cfg]);
        let child = cmd(self, &args)
            .env("SF_SLACK_SIGNING_SECRET", SLACK_SECRET)
            .env("SF_GITHUB_WEBHOOK_SECRET", GH_SECRET)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.server = Some((child, addr.clone()));
        let t = Instant::now();
        while http(&addr, "GET", "/healthz", &[], b"").0 != 200 {
            assert!(
                t.elapsed() < Duration::from_secs(20),
                "server never came up"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stop(&mut self) {
        if let Some((mut c, _)) = self.server.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn addr(&self) -> String {
        self.server.as_ref().unwrap().1.clone()
    }
}

/// One request, one response: (status, body).
fn http(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> (u16, String) {
    let Ok(mut s) = TcpStream::connect(addr) else {
        return (0, String::new());
    };
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req += &format!("{k}: {v}\r\n");
    }
    req += "\r\n";
    if s.write_all(req.as_bytes())
        .and_then(|_| s.write_all(body))
        .is_err()
    {
        return (0, String::new());
    }
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (
        status,
        text.split("\r\n\r\n").nth(1).unwrap_or("").to_string(),
    )
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Signed Slack delivery; `ts_offset` skews the timestamp, `secret` can be wrong, `retry` marks a Slack retry.
fn slack_post(addr: &str, body: String, ts_offset: i64, secret: &str, retry: bool) -> (u16, Value) {
    let ts = (now_secs() + ts_offset).to_string();
    let sig = format!(
        "v0={}",
        hmac_sha256_hex(secret.as_bytes(), format!("v0:{ts}:{body}").as_bytes())
    );
    let mut h = vec![
        ("X-Slack-Request-Timestamp".to_string(), ts),
        ("X-Slack-Signature".into(), sig),
    ];
    if retry {
        h.push(("X-Slack-Retry-Num".into(), "1".into()));
    }
    let (st, resp) = http(addr, "POST", "/hooks/slack", &h, body.as_bytes());
    (st, serde_json::from_str(&resp).unwrap_or(Value::Null))
}

fn mention(user: &str, event_id: &str, text: &str) -> String {
    json!({"type": "event_callback", "event_id": event_id, "event": {
        "type": "app_mention", "user": user, "text": text, "channel": "C0DEMO", "ts": format!("1712345678.{event_id}")}})
    .to_string()
}

fn slack(e: &Env, user: &str, event_id: &str, text: &str) -> (u16, Value) {
    slack_post(
        &e.addr(),
        mention(user, event_id, text),
        0,
        SLACK_SECRET,
        false,
    )
}

fn github(
    e: &Env,
    repo: &str,
    number: u32,
    delivery: &str,
    secret: &str,
    title: &str,
) -> (u16, Value) {
    let body = json!({"action": "opened", "repository": {"full_name": repo}, "sender": {"login": "octocat"},
        "issue": {"number": number, "title": title, "body": format!("details {number}"), "user": {"login": "octocat"},
                  "labels": [{"name": "factory:ship"}]}})
    .to_string();
    let sig = format!(
        "sha256={}",
        hmac_sha256_hex(secret.as_bytes(), body.as_bytes())
    );
    let h = vec![
        ("X-GitHub-Event".to_string(), "issues".to_string()),
        ("X-GitHub-Delivery".into(), delivery.into()),
        ("X-Hub-Signature-256".into(), sig),
    ];
    let (st, resp) = http(&e.addr(), "POST", "/hooks/github", &h, body.as_bytes());
    (st, serde_json::from_str(&resp).unwrap_or(Value::Null))
}

#[test]
fn chat_webhook_events_are_verified_deduped_permissioned_and_rate_limited() {
    let mut e = env();
    e.start();

    let (st, r) = slack(
        &e,
        "U1",
        "Ev1",
        "<@U0BOT> demo please run the nightly report",
    );
    assert_eq!((st, r["status"].as_str()), (200, Some("queued")), "{r}");
    assert_eq!(items(&e, "queued").len(), 1);

    // Slack retry of the same event: same item, nothing new.
    let body = mention("U1", "Ev1", "<@U0BOT> demo please run the nightly report");
    let (st, r) = slack_post(&e.addr(), body, 0, SLACK_SECRET, true);
    assert_eq!((st, r["inserted"].as_bool()), (200, Some(false)), "{r}");
    assert_eq!(total(&e), 1);

    // Bad signature and stale timestamp: 401, nothing stored.
    let (st, _) = slack_post(
        &e.addr(),
        mention("U1", "Ev2", "<@U0BOT> demo forged"),
        0,
        "wrong-secret",
        false,
    );
    assert_eq!(st, 401);
    let (st, _) = slack_post(
        &e.addr(),
        mention("U1", "Ev3", "<@U0BOT> demo replayed"),
        -1000,
        SLACK_SECRET,
        false,
    );
    assert_eq!(st, 401);
    assert_eq!(total(&e), 1);

    // Chat asks for a deploy workflow: stored as Rejected by the permission table, never queued.
    let (st, r) = slack(&e, "U1", "Ev4", "<@U0BOT> ship the release to prod");
    assert_eq!((st, r["status"].as_str()), (200, Some("rejected")), "{r}");
    let rejected = items(&e, "rejected");
    assert_eq!(rejected.len(), 1);
    assert!(
        rejected[0]["reason"].as_str().unwrap().contains("deploy"),
        "{rejected:?}"
    );
    assert_eq!(items(&e, "queued").len(), 1);

    // Near-duplicate (same actor, same words, new event id): no new item.
    let (st, r) = slack(
        &e,
        "U1",
        "Ev5",
        "<@U0BOT> demo  Please run the nightly report!",
    );
    assert_eq!((st, r["inserted"].as_bool()), (200, Some(false)), "{r}");
    assert_eq!(total(&e), 2);

    // One actor floods (limit 4 per window): the rest are kept as RateLimited.
    for n in 0..8 {
        slack(
            &e,
            "U2",
            &format!("EvF{n}"),
            &format!("<@U0BOT> demo job number {n} of the flood"),
        );
    }
    assert_eq!(
        items(&e, "queued").len(),
        5,
        "1 earlier + 4 from the flooder"
    );
    assert_eq!(items(&e, "rate_limited").len(), 4);

    // Unconfigured routes and oversized bodies are refused quickly.
    assert_eq!(http(&e.addr(), "GET", "/nope", &[], b"").0, 404);
    assert_eq!(
        http(&e.addr(), "POST", "/hooks/nope", &[], b"{}").0,
        404,
        "unknown source"
    );
    assert_eq!(
        http(&e.addr(), "POST", "/hooks/gh-poll", &[], b"{}").0,
        404,
        "a command source has no webhook"
    );
    assert_eq!(
        http(&e.addr(), "POST", "/slack/events", &[], b"{}").0,
        404,
        "no vendor routes"
    );
    assert_eq!(http(&e.addr(), "GET", "/hooks/slack", &[], b"").0, 405);
    // The declared length alone is enough to refuse; the server never reads the body.
    let mut s = TcpStream::connect(e.addr()).unwrap();
    s.write_all(b"POST /hooks/slack HTTP/1.1\r\nContent-Length: 2097152\r\n\r\n")
        .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 413"), "{raw}");
}

#[test]
fn issue_webhooks_are_verified_deduped_and_permissioned() {
    let mut e = env();
    e.start();
    let (st, r) = github(&e, REPO, 7, "d-1", GH_SECRET, "Ship the thing");
    assert_eq!((st, r["status"].as_str()), (200, Some("queued")), "{r}");
    let (st, r) = github(&e, REPO, 7, "d-1", GH_SECRET, "Ship the thing");
    assert_eq!(
        (st, r["inserted"].as_bool()),
        (200, Some(false)),
        "redelivery: {r}"
    );
    assert_eq!(total(&e), 1);

    assert_eq!(github(&e, REPO, 8, "d-2", "wrong", "Forged").0, 401);
    assert_eq!(total(&e), 1);

    // Repo with no rule for `ship`: stored as Rejected.
    let (st, r) = github(&e, "acme/shop", 9, "d-3", GH_SECRET, "Not allowed here");
    assert_eq!((st, r["status"].as_str()), (200, Some("rejected")), "{r}");
    assert_eq!(
        (items(&e, "queued").len(), items(&e, "rejected").len()),
        (1, 1)
    );
}

#[test]
fn dispatch_starts_runs_in_priority_order_with_trigger_and_input() {
    let mut e = env();
    e.start();
    slack(&e, "U1", "EvA", "<@U0BOT> demo plain job");
    slack(&e, "U3", "EvB", "<@U0BOT> demo urgent job");
    github(&e, REPO, 11, "d-11", GH_SECRET, "Ship it");
    e.stop();

    let out = intake(&e, &["dispatch", "--max", "10"]);
    assert!(out["paused"].is_null(), "{out}");
    let started = out["started"].as_array().unwrap();
    let order: Vec<_> = started
        .iter()
        .map(|s| s["workflow"].as_str().unwrap())
        .collect();
    assert_eq!(order.len(), 3, "{out}");
    assert_eq!(
        order,
        ["demo", "ship", "demo"],
        "urgent chat (55), issue (10), plain chat (5): bases come from priority.base"
    );

    let store = SqliteStore::open(&e.db).unwrap();
    let started_items = items(&e, "started");
    assert_eq!(started_items.len(), 3);
    assert!(items(&e, "queued").is_empty());
    for s in started {
        let run = store
            .get_run(s["run_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
        let item = store
            .get_item(s["item_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
        let prefix = if item.source == "slack" { "chat:" } else { "" }; // `chat: true` sources run under chat:<source>:<ref>
        assert_eq!(
            run.trigger_ref,
            format!("{prefix}{}:{}", item.source, item.source_ref)
        );
        assert_eq!(item.run_id.as_deref(), Some(run.run_id.as_str()));
        let input = store
            .delivery(&format!("input:{}", run.run_id))
            .unwrap()
            .unwrap();
        assert_eq!(input["item_id"], item.item_id);
        assert_eq!(input["payload"]["text"], item.payload["text"]);
        assert_eq!(input["payload"]["untrusted"], true);
    }
    // A second dispatch has nothing left to start.
    assert!(intake(&e, &["dispatch"])["started"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn a_chat_run_that_tries_to_deploy_is_forbidden_and_escalated() {
    // Misconfigured on purpose: `ship` is not declared a deploy workflow, so the table lets Slack
    // start it. The engine's own chat rule is the second wall (SBX-3).
    let mut e = env_with(|i| config(&[], &["ship"], 10, 1_000_000, i));
    e.start();
    let (_, r) = slack(&e, "U1", "EvS", "<@U0BOT> ship the release");
    assert_eq!(r["status"], "queued", "{r}");
    e.stop();
    let id = intake(&e, &["dispatch"])["started"][0]["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut state = String::new();
    for _ in 0..8 {
        json_out(&e, &["run"]);
        state = json_out(&e, &["status", &id])["state"]
            .as_str()
            .unwrap()
            .to_string();
        if state != "AwaitingApproval" {
            break;
        }
        json_out(&e, &["approve", &id, "--as", "alice"]);
    }
    let s = json_out(&e, &["status", &id]);
    assert_eq!(state, "Escalated", "{s}");
    assert!(
        s["escalation"]
            .as_str()
            .unwrap()
            .contains("deploy from chat trigger chat:slack:"),
        "{s}"
    );
    let effects = std::fs::read_to_string(&e.effects).unwrap_or_default();
    assert!(
        !effects.lines().any(|l| l.starts_with("deploy ")),
        "deploy must not run: {effects}"
    );
    let audit = json_out(&e, &["audit", &id]);
    assert!(
        audit
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["action"] == "deploy" && r["decision"] == "blocked"),
        "{audit}"
    );
}

#[test]
fn daily_budget_pauses_dispatch_not_running_work() {
    let mut e = env_with(|i| config(&["ship"], &["demo"], 10, 1000, i));
    e.start();
    slack(&e, "U1", "EvX", "<@U0BOT> demo first job");
    slack(&e, "U2", "EvY", "<@U0BOT> demo second job");
    e.stop();

    let first = intake(&e, &["dispatch", "--max", "1"]);
    let id = first["started"][0]["run_id"].as_str().unwrap().to_string();
    json_out(&e, &["run"]); // demo charges 1200 tokens: over the 1000 daily budget
    let before = json_out(&e, &["status", &id]);
    assert!(before["spent_tokens"].as_u64().unwrap() >= 1200, "{before}");

    let second = intake(&e, &["dispatch"]);
    assert!(second["started"].as_array().unwrap().is_empty(), "{second}");
    assert_eq!(second["paused"]["reason"], "paused: daily budget");
    assert_eq!(items(&e, "queued").len(), 1, "the second item waits");
    assert_eq!(
        json_out(&e, &["status", &id]),
        before,
        "running work is untouched"
    );
}

fn issue(number: u32, title: &str, updated: &str, label: &str) -> Value {
    json!({"number": number, "title": title, "body": format!("body {number}"), "user": {"login": format!("user{number}")},
           "labels": [{"name": label}], "updated_at": updated, "state": "open"})
}

/// What the `cat` command source prints on its next poll.
fn set_issues(e: &Env, v: Value) {
    std::fs::write(&e.issues, v.to_string()).unwrap();
}

fn approve_poll(e: &Env) {
    let ap = intake(e, &["connectors", "approve", "gh-poll", "--as", "alice"]);
    assert_eq!(ap["approval"], "approved");
}

fn poll(e: &Env) -> Value {
    intake(e, &["poll", "--source", "gh-poll", "--now"])["polled"][0].clone()
}

#[test]
fn polling_a_command_source_twice_adds_no_duplicates_and_keeps_the_cursor() {
    let e = env();
    let mut pr = issue(3, "A pull request", "2025-01-02T00:00:00Z", "factory:demo");
    pr["pull_request"] = json!({"url": "x"});
    set_issues(
        &e,
        json!([
            issue(1, "Add a retry", "2025-01-01T10:00:00Z", "factory:demo"),
            issue(2, "Unlabelled workflow", "2025-01-01T11:00:00Z", "question"),
            pr,
        ]),
    );
    // A command source runs a process, so it is refused until a human approves it.
    let o = cmd(
        &e,
        &[
            "intake",
            "poll",
            "--source",
            "gh-poll",
            "--now",
            "--config",
            e.config.to_str().unwrap(),
        ],
    )
    .output()
    .unwrap();
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("disabled"),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    approve_poll(&e);

    let first = poll(&e);
    assert_eq!(
        (first["considered"].as_u64(), first["ignored"].as_u64()),
        (Some(1), Some(2)),
        "{first}"
    );
    assert_eq!(items(&e, "queued").len(), 1);
    assert_eq!(first["cursor"], "2025-01-01T10:00:00Z");
    let second = poll(&e);
    assert_eq!(second["items"][0]["inserted"], false, "{second}");
    assert_eq!(total(&e), 1);

    // The same issue updated later is re-read but is the same item; the cursor moves forward only.
    set_issues(
        &e,
        json!([issue(
            1,
            "Add a retry",
            "2025-02-01T10:00:00Z",
            "factory:demo"
        )]),
    );
    let third = poll(&e);
    assert_eq!(
        (
            third["considered"].as_u64(),
            third["items"][0]["inserted"].as_bool()
        ),
        (Some(1), Some(false)),
        "{third}"
    );
    assert_eq!(third["cursor"], "2025-02-01T10:00:00Z");
    assert_eq!(total(&e), 1);
    set_issues(
        &e,
        json!([issue(
            1,
            "Add a retry",
            "2025-01-15T10:00:00Z",
            "factory:demo"
        )]),
    );
    assert_eq!(
        poll(&e)["cursor"],
        "2025-02-01T10:00:00Z",
        "never backwards"
    );
}

#[test]
fn bottlenecks_from_a_seeded_cluster_make_one_idempotent_backlog_item() {
    let e = env();
    set_issues(
        &e,
        json!([
            issue(
                1,
                "Checkout total wrong",
                "2025-01-01T10:00:00Z",
                "factory:demo"
            ),
            issue(
                2,
                "Checkout total wrong",
                "2025-01-01T11:00:00Z",
                "factory:demo"
            ),
            issue(
                3,
                "Checkout total wrong",
                "2025-01-01T12:00:00Z",
                "factory:demo"
            ),
            issue(4, "Unrelated thing", "2025-01-01T13:00:00Z", "factory:demo"),
        ]),
    );
    approve_poll(&e);
    poll(&e);
    let first = intake(&e, &["bottlenecks", "--window-days", "7", "--min", "3"]);
    assert_eq!(first.as_array().unwrap().len(), 1, "{first}");
    assert_eq!(
        (first[0]["inserted"].as_bool(), first[0]["count"].as_u64()),
        (Some(true), Some(3))
    );
    let again = intake(&e, &["bottlenecks", "--window-days", "7", "--min", "3"]);
    assert_eq!(again[0]["inserted"], false, "{again}");
    let backlog = items(&e, "backlog");
    assert_eq!(backlog.len(), 1);
    assert_eq!(
        backlog[0]["payload"]["evidence"].as_array().unwrap().len(),
        3
    );
}

#[test]
fn the_example_config_and_every_preset_load_and_missing_secrets_are_named_not_printed() {
    let e = env();
    let example = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/intake.example.json"
    );
    let out = json_out(&e, &["intake", "dispatch", "--config", example]);
    assert!(out["started"].as_array().unwrap().is_empty(), "{out}");
    let list = json_out(&e, &["intake", "connectors", "list", "--config", example]);
    let rows: Vec<_> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap(),
                r["kind"].as_str().unwrap(),
                r["approval"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("team-chat", "webhook", "not required"),
            ("gh-issues-hook", "webhook", "not required"),
            ("gh-issues", "command", "pending"),
            ("linear", "mcp", "pending")
        ]
    );
    let o = cmd(
        &e,
        &[
            "intake",
            "serve",
            "--addr",
            "127.0.0.1:0",
            "--config",
            example,
        ],
    )
    .env_remove("SF_CHAT_SIGNING_SECRET")
    .output()
    .unwrap();
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("SF_CHAT_SIGNING_SECRET is not set"));

    // Each preset is a source object that loads on its own (the permission rule names it).
    let dir =
        std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/presets")).unwrap();
    let mut n = 0;
    for f in dir {
        let src: Value =
            serde_json::from_str(&std::fs::read_to_string(f.unwrap().path()).unwrap()).unwrap();
        let name = src["name"].as_str().unwrap().to_string();
        let cfg = json!({"version": 2, "sources": [src], "permissions": {"rules": [{"source": name, "channel": "*", "workflows": ["demo"]}]}});
        let p = e._dir.path().join(format!("preset-{name}.json"));
        std::fs::write(&p, cfg.to_string()).unwrap();
        let l = json_out(
            &e,
            &[
                "intake",
                "connectors",
                "list",
                "--config",
                p.to_str().unwrap(),
            ],
        );
        assert_eq!(l[0]["name"], name.as_str(), "{l}");
        n += 1;
    }
    assert_eq!(n, 4);
}

impl Env {
    /// Waits for a spawned daemon (started with `--ticks N`) to finish on its own.
    fn wait_daemon(&mut self) {
        let (mut c, _) = self.server.take().unwrap();
        let t = Instant::now();
        while c.try_wait().unwrap().is_none() {
            assert!(
                t.elapsed() < Duration::from_secs(60),
                "daemon did not stop after its ticks"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(c.wait().unwrap().success());
    }

    fn run_state(&self, run_id: &str) -> String {
        json_out(self, &["status", run_id])["state"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

#[test]
fn the_daemon_takes_a_webhook_to_a_finished_run() {
    let mut e = env_with(|i| config(&["ship"], &["demo", "triage"], 10, 1_000_000, i));
    e.spawn(&["daemon", "--tick-s", "1", "--ticks", "8"]);
    let (st, r) = slack(&e, "U1", "EvD", "<@U0BOT> triage the flaky test");
    assert_eq!((st, r["status"].as_str()), (200, Some("queued")), "{r}");
    e.wait_daemon();
    let started = items(&e, "started");
    assert_eq!(started.len(), 1, "{started:?}");
    let run_id = started[0]["run_id"].as_str().unwrap();
    assert_eq!(e.run_state(run_id), "Succeeded");
    let store = SqliteStore::open(&e.db).unwrap();
    assert!(store
        .get_run(run_id)
        .unwrap()
        .unwrap()
        .trigger_ref
        .starts_with("chat:slack:"));
}

#[test]
fn the_daemon_polls_dispatches_and_runs_without_a_server() {
    let e = env();
    set_issues(
        &e,
        json!([issue(
            5,
            "Look at this",
            "2025-01-01T10:00:00Z",
            "factory:triage"
        )]),
    );
    approve_poll(&e);
    // Unapproved sources would be logged and skipped; this one is approved. No --addr, so no secrets needed.
    let o = cmd(
        &e,
        &[
            "intake",
            "daemon",
            "--tick-s",
            "1",
            "--ticks",
            "2",
            "--config",
            e.config.to_str().unwrap(),
        ],
    )
    .output()
    .unwrap();
    assert!(o.status.success());
    // One JSON line per tick that did something, then the summary.
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        (lines[0]["polled"].as_u64(), lines[0]["dispatched"].as_u64()),
        (Some(1), Some(1)),
        "{text}"
    );
    assert_eq!(lines.last().unwrap()["ticks"], 2);
    let started = items(&e, "started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(
        e.run_state(started[0]["run_id"].as_str().unwrap()),
        "Succeeded"
    );
}

#[test]
fn the_daemon_logs_a_blocked_source_and_keeps_going() {
    let e = env();
    set_issues(
        &e,
        json!([issue(
            5,
            "Look at this",
            "2025-01-01T10:00:00Z",
            "factory:triage"
        )]),
    );
    let o = cmd(
        &e,
        &[
            "intake",
            "daemon",
            "--tick-s",
            "1",
            "--ticks",
            "1",
            "--config",
            e.config.to_str().unwrap(),
        ],
    )
    .output()
    .unwrap();
    assert!(o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("poll_error"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(total(&e), 0);
}

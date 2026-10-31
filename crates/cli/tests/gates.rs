//! Milestone 3 exit test A: every gated action parks and resumes on approval, through the real
//! `sf` binary across processes, with a real SQLite file and real process death.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const GATED: [&str; 4] = ["merge_protected", "deploy", "external_message", "new_capability"];

struct Env {
    _dir: tempfile::TempDir,
    db: PathBuf,
    sandbox: PathBuf,
    effects: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().to_path_buf();
    Env { db: p.join("sf.db"), sandbox: p.join("sbx"), effects: p.join("effects.txt"), _dir: dir }
}

fn cmd(e: &Env, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_sf"));
    c.arg("--db").arg(&e.db).arg("--sandbox").arg(&e.sandbox).args(args);
    c.env("SF_SHIP_EFFECT_FILE", &e.effects).env_remove("SF_SHIP_CRASH_AFTER_WRITE");
    c
}

fn out(e: &Env, args: &[&str]) -> Output {
    cmd(e, args).output().unwrap()
}

fn json(e: &Env, args: &[&str]) -> Value {
    let o = out(e, args);
    assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stdout));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn start(e: &Env) -> String {
    json(e, &["start", "ship"])["run_id"].as_str().unwrap().to_string()
}

fn state(e: &Env, id: &str) -> String {
    json(e, &["status", id])["state"].as_str().unwrap().to_string()
}

fn effect_lines(e: &Env) -> Vec<String> {
    std::fs::read_to_string(&e.effects).map(|s| s.lines().map(String::from).collect()).unwrap_or_default()
}

fn count(e: &Env, kind: &str) -> usize {
    effect_lines(e).iter().filter(|l| l.split(' ').next() == Some(kind)).count()
}

fn audit(e: &Env, id: &str) -> Vec<Value> {
    json(e, &["audit", id]).as_array().unwrap().clone()
}

fn wake_key(e: &Env, id: &str) -> String {
    json(e, &["status", id])["wake"]["Event"].as_str().unwrap().to_string()
}

#[test]
fn every_gated_action_parks_and_resumes_on_approval() {
    let e = env();
    let id = start(&e);
    json(&e, &["run"]);
    for kind in GATED {
        let s = json(&e, &["status", &id]);
        assert_eq!(s["state"], "AwaitingApproval", "before {kind}: {s}");
        let p = &s["pending_action"];
        assert_eq!(p["kind"], kind, "{s}");
        assert!(p["target"].as_str().is_some_and(|t| !t.is_empty()), "{s}");
        assert!(p["detail"].is_object(), "{s}");
        assert_eq!(p["digest"].as_str().map(str::len), Some(64), "{s}");
        assert_eq!(count(&e, kind), 0, "{kind} ran before approval");
        json(&e, &["approve", &id, "--as", "alice"]);
        assert_eq!(count(&e, kind), 0, "approve itself must not run the effect");
        json(&e, &["run"]);
        assert_eq!(count(&e, kind), 1, "{kind}");
    }
    assert_eq!(state(&e, &id), "Succeeded");

    // Each effect exactly once; draft_pr never parked.
    for kind in GATED.iter().chain(&["draft_pr"]) {
        assert_eq!(count(&e, kind), 1, "{kind}: {:?}", effect_lines(&e));
    }
    let log = audit(&e, &id);
    for kind in GATED {
        let recs: Vec<_> = log.iter().filter(|r| r["action"] == kind).collect();
        assert_eq!(recs.len(), 1, "{kind}: {recs:?}");
        assert_eq!((recs[0]["decision"].as_str(), recs[0]["approver"].as_str()), (Some("approved"), Some("alice")), "{kind}");
    }
    let pr: Vec<_> = log.iter().filter(|r| r["action"] == "draft_pr").collect();
    assert_eq!(pr.len(), 1);
    assert_eq!((pr[0]["decision"].as_str(), pr[0]["approver"].is_null()), (Some("auto"), true));
    let events = json(&e, &["events", &id]);
    let parks = events.as_array().unwrap().iter().filter(|x| x["payload"]["state"] == "parked").count();
    assert_eq!(parks, GATED.len(), "only gated kinds park");
    let v = out(&e, &["audit-verify"]);
    assert!(v.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&v.stdout).unwrap(), serde_json::json!({"ok": true}));
}

#[test]
fn deny_escalates_and_the_effect_never_runs() {
    let e = env();
    let id = start(&e);
    json(&e, &["run"]);
    assert_eq!(json(&e, &["status", &id])["pending_action"]["kind"], "merge_protected");
    // A deny needs a reason and a name.
    assert!(!out(&e, &["deny", &id, "--as", "bob"]).status.success());
    assert!(!out(&e, &["deny", &id, "--reason", "no"]).status.success());
    json(&e, &["deny", &id, "--as", "bob", "--reason", "not now"]);
    json(&e, &["run"]);
    let s = json(&e, &["status", &id]);
    assert_eq!(s["state"], "Escalated", "{s}");
    assert!(s["escalation"].as_str().unwrap().contains("not now"), "{s}");
    assert_eq!(count(&e, "merge_protected"), 0);
    assert_eq!(count(&e, "deploy"), 0);
    json(&e, &["run"]);
    assert_eq!(state(&e, &id), "Escalated", "an escalated run stays put");
    let log = audit(&e, &id);
    assert!(log.iter().any(|r| r["action"] == "merge_protected" && r["decision"] == "denied" && r["approver"] == "bob"));
    assert!(!log.iter().any(|r| r["action"] == "merge_protected" && r["decision"] == "approved"));
    assert!(out(&e, &["audit-verify"]).status.success());
}

#[test]
fn deliver_cannot_answer_an_approval_and_approve_needs_a_name() {
    let e = env();
    let id = start(&e);
    json(&e, &["run"]);
    let key = wake_key(&e, &id);
    assert!(key.starts_with("approval:"), "{key}");
    for k in [key.as_str(), "budget:x", "waive:x:y"] {
        let o = out(&e, &["deliver", k, r#"{"approved":true,"by":"mallory"}"#]);
        assert!(!o.status.success(), "{k}");
        assert!(String::from_utf8_lossy(&o.stdout).contains("named human"), "{}", String::from_utf8_lossy(&o.stdout));
    }
    // A plain event still goes through.
    json(&e, &["deliver", "ci:build-1", r#"{"green":true}"#]);
    for bad in [&["approve", id.as_str()][..], &["approve", &id, "--as", ""], &["approve", &id, "--as", "  "]] {
        assert!(!out(&e, bad).status.success(), "{bad:?}");
    }
    json(&e, &["run"]);
    assert_eq!(state(&e, &id), "AwaitingApproval");
    assert_eq!(count(&e, "merge_protected"), 0);
    assert!(!audit(&e, &id).iter().any(|r| r["decision"] == "approved"));
}

#[test]
fn sigkill_between_approve_and_run_runs_each_effect_once() {
    let e = env();
    let id = start(&e);
    json(&e, &["run"]);
    for kind in GATED {
        json(&e, &["approve", &id, "--as", "alice"]);
        // Kill the runner as soon as the external write lands: before the ledger row or checkpoint.
        let mut child = cmd(&e, &["run"]).stdout(Stdio::null()).spawn().unwrap();
        let t = Instant::now();
        while count(&e, kind) == 0 && t.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = child.kill(); // SIGKILL
        child.wait().unwrap();
        json(&e, &["run"]);
        assert_eq!(count(&e, kind), 1, "{kind}: {:?}", effect_lines(&e));
    }
    assert_eq!(state(&e, &id), "Succeeded");
    for kind in GATED.iter().chain(&["draft_pr"]) {
        assert_eq!(count(&e, kind), 1, "{kind}");
    }
    assert!(out(&e, &["audit-verify"]).status.success());
}

#[test]
fn crash_between_effect_write_and_ledger_does_not_duplicate() {
    let e = env();
    let id = start(&e);
    json(&e, &["run"]);
    json(&e, &["approve", &id, "--as", "alice"]);
    let o = cmd(&e, &["run"]).env("SF_SHIP_CRASH_AFTER_WRITE", "merge_protected").output().unwrap();
    assert!(!o.status.success(), "process should have aborted");
    assert_eq!(count(&e, "merge_protected"), 1);
    json(&e, &["run"]); // the effect re-runs; the receiver dedupes on the key
    assert_eq!(count(&e, "merge_protected"), 1);
    assert_eq!(json(&e, &["status", &id])["pending_action"]["kind"], "deploy");
    let log = audit(&e, &id);
    assert_eq!(log.iter().filter(|r| r["action"] == "merge_protected").count(), 1);
}

#[test]
fn resume_cancel_and_unconfined_flags() {
    let e = env();
    let id = start(&e);
    json(&e, &["run"]);
    assert!(!out(&e, &["resume", &id, "--as", "alice"]).status.success(), "not escalated");
    let c = json(&e, &["--unconfined", "status", &id]);
    assert_eq!(c["unconfined"], true);
    assert!(json(&e, &["status", &id]).get("unconfined").is_none());
    assert!(out(&e, &["--unconfined", "approve", &id]).stdout.windows(11).any(|w| w == b"unconfined\""));
    json(&e, &["cancel", &id]);
    assert_eq!(state(&e, &id), "Cancelled");
    assert!(Path::new(&e.db).exists());
}

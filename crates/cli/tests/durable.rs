//! Milestone 2 exit tests: real `sf` binary, real SQLite file, real process death.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Env {
    _dir: tempfile::TempDir,
    db: PathBuf,
    sandbox: PathBuf,
    side: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().to_path_buf();
    Env { db: p.join("sf.db"), sandbox: p.join("sbx"), side: p.join("side.txt"), _dir: dir }
}

fn cmd(e: &Env, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_sf"));
    c.arg("--db").arg(&e.db).arg("--sandbox").arg(&e.sandbox).args(args);
    c.env("SF_DEMO_SIDE_EFFECT_FILE", &e.side).env_remove("SF_DEMO_CRASH_AFTER_EFFECT").env_remove("SF_DEMO_CRASH_AFTER_WRITE").env("SF_DEMO_SLEEP_MS", "0");
    c
}

fn json(e: &Env, args: &[&str]) -> Value {
    let o = cmd(e, args).output().unwrap();
    assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stdout));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn start(e: &Env) -> String {
    json(e, &["start", "demo"])["run_id"].as_str().unwrap().to_string()
}

fn lines(p: &Path) -> usize {
    std::fs::read_to_string(p).map(|s| s.lines().count()).unwrap_or(0)
}


fn state(e: &Env, id: &str) -> String {
    json(e, &["status", id])["state"].as_str().unwrap().to_string()
}

#[test]
fn sigkill_mid_run_resumes_without_duplicate() {
    let e = env();
    let id = start(&e);
    let mut child = cmd(&e, &["run"]).env("SF_DEMO_SLEEP_MS", "10000").stdout(Stdio::null()).spawn().unwrap();
    let t = Instant::now();
    // Kill as soon as the external write lands, possibly before its ledger row or checkpoint.
    while lines(&e.side) == 0 {
        assert!(t.elapsed() < Duration::from_secs(20), "side effect never appeared");
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().unwrap(); // SIGKILL
    child.wait().unwrap();
    json(&e, &["run"]);
    assert_eq!(state(&e, &id), "AwaitingApproval");
    json(&e, &["approve", &id, "--as", "alice"]);
    json(&e, &["run"]);
    assert_eq!(state(&e, &id), "Succeeded");
    assert_eq!(lines(&e.side), 1);
}

#[test]
fn crash_between_effect_and_checkpoint_does_not_duplicate() {
    let e = env();
    let id = start(&e);
    let o = cmd(&e, &["run"]).env("SF_DEMO_CRASH_AFTER_EFFECT", "1").output().unwrap();
    assert!(!o.status.success(), "process should have aborted");
    assert_eq!(lines(&e.side), 1);
    json(&e, &["run"]);
    json(&e, &["approve", &id, "--as", "alice"]);
    json(&e, &["run"]);
    assert_eq!(state(&e, &id), "Succeeded");
    assert_eq!(lines(&e.side), 1, "effect ledger must suppress the re-run");
}

#[test]
fn crash_between_external_write_and_ledger_does_not_duplicate() {
    // The ledger has no row yet, so the effect re-runs; the receiver's key dedupe absorbs it (DUR-3).
    let e = env();
    let id = start(&e);
    let o = cmd(&e, &["run"]).env("SF_DEMO_CRASH_AFTER_WRITE", "1").output().unwrap();
    assert!(!o.status.success(), "process should have aborted");
    assert_eq!(lines(&e.side), 1);
    json(&e, &["run"]);
    json(&e, &["approve", &id, "--as", "alice"]);
    json(&e, &["run"]);
    assert_eq!(state(&e, &id), "Succeeded");
    assert_eq!(lines(&e.side), 1, "receiver must dedupe on the idempotency key");
}

#[test]
fn deploy_mid_run_across_three_processes() {
    let e = env();
    let id = start(&e);
    json(&e, &["run"]); // process 1: parks for approval, exits
    assert_eq!(state(&e, &id), "AwaitingApproval");
    json(&e, &["approve", &id, "--as", "alice"]); // process 2
    json(&e, &["run"]); // process 3
    let s = json(&e, &["status", &id]);
    assert_eq!(s["state"], "Succeeded");
    let ev = json(&e, &["events", &id]);
    let seqs: Vec<u64> = ev.as_array().unwrap().iter().map(|x| x["seq"].as_u64().unwrap()).collect();
    assert!(!seqs.is_empty());
    assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>(), "seq must be contiguous");
    assert_eq!(lines(&e.side), 1);
}

#[test]
fn cost_baseline_is_reported() {
    let e = env();
    let id = start(&e);
    json(&e, &["run"]);
    let s = json(&e, &["status", &id]);
    assert_eq!(s["spent_tokens"], 1200);
    assert_eq!(s["spent_usd_micros"], 3000);
}

//! Milestone 4 tests (FBK-1): the three feedback sources through the real `sf` binary.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;

struct Env {
    _dir: tempfile::TempDir,
    db: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    Env { db: dir.path().join("sf.db"), _dir: dir }
}

fn sf(e: &Env, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sf")).arg("--db").arg(&e.db).args(args).output().unwrap()
}

fn ok(e: &Env, args: &[&str]) -> Value {
    let o = sf(e, args);
    assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stdout));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn err(e: &Env, args: &[&str]) -> String {
    let o = sf(e, args);
    assert!(!o.status.success(), "{args:?} should fail");
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    v["error"].as_str().unwrap().to_string()
}

fn run(e: &Env) -> String {
    ok(e, &["start", "demo"])["run_id"].as_str().unwrap().to_string()
}

#[test]
fn human_feedback_all_target_kinds_and_listing() {
    let e = env();
    let r = run(&e);
    ok(&e, &["run"]); // creates the step rows
    let step = format!("step:{r}/prepare");
    let art = format!("artifact:src/lib.rs#L10-L24@{r}");
    let run_t = format!("run:{r}");
    for (t, v) in [(&run_t, "comment"), (&step, "fail"), (&art, "wrong"), (&"entry:kb-7".to_string(), "helped"), (&"template:header".to_string(), "comment")] {
        let out = ok(&e, &["feedback", "add", "--target", t, "--verdict", v, "--as", "alice", "--note", "hi"]);
        assert_eq!(out["feedback"]["source"], "human");
        assert_eq!(out["feedback"]["author"], "alice");
        assert_eq!(out["deduped"], false);
    }
    let all = ok(&e, &["feedback", "list"]);
    assert_eq!(all.as_array().unwrap().len(), 5);
    let seqs: Vec<_> = all.as_array().unwrap().iter().map(|f| f["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, [0, 1, 2, 3, 4]);
    assert_eq!(all[2]["target"], serde_json::json!({"kind": "artifact", "run_id": r, "artifact": "src/lib.rs", "span": "L10-L24"}));
    let by_entry = ok(&e, &["feedback", "list", "--target", "entry:kb-7"]);
    assert_eq!(by_entry.as_array().unwrap().len(), 1);
    assert_eq!(by_entry[0]["verdict"], "helped");
    assert_eq!(ok(&e, &["feedback", "list", "--from", "3"]).as_array().unwrap().len(), 2);
}

#[test]
fn explicit_id_dedupes_and_url_artifact_parses() {
    let e = env();
    let a = ["feedback", "add", "--target", "artifact:https://u@h.example/pr/1#L3", "--verdict", "pass", "--as", "bob", "--id", "x1"];
    let first = ok(&e, &a);
    let second = ok(&e, &a);
    assert_eq!(second["deduped"], true);
    assert_eq!(first["feedback"]["seq"], second["feedback"]["seq"]);
    assert_eq!(first["feedback"]["target"]["artifact"], "https://u@h.example/pr/1");
    assert_eq!(first["feedback"]["target"]["span"], "L3");
    assert_eq!(ok(&e, &["feedback", "list"]).as_array().unwrap().len(), 1);
}

#[test]
fn run_and_step_targets_must_exist() {
    let e = env();
    assert!(err(&e, &["feedback", "add", "--target", "run:nope", "--verdict", "pass", "--as", "a"]).contains("not found"));
    let r = run(&e);
    assert!(err(&e, &["feedback", "add", "--target", &format!("step:{r}/ghost"), "--verdict", "pass", "--as", "a"]).contains("not found"));
    assert!(err(&e, &["outcome", "nope", "survived", "--as", "monitor"]).contains("not found"));
    assert!(err(&e, &["feedback", "add", "--target", "bogus", "--verdict", "pass", "--as", "a"]).contains("bad target"));
    assert!(err(&e, &["feedback", "add", "--target", "entry:x", "--verdict", "great", "--as", "a"]).contains("verdict"));
    assert!(err(&e, &["feedback", "add", "--target", "entry:x", "--verdict", "pass"]).contains("--as"));
    ok(&e, &["feedback", "add", "--target", "entry:no-such-entry", "--verdict", "stale", "--as", "a"]);
}

#[test]
fn check_records_pass_fail_exit_code_and_dedupes_identical_output() {
    let e = env();
    let r = run(&e);
    let t = format!("run:{r}");
    let o = sf(&e, &["check", "--name", "unit", "--target", &t, "--", "sh", "-c", "echo hello --as; exit 0"]);
    assert!(o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["feedback"]["verdict"], "pass");
    assert_eq!(v["feedback"]["source"], "auto");
    assert_eq!(v["feedback"]["author"], "unit");
    assert_eq!(v["feedback"]["detail"]["exit_code"], 0);
    assert!(v["feedback"]["detail"]["output_tail"].as_str().unwrap().contains("hello --as"));
    assert_eq!(v["deduped"], false);

    // Same output again: deduped, even though the run took a different time.
    let again: Value = serde_json::from_slice(&sf(&e, &["check", "--name", "unit", "--target", &t, "--", "sh", "-c", "echo hello --as; exit 0"]).stdout).unwrap();
    assert_eq!(again["deduped"], true);
    assert_eq!(again["feedback"]["seq"], v["feedback"]["seq"]);

    // Failure: exits with the command's code, records Fail; different output is a new record.
    let o = sf(&e, &["check", "--name", "unit", "--target", &t, "--", "sh", "-c", "echo boom >&2; exit 3"]);
    assert_eq!(o.status.code(), Some(3));
    let f: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(f["feedback"]["verdict"], "fail");
    assert_eq!(f["feedback"]["detail"]["exit_code"], 3);
    assert!(f["feedback"]["detail"]["output_tail"].as_str().unwrap().contains("boom"));
    assert_eq!(f["feedback"]["seq"], 1);
    assert_eq!(ok(&e, &["feedback", "list"]).as_array().unwrap().len(), 2);
}

#[test]
fn check_keeps_only_last_2kb_and_validates_target() {
    let e = env();
    let o = sf(&e, &["check", "--name", "big", "--target", "template:t", "--", "sh", "-c", "head -c 5000 /dev/zero | tr '\\0' a; printf END"]);
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let tail = v["feedback"]["detail"]["output_tail"].as_str().unwrap();
    assert_eq!(tail.len(), 2048);
    assert!(tail.ends_with("END"));
    assert!(err(&e, &["check", "--name", "c", "--target", "run:ghost", "--", "true"]).contains("not found"));
    assert!(err(&e, &["check", "--name", "c", "--target", "entry:e"]).contains("missing command"));
}

#[test]
fn outcomes_target_the_run() {
    let e = env();
    let r = run(&e);
    for v in ["survived", "reverted", "incident"] {
        let out = ok(&e, &["outcome", &r, v, "--as", "outcome-monitor", "--note", "n"]);
        assert_eq!(out["feedback"]["source"], "outcome");
        assert_eq!(out["feedback"]["verdict"], v);
        assert_eq!(out["feedback"]["target"]["run_id"], r.as_str());
    }
    assert!(err(&e, &["outcome", &r, "pass", "--as", "m"]).contains("outcome must be"));
    assert_eq!(ok(&e, &["feedback", "list", "--target", &format!("run:{r}")]).as_array().unwrap().len(), 3);
}

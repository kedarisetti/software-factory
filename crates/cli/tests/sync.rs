//! Milestone 4 tests (FBK-3): `sf feedback sync` and `sf feedback status` through the real binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};
use sf_core::{Feedback, FeedbackSource, FeedbackTarget, FeedbackVerdict, Store};
use sf_kb::{scan::ScanIndex, Caller, EntryStatus, Kb, Knowledge, LearningRecord, NewEntry, Outcome};

struct Env {
    dir: tempfile::TempDir,
    db: PathBuf,
}

impl Env {
    fn kb_dir(&self) -> PathBuf {
        self.dir.path().join("kb")
    }
    fn golden(&self) -> PathBuf {
        self.dir.path().join("golden")
    }
    fn kb(&self) -> Kb {
        Kb::open(self.kb_dir(), Box::new(ScanIndex::default())).unwrap()
    }
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    Env { db: dir.path().join("sf.db"), dir }
}

fn sf_env(e: &Env, args: &[&str], envs: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sf")).arg("--db").arg(&e.db).args(args).envs(envs.iter().copied()).output().unwrap()
}

fn sf(e: &Env, args: &[&str]) -> Output {
    sf_env(e, args, &[])
}

fn ok(e: &Env, args: &[&str]) -> Value {
    let o = sf(e, args);
    assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stdout));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn sync_args(e: &Env) -> Vec<String> {
    ["feedback", "sync", "--kb", e.kb_dir().to_str().unwrap(), "--golden", e.golden().to_str().unwrap()].map(String::from).to_vec()
}

fn sync_with(e: &Env, envs: &[(&str, &str)]) -> Output {
    let a = sync_args(e);
    sf_env(e, &a.iter().map(String::as_str).collect::<Vec<_>>(), envs)
}

fn sync(e: &Env) -> Value {
    let o = sync_with(e, &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn fb(e: &Env, target: &str, verdict: &str, id: &str) {
    ok(e, &["feedback", "add", "--target", target, "--verdict", verdict, "--as", "alice", "--id", id, "--note", "n"]);
}

fn start(e: &Env) -> String {
    let r = ok(e, &["start", "demo"])["run_id"].as_str().unwrap().to_string();
    ok(e, &["run"]);
    r
}

fn who(session: &str) -> Caller {
    Caller { agent: "pi".into(), session: session.into() }
}

fn note(title: &str) -> NewEntry {
    NewEntry { title: title.into(), body: "body".into(), ..Default::default() }
}

fn candidate(e: &Env, sub: &str, id: &str) -> Option<Value> {
    let p = e.golden().join("candidates").join(sub).join(format!("{id}.json"));
    std::fs::read_to_string(p).ok().map(|t| serde_json::from_str(&t).unwrap())
}

fn files(dir: &Path) -> usize {
    std::fs::read_dir(dir).map(|d| d.filter(|e| e.as_ref().unwrap().path().is_file()).count()).unwrap_or(0)
}

#[test]
fn entry_verdicts_update_kb_and_rerun_applies_nothing() {
    let e = env();
    let kb = e.kb();
    let a = kb.add(&who("s"), note("Alpha")).unwrap().meta.id;
    let b = kb.add(&who("s"), note("Beta")).unwrap().meta.id;
    fb(&e, &format!("entry:{a}"), "helped", "f1");
    fb(&e, &format!("entry:{a}"), "misled", "f2");
    fb(&e, &format!("entry:{b}"), "wrong", "f3");
    fb(&e, "entry:notes/general/nope", "stale", "f4");
    fb(&e, &format!("entry:{a}"), "comment", "f5"); // no kb meaning

    let s = sync(&e);
    assert_eq!((s["processed"].clone(), s["cursor_before"].clone(), s["cursor_after"].clone()), (json!(5), json!(0), json!(5)));
    assert_eq!(s["applied"]["entry_feedback"], 3);
    assert_eq!(s["applied"]["entry_missing"], 1);
    assert_eq!(s["applied"]["ignored"], 1);
    let ea = kb.get(&who("s"), &a).unwrap().meta;
    assert_eq!((ea.helped, ea.misled, ea.status), (1, 1, EntryStatus::Active));
    let eb = kb.get(&who("s"), &b).unwrap().meta;
    assert_eq!((eb.misled, eb.status), (1, EntryStatus::Expired)); // M1 rules: wrong expires
    // The sync is recorded as the factory agent.
    let log = std::fs::read_to_string(e.kb_dir().join(".kb/calls.jsonl")).unwrap();
    assert!(log.lines().any(|l| l.contains("\"agent\":\"factory\"") && l.contains("sync:f1")));

    let again = sync(&e);
    assert_eq!(again["processed"], 0);
    assert_eq!((again["cursor_before"].clone(), again["cursor_after"].clone()), (json!(5), json!(5)));
    let ea = kb.get(&who("s"), &a).unwrap().meta;
    assert_eq!((ea.helped, ea.misled), (1, 1));
}

#[test]
fn reverted_outcome_expires_that_runs_learnings_only() {
    let e = env();
    let r = start(&e);
    let other = start(&e);
    let kb = e.kb();
    let rec = |t: &str| LearningRecord { task_class: "fix-ci".into(), worked: vec![t.into()], outcome: Some(Outcome::Succeeded), ..Default::default() };
    let bad = kb.learn(&who(&r), rec("bump the timeout")).unwrap().created[0].clone();
    let fine = kb.learn(&who(&other), rec("pin the toolchain")).unwrap().created[0].clone();
    ok(&e, &["outcome", &r, "reverted", "--as", "outcome-monitor", "--id", "o1"]);
    ok(&e, &["outcome", &other, "survived", "--as", "outcome-monitor", "--id", "o2"]);

    let s = sync(&e);
    assert_eq!(s["applied"]["learnings_expired"], 1);
    assert_eq!(s["applied"]["ignored"], 1); // survived
    assert_eq!(kb.get(&who("s"), &bad).unwrap().meta.status, EntryStatus::Expired);
    assert_eq!(kb.get(&who("s"), &fine).unwrap().meta.status, EntryStatus::Active);
    // The replayed finding is recorded as reverted, from that run, linked to what it contradicted.
    let listing = kb.list(&who("s"), "learnings/fix-ci").unwrap();
    let new = listing.entries.iter().find(|i| i.id != bad && i.id != fine).expect("reverted learning");
    let m = kb.get(&who("s"), &new.id).unwrap().meta;
    assert_eq!((m.outcome, m.source.as_str()), (Some(Outcome::Reverted), format!("run:{r}").as_str()));
    assert!(m.links.iter().any(|l| l.target == bad));
    // The reverted run also yields an eval candidate.
    assert!(candidate(&e, "", "o1").is_some());
    assert!(candidate(&e, "", "o2").is_none());

    // Re-sync (even with the cursor reset) does not expire or duplicate anything.
    let st = sf_store_sqlite::SqliteStore::open(&e.db).unwrap();
    st.set_cursor("feedback-sync", 0).unwrap();
    drop(st);
    let s = sync(&e);
    assert_eq!(s["applied"]["learnings_expired"], 0);
    assert_eq!(kb.list(&who("s"), "learnings/fix-ci").unwrap().entries.len(), 3);
}

#[test]
fn negative_feedback_writes_candidates_with_run_context() {
    let e = env();
    let r = start(&e);
    fb(&e, &format!("step:{r}/prepare"), "fail", "step-bad");
    fb(&e, &format!("artifact:src/lib.rs#L1-L3@{r}"), "wrong", "art-bad");
    fb(&e, "artifact:https://example.com/pr/1", "fail", "art-norun");
    fb(&e, &format!("run:{r}"), "comment", "just-a-comment");
    fb(&e, &format!("run:{r}"), "pass", "pass");
    fb(&e, "template:header", "fail", "tpl-bad");
    fb(&e, "template:header", "comment", "tpl-note");
    ok(&e, &["check", "--name", "unit", "--target", &format!("run:{r}"), "--id", "x", "--", "true"]);
    let o = sf(&e, &["check", "--name", "unit", "--target", &format!("run:{r}"), "--", "sh", "-c", "echo plain failure; exit 1"]);
    assert_eq!(o.status.code(), Some(1));
    let o = sf(&e, &["check", "--name", "golden", "--target", &format!("run:{r}"), "--", "sh", "-c", "echo 'gate: FAIL (1 regressions)'; exit 1"]);
    assert_eq!(o.status.code(), Some(1));
    let golden_id = serde_json::from_slice::<Value>(&o.stdout).unwrap()["feedback"]["feedback_id"].as_str().unwrap().to_string();

    let s = sync(&e);
    assert_eq!(s["applied"]["candidates"], 4); // step, artifact x2, golden check
    assert_eq!(s["applied"]["template_candidates"], 1);
    assert_eq!(files(&e.golden().join("candidates")), 4);
    assert_eq!(files(&e.golden().join("candidates/template")), 1);

    let c = candidate(&e, "", "step-bad").unwrap();
    assert_eq!(c["version"], 1);
    assert_eq!(c["feedback"]["feedback_id"], "step-bad");
    assert_eq!((c["run"]["workflow_id"].clone(), c["run"]["workflow_version"].clone()), (json!("demo"), json!(1)));
    assert_eq!(c["steps"].as_array().unwrap().len(), 1);
    assert_eq!(c["steps"][0]["step_id"], "prepare");
    assert_eq!(c["steps"][0]["output"], json!({"prepared": true}));
    assert!(!c["event_log_excerpt"].as_array().unwrap().is_empty());
    assert!(c["event_log_excerpt"].as_array().unwrap().iter().all(|ev| ev["step_id"] == "prepare"));
    assert!(c["expected"]["TODO"].is_string());
    // Run-wide context for artifacts of a run; none for a bare URL.
    assert!(candidate(&e, "", "art-bad").unwrap()["steps"].as_array().unwrap().len() > 1);
    assert!(candidate(&e, "", "art-norun").unwrap()["run"].is_null());
    assert!(candidate(&e, "template", "tpl-bad").unwrap()["feedback"]["target"]["section"] == "header");
    assert!(candidate(&e, "", &safe(&golden_id)).is_some());
    assert!(candidate(&e, "", "just-a-comment").is_none());

    // Re-run: nothing new, and no file changes.
    assert_eq!(sync(&e)["processed"], 0);
    assert_eq!(files(&e.golden().join("candidates")), 4);
}

fn safe(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect()
}

#[test]
fn crash_after_apply_reapplies_at_most_one_record_without_double_counting() {
    let e = env();
    let kb = e.kb();
    let a = kb.add(&who("s"), note("Alpha")).unwrap().meta.id;
    for i in 0..3 {
        fb(&e, &format!("entry:{a}"), "helped", &format!("h{i}"));
    }
    let o = sync_with(&e, &[("SF_SYNC_CRASH_AFTER_APPLY", "2")]);
    assert!(!o.status.success(), "should abort");
    // Two records were applied; only the first advanced the cursor.
    assert_eq!(kb.get(&who("s"), &a).unwrap().meta.helped, 2);
    assert_eq!(ok(&e, &["feedback", "status"])["cursor"], 1);
    assert_eq!(ok(&e, &["feedback", "status"])["pending"], 2);

    let s = sync(&e);
    assert_eq!((s["processed"].clone(), s["cursor_before"].clone(), s["cursor_after"].clone()), (json!(2), json!(1), json!(3)));
    assert_eq!(s["applied"]["entry_already_applied"], 1); // the record applied before the crash
    assert_eq!(kb.get(&who("s"), &a).unwrap().meta.helped, 3);
    assert_eq!(sync(&e)["processed"], 0);
}

#[test]
fn status_reports_pending_and_age_and_exits_2_past_a_day() {
    let e = env();
    let s = ok(&e, &["feedback", "status"]);
    assert_eq!((s["pending"].clone(), s["oldest_pending_age_ms"].clone()), (json!(0), Value::Null));

    fb(&e, "entry:notes/general/x", "helped", "fresh");
    let s = ok(&e, &["feedback", "status"]);
    assert_eq!(s["pending"], 1);
    assert!(s["oldest_pending_age_ms"].as_u64().unwrap() < 60_000);

    // A record from two days ago, written through the store (the CLI always stamps "now").
    let old = sf_core::Clock::now(&sf_core::SystemClock) - 2 * 24 * 3600 * 1000;
    let st = sf_store_sqlite::SqliteStore::open(&e.db).unwrap();
    st.append_feedback(&Feedback {
        seq: 0,
        feedback_id: "old".into(),
        target: FeedbackTarget::Template { section: "t".into() },
        source: FeedbackSource::Human,
        verdict: FeedbackVerdict::Comment,
        note: None,
        author: "a".into(),
        ts: old,
        detail: Value::Null,
    })
    .unwrap();
    drop(st);
    let o = sf(&e, &["feedback", "status"]);
    assert_eq!(o.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!((v["pending"].clone(), v["over_limit"].clone()), (json!(2), json!(true)));
    assert!(v["oldest_pending_age_ms"].as_u64().unwrap() > 2 * 24 * 3600 * 1000 - 60_000);

    sync(&e);
    let s = ok(&e, &["feedback", "status"]);
    assert_eq!((s["pending"].clone(), s["oldest_pending_age_ms"].clone()), (json!(0), Value::Null));
}

#[test]
fn sync_requires_a_kb() {
    let e = env();
    let o = sf(&e, &["feedback", "sync"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stdout).contains("--kb"));
}

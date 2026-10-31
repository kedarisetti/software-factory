//! FBK-3 daily sync: feedback -> knowledge base and eval-set candidates; plus the lag check.
//!
//! Reads feedback from the `feedback-sync` cursor (the next seq to process) and advances it only after
//! each record is fully applied, so a crash re-applies at most one record. Every application is
//! idempotent: entry verdicts are deduped against the kb call log, run-outcome expiry only touches
//! Active learnings, candidate files are overwritten atomically.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sf_core::{Clock, Feedback, FeedbackSource, FeedbackTarget, FeedbackVerdict, Store, SystemClock};
use sf_kb::{Caller, EntryKind, EntryStatus, Knowledge, LearningRecord, Outcome, Verdict};

pub const CURSOR: &str = "feedback-sync";
/// FBK-3: feedback must reach the kb and the eval set within a day.
pub const LIMIT_MS: u64 = 24 * 60 * 60 * 1000;
const EXCERPT_EVENTS: usize = 50;

/// The one place the concrete index is constructed (same layout as `sf-kb`).
pub fn open_kb(root: &Path) -> Result<sf_kb::Kb, String> {
    let dir = root.join(".kb").join("index");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let idx = sf_retrieval_tantivy::TantivyIndex::open(&dir).map_err(|e| e.to_string())?;
    sf_kb::Kb::open(root, Box::new(idx)).map_err(|e| e.to_string())
}

fn caller() -> Caller {
    Caller { agent: "factory".into(), session: "feedback-sync".into() }
}

#[derive(Default)]
struct Counts {
    entry_feedback: u64,
    entry_missing: u64,
    entry_already_applied: u64,
    learnings_expired: u64,
    candidates: u64,
    template_candidates: u64,
    ignored: u64,
}

fn s<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Process everything past the cursor. Returns the JSON summary.
pub fn sync(store: &dyn Store, kb: &sf_kb::Kb, golden: &Path) -> Result<Value, String> {
    let before = store.get_cursor(CURSOR).map_err(s)?.unwrap_or(0);
    let pending = store.read_feedback(before).map_err(s)?;
    let mut c = Counts::default();
    let mut processed = 0u64;
    let mut after = before;
    // Test hook (crash safety): abort after applying the n-th record, before its cursor advance.
    let crash_at = std::env::var("SF_SYNC_CRASH_AFTER_APPLY").ok().and_then(|v| v.parse::<u64>().ok());
    for fb in &pending {
        apply(store, kb, golden, fb, &mut c)?;
        processed += 1;
        if crash_at == Some(processed) {
            std::process::abort();
        }
        after = fb.seq + 1;
        store.set_cursor(CURSOR, after).map_err(s)?;
    }
    Ok(json!({
        "processed": processed,
        "applied": {
            "entry_feedback": c.entry_feedback,
            "entry_missing": c.entry_missing,
            "entry_already_applied": c.entry_already_applied,
            "learnings_expired": c.learnings_expired,
            "candidates": c.candidates,
            "template_candidates": c.template_candidates,
            "ignored": c.ignored,
        },
        "cursor_before": before,
        "cursor_after": after,
    }))
}

fn apply(store: &dyn Store, kb: &sf_kb::Kb, golden: &Path, fb: &Feedback, c: &mut Counts) -> Result<(), String> {
    let mut acted = false;
    if let FeedbackTarget::Entry { entry_id } = &fb.target {
        acted = true;
        entry_verdict(kb, fb, entry_id, c)?;
    }
    if fb.source == FeedbackSource::Outcome && matches!(fb.verdict, FeedbackVerdict::Reverted | FeedbackVerdict::Incident) {
        if let FeedbackTarget::Run { run_id } = &fb.target {
            acted = true;
            c.learnings_expired += expire_learnings(kb, fb, run_id)?;
        }
    }
    if wants_candidate(fb) {
        match &fb.target {
            FeedbackTarget::Run { .. } | FeedbackTarget::Step { .. } | FeedbackTarget::Artifact { .. } => {
                write_candidate(store, golden, fb, false)?;
                c.candidates += 1;
                acted = true;
            }
            FeedbackTarget::Template { .. } => {
                write_candidate(store, golden, fb, true)?;
                c.template_candidates += 1;
                acted = true;
            }
            FeedbackTarget::Entry { .. } => {}
        }
    }
    if !acted {
        c.ignored += 1;
    }
    Ok(())
}

/// Negative human or outcome feedback; auto only when it is a golden-set failure.
fn wants_candidate(fb: &Feedback) -> bool {
    match fb.source {
        FeedbackSource::Human | FeedbackSource::Outcome => fb.verdict.is_negative(),
        FeedbackSource::Auto => fb.verdict == FeedbackVerdict::Fail && is_golden_failure(&fb.detail),
    }
}

/// A check says it ran the golden set via `detail.golden_set: true`, or its output tail carries the
/// gate's own failure line.
fn is_golden_failure(detail: &Value) -> bool {
    detail["golden_set"] == true || detail["output_tail"].as_str().is_some_and(|t| t.contains("gate: FAIL"))
}

/// Has the kb already logged a successful feedback op for this record? The op logs the note we pass.
fn already_applied(kb: &sf_kb::Kb, marker: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(kb.root().join(".kb").join("calls.jsonl")) else { return false };
    text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).any(|l| {
        l["op"] == "feedback"
            && l["ok"] == true
            && l["args"]["note"].as_str().is_some_and(|n| n == marker || n.starts_with(&format!("{marker} ")))
    })
}

fn note_for(fb: &Feedback, extra: &str) -> String {
    let mut n = format!("sync:{}", fb.feedback_id);
    for part in [extra, fb.note.as_deref().unwrap_or("")] {
        if !part.is_empty() {
            n.push(' ');
            n.push_str(part);
        }
    }
    n
}

/// KIF feedback on an entry: counts and expiry follow the M1 rules (the kb applies them).
fn entry_verdict(kb: &sf_kb::Kb, fb: &Feedback, id: &str, c: &mut Counts) -> Result<(), String> {
    let v = match fb.verdict {
        FeedbackVerdict::Helped => Verdict::Helped,
        FeedbackVerdict::Misled => Verdict::Misled,
        FeedbackVerdict::Wrong => Verdict::Wrong,
        FeedbackVerdict::Stale => Verdict::Stale,
        _ => {
            c.ignored += 1;
            return Ok(());
        }
    };
    if already_applied(kb, &format!("sync:{}", fb.feedback_id)) {
        c.entry_already_applied += 1;
        return Ok(());
    }
    match kb.feedback(&caller(), id, v, Some(&note_for(fb, ""))) {
        Ok(_) => c.entry_feedback += 1,
        // Feedback on an entry that is gone or never was: nothing to update, do not block the queue.
        Err(sf_kb::Error::NotFound(_) | sf_kb::Error::Invalid(_)) => c.entry_missing += 1,
        Err(e) => return Err(format!("entry {id}: {e}")),
    }
    Ok(())
}

/// LRN-4 through the public API: for each Active learning from this run that was recorded as a
/// success, replay its finding as a Reverted/Failed learning from the same run. `learn` expires the
/// contradicted entry and writes a new one with the negative outcome, linked `contradicts`.
fn expire_learnings(kb: &sf_kb::Kb, fb: &Feedback, run_id: &str) -> Result<u64, String> {
    let who = Caller { agent: "factory".into(), session: run_id.to_string() };
    let outcome = if fb.verdict == FeedbackVerdict::Reverted { Outcome::Reverted } else { Outcome::Failed };
    let mut n = 0;
    let top = match kb.list(&who, EntryKind::Learning.dir()) {
        Ok(l) => l,
        Err(sf_kb::Error::NotFound(_)) => return Ok(0), // no learnings yet
        Err(e) => return Err(s(e)),
    };
    for dir in top.dirs {
        for item in kb.list(&who, &dir.path).map_err(s)?.entries {
            if item.status != EntryStatus::Active {
                continue;
            }
            let e = kb.get(&who, &item.id).map_err(s)?;
            let m = &e.meta;
            let from_run = m.source_runs.iter().any(|r| r == run_id) || m.source == format!("run:{run_id}");
            if !from_run || matches!(m.outcome, Some(Outcome::Failed | Outcome::Reverted)) {
                continue;
            }
            let Some(task_class) = m.task_class.clone() else { continue };
            // Body is `<finding>\n` plus an optional `\n\nSteps: ..` suffix (kb `learn`).
            let text = e.body.split("\n\nSteps:").next().unwrap_or("").trim().to_string();
            let mut rec = LearningRecord { task_class, outcome: Some(outcome), ..Default::default() };
            match m.tags.first().map(String::as_str) {
                Some("worked") => rec.worked = vec![text],
                Some("failed") => rec.failed = vec![text],
                Some("dead-end") => rec.dead_ends = vec![text],
                Some("shortest-path") => rec.shortest_path = Some(text),
                _ => continue,
            }
            n += kb.learn(&who, rec).map_err(s)?.expired.len() as u64;
        }
    }
    Ok(n)
}

fn safe_name(id: &str) -> String {
    let n: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    n.trim_start_matches('.').to_string()
}

fn run_of(t: &FeedbackTarget) -> Option<&str> {
    match t {
        FeedbackTarget::Run { run_id } | FeedbackTarget::Step { run_id, .. } => Some(run_id),
        FeedbackTarget::Artifact { run_id, .. } => run_id.as_deref(),
        _ => None,
    }
}

/// `golden/candidates/<feedback_id>.json` (`candidates/template/` for template targets): the feedback
/// record plus what a human needs to turn it into a case. Format in docs/verification.md.
fn write_candidate(store: &dyn Store, golden: &Path, fb: &Feedback, template: bool) -> Result<(), String> {
    let mut dir = golden.join("candidates");
    if template {
        dir.push("template");
    }
    std::fs::create_dir_all(&dir).map_err(s)?;
    let (mut run, mut steps, mut events) = (Value::Null, Value::Null, Value::Null);
    if let Some(run_id) = run_of(&fb.target) {
        if let Some(r) = store.get_run(run_id).map_err(s)? {
            run = json!({"run_id": r.run_id, "workflow_id": r.workflow_id, "workflow_version": r.workflow_version, "state": format!("{:?}", r.state)});
            let mut all = store.get_steps(run_id).map_err(s)?;
            if let FeedbackTarget::Step { step_id, .. } = &fb.target {
                all.retain(|st| st.step_id == *step_id);
            }
            steps = json!(all.iter().map(|st| json!({"step_id": st.step_id, "status": format!("{:?}", st.status), "attempt": st.attempt, "output": st.output, "error": st.error})).collect::<Vec<_>>());
            let mut ev = store.read_run(run_id).map_err(s)?;
            if let FeedbackTarget::Step { step_id, .. } = &fb.target {
                ev.retain(|e| e.step_id == *step_id);
            }
            let skip = ev.len().saturating_sub(EXCERPT_EVENTS);
            events = json!(ev[skip..].iter().map(|e| json!({"seq": e.seq, "step_id": e.step_id, "kind": format!("{:?}", e.kind), "ts": e.ts, "payload": e.payload})).collect::<Vec<_>>());
        }
    }
    let doc = json!({
        "version": 1,
        "feedback": fb,
        "run": run,
        "steps": steps,
        "event_log_excerpt": events,
        "expected": {"TODO": "a human sets the expected outcome, then moves this into golden/cases/ (see docs/verification.md)"},
    });
    let path: PathBuf = dir.join(format!("{}.json", safe_name(&fb.feedback_id)));
    let tmp = dir.join(format!(".{}.tmp", safe_name(&fb.feedback_id)));
    let body = serde_json::to_string_pretty(&doc).map_err(s)?;
    std::fs::write(&tmp, body + "\n").map_err(s)?;
    std::fs::rename(&tmp, &path).map_err(s)
}

/// Pending count and oldest pending age, so a monitor can alert before the one-day bound (FBK-3).
/// Second value: true when the bound is exceeded.
pub fn status(store: &dyn Store) -> Result<(Value, bool), String> {
    let cursor = store.get_cursor(CURSOR).map_err(s)?.unwrap_or(0);
    let pending = store.read_feedback(cursor).map_err(s)?;
    let now = SystemClock.now();
    let age = pending.iter().map(|f| f.ts).min().map(|t| now.saturating_sub(t));
    let over = age.is_some_and(|a| a > LIMIT_MS);
    Ok((json!({"pending": pending.len(), "oldest_pending_age_ms": age, "limit_ms": LIMIT_MS, "over_limit": over, "cursor": cursor}), over))
}

//! SQLite-backed durable store (DUR-1 to DUR-3, DUR-5). Depends only on sf-core.
//!
//! WAL plus `synchronous=FULL` makes every commit survive power loss (DUR-1).
//! The event log and the audit log are append-only, enforced by triggers (DUR-2, GOV-audit).

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, ErrorCode, OptionalExtension, Row, TransactionBehavior};
use serde::de::DeserializeOwned;
use serde::Serialize;
use sf_core::{
    AuditRecord, Commit, Decision, Error, Event, Feedback, FeedbackTarget, Result, Run, RunState, Step, Store,
};

/// Migrations, applied in order; `PRAGMA user_version` records how many have run.
const MIGRATIONS: &[&str] = &["
CREATE TABLE runs (
    run_id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL, workflow_version INTEGER NOT NULL,
    trigger_ref TEXT NOT NULL, state TEXT NOT NULL, budget_tokens INTEGER NOT NULL,
    budget_usd_micros INTEGER NOT NULL, spent_tokens INTEGER NOT NULL,
    spent_usd_micros INTEGER NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
    wake TEXT
);
CREATE INDEX runs_state ON runs(state, created_at);
CREATE TABLE steps (
    run_id TEXT NOT NULL REFERENCES runs(run_id), step_id TEXT NOT NULL,
    agent_id TEXT, model_id TEXT, attempt INTEGER NOT NULL, status TEXT NOT NULL,
    idempotency_key TEXT NOT NULL, output TEXT, error TEXT,
    PRIMARY KEY (run_id, step_id)
);
CREATE TABLE events (
    run_id TEXT NOT NULL REFERENCES runs(run_id), seq INTEGER NOT NULL, step_id TEXT NOT NULL,
    kind TEXT NOT NULL, payload TEXT NOT NULL, ts INTEGER NOT NULL,
    PRIMARY KEY (run_id, seq)
);
CREATE TRIGGER events_no_update BEFORE UPDATE ON events
BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events
BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
CREATE TABLE effects (key TEXT PRIMARY KEY, result TEXT NOT NULL);
CREATE TABLE deliveries (key TEXT PRIMARY KEY, payload TEXT NOT NULL);
", "
CREATE TABLE audit (
    seq INTEGER PRIMARY KEY, key TEXT UNIQUE NOT NULL, ts INTEGER NOT NULL, run_id TEXT NOT NULL,
    step_id TEXT NOT NULL, agent_id TEXT, action TEXT NOT NULL, target TEXT NOT NULL,
    digest TEXT NOT NULL, decision TEXT NOT NULL, approver TEXT, detail TEXT NOT NULL,
    prev_hash TEXT NOT NULL, hash TEXT NOT NULL
);
CREATE INDEX audit_run ON audit(run_id, seq);
CREATE TRIGGER audit_no_update BEFORE UPDATE ON audit
BEGIN SELECT RAISE(ABORT, 'audit is append-only'); END;
CREATE TRIGGER audit_no_delete BEFORE DELETE ON audit
BEGIN SELECT RAISE(ABORT, 'audit is append-only'); END;
", "
CREATE TABLE feedback (
    seq INTEGER PRIMARY KEY, feedback_id TEXT UNIQUE NOT NULL, target TEXT NOT NULL,
    target_kind TEXT NOT NULL, target_id TEXT NOT NULL, source TEXT NOT NULL,
    verdict TEXT NOT NULL, note TEXT, author TEXT NOT NULL, ts INTEGER NOT NULL,
    detail TEXT NOT NULL
);
CREATE INDEX feedback_target ON feedback(target_kind, target_id, seq);
CREATE TRIGGER feedback_no_update BEFORE UPDATE ON feedback
BEGIN SELECT RAISE(ABORT, 'feedback is append-only'); END;
CREATE TRIGGER feedback_no_delete BEFORE DELETE ON feedback
BEGIN SELECT RAISE(ABORT, 'feedback is append-only'); END;
CREATE TABLE cursors (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
"];

/// Durable [`Store`] on a local SQLite file.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

fn err(e: rusqlite::Error) -> Error {
    match &e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::ConstraintViolation => {
            Error::Conflict(e.to_string())
        }
        _ => Error::Storage(e.to_string()),
    }
}

fn json<T: Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| Error::Storage(e.to_string()))
}

fn unjson<T: DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| Error::Storage(format!("corrupt row: {e}")))
}

/// Enums are stored as JSON text, e.g. `"Queued"`.
const RUN_COLS: &str = "run_id, workflow_id, workflow_version, trigger_ref, state, budget_tokens, \
    budget_usd_micros, spent_tokens, spent_usd_micros, created_at, updated_at, wake";

fn run_from(r: &Row) -> rusqlite::Result<Result<Run>> {
    let (state, wake): (String, Option<String>) = (r.get(4)?, r.get(11)?);
    let u = |i| r.get::<_, i64>(i).map(|v| v as u64);
    let build = || -> Result<Run> {
        Ok(Run {
            run_id: r.get(0).map_err(err)?,
            workflow_id: r.get(1).map_err(err)?,
            workflow_version: r.get(2).map_err(err)?,
            trigger_ref: r.get(3).map_err(err)?,
            state: unjson(&state)?,
            budget_tokens: u(5).map_err(err)?,
            budget_usd_micros: u(6).map_err(err)?,
            spent_tokens: u(7).map_err(err)?,
            spent_usd_micros: u(8).map_err(err)?,
            created_at: u(9).map_err(err)?,
            updated_at: u(10).map_err(err)?,
            wake: wake.as_deref().map(unjson).transpose()?,
        })
    };
    Ok(build())
}

fn step_from(r: &Row) -> rusqlite::Result<Result<Step>> {
    let (status, output): (String, Option<String>) = (r.get(5)?, r.get(7)?);
    let build = || -> Result<Step> {
        Ok(Step {
            run_id: r.get(0).map_err(err)?,
            step_id: r.get(1).map_err(err)?,
            agent_id: r.get(2).map_err(err)?,
            model_id: r.get(3).map_err(err)?,
            attempt: r.get(4).map_err(err)?,
            status: unjson(&status)?,
            idempotency_key: r.get(6).map_err(err)?,
            output: output.as_deref().map(unjson).transpose()?,
            error: r.get(8).map_err(err)?,
        })
    };
    Ok(build())
}

fn event_from(r: &Row) -> rusqlite::Result<Result<Event>> {
    let (kind, payload): (String, String) = (r.get(3)?, r.get(4)?);
    let build = || -> Result<Event> {
        Ok(Event {
            run_id: r.get(0).map_err(err)?,
            seq: r.get::<_, i64>(1).map_err(err)? as u64,
            step_id: r.get(2).map_err(err)?,
            kind: unjson(&kind)?,
            payload: unjson(&payload)?,
            ts: r.get::<_, i64>(5).map_err(err)? as u64,
        })
    };
    Ok(build())
}

const AUDIT_COLS: &str = "seq, ts, run_id, step_id, agent_id, key, action, target, digest, \
    decision, approver, detail, prev_hash, hash";

fn decision_str(d: Decision) -> Result<String> {
    match serde_json::to_value(d) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        _ => Err(Error::Storage("decision did not serialize to a string".into())),
    }
}

fn audit_from(r: &Row) -> rusqlite::Result<Result<AuditRecord>> {
    let (decision, detail): (String, String) = (r.get(9)?, r.get(11)?);
    Ok((|| {
        Ok(AuditRecord {
            seq: r.get::<_, i64>(0).map_err(err)? as u64,
            ts: r.get::<_, i64>(1).map_err(err)? as u64,
            run_id: r.get(2).map_err(err)?,
            step_id: r.get(3).map_err(err)?,
            agent_id: r.get(4).map_err(err)?,
            key: r.get(5).map_err(err)?,
            action: r.get(6).map_err(err)?,
            target: r.get(7).map_err(err)?,
            digest: r.get(8).map_err(err)?,
            decision: unjson(&json(&decision)?)?,
            approver: r.get(10).map_err(err)?,
            detail: unjson(&detail)?,
            prev_hash: r.get(12).map_err(err)?,
            hash: r.get(13).map_err(err)?,
        })
    })())
}

const FEEDBACK_COLS: &str = "seq, feedback_id, target, source, verdict, note, author, ts, detail";

/// Indexed lookup key of a target: (kind, id). Steps are `run/step`.
fn target_key(t: &FeedbackTarget) -> (&'static str, String) {
    match t {
        FeedbackTarget::Run { run_id } => ("run", run_id.clone()),
        FeedbackTarget::Step { run_id, step_id } => ("step", format!("{run_id}/{step_id}")),
        FeedbackTarget::Artifact { artifact, .. } => ("artifact", artifact.clone()),
        FeedbackTarget::Entry { entry_id } => ("entry", entry_id.clone()),
        FeedbackTarget::Template { section } => ("template", section.clone()),
    }
}

fn feedback_from(r: &Row) -> rusqlite::Result<Result<Feedback>> {
    let (target, source, verdict, detail): (String, String, String, String) =
        (r.get(2)?, r.get(3)?, r.get(4)?, r.get(8)?);
    Ok((|| {
        Ok(Feedback {
            seq: r.get::<_, i64>(0).map_err(err)? as u64,
            feedback_id: r.get(1).map_err(err)?,
            target: unjson(&target)?,
            source: unjson(&json(&source)?)?,
            verdict: unjson(&json(&verdict)?)?,
            note: r.get(5).map_err(err)?,
            author: r.get(6).map_err(err)?,
            ts: r.get::<_, i64>(7).map_err(err)? as u64,
            detail: unjson(&detail)?,
        })
    })())
}

fn enum_str<T: Serialize>(v: &T) -> Result<String> {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        _ => Err(Error::Storage("enum did not serialize to a string".into())),
    }
}

fn insert_run(c: &Connection, run: &Run) -> Result<usize> {
    let sql = format!("INSERT INTO runs ({RUN_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)");
    c.execute(&sql, params![
        run.run_id, run.workflow_id, run.workflow_version, run.trigger_ref, json(&run.state)?,
        run.budget_tokens as i64, run.budget_usd_micros as i64, run.spent_tokens as i64,
        run.spent_usd_micros as i64, run.created_at as i64, run.updated_at as i64,
        run.wake.as_ref().map(json).transpose()?,
    ])
    .map_err(err)
}

impl SqliteStore {
    /// Open (creating if needed) a file database and migrate it.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::init(Connection::open(path).map_err(err)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().map_err(err)?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_millis(5000)).map_err(err)?;
        // journal_mode returns a row, so read it back instead of executing.
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(())).map_err(err)?;
        conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;").map_err(err)?;
        Self::migrate(&mut conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn migrate(conn: &mut Connection) -> Result<()> {
        let v: usize = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(err)?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(v) {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(err)?;
            tx.execute_batch(sql).map_err(err)?;
            tx.pragma_update(None, "user_version", i + 1).map_err(err)?;
            tx.commit().map_err(err)?;
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn get_json(&self, sql: &str, key: &str) -> Result<Option<serde_json::Value>> {
        let s: Option<String> =
            self.lock().query_row(sql, [key], |r| r.get(0)).optional().map_err(err)?;
        s.as_deref().map(unjson).transpose()
    }
}

impl Store for SqliteStore {
    fn create_run(&self, run: &Run) -> Result<()> {
        insert_run(&self.lock(), run).map(|_| ())
    }

    fn get_run(&self, run_id: &str) -> Result<Option<Run>> {
        let sql = format!("SELECT {RUN_COLS} FROM runs WHERE run_id = ?1");
        let r = self.lock().query_row(&sql, [run_id], run_from).optional().map_err(err)?;
        r.transpose()
    }

    fn list_runs(&self, states: &[RunState]) -> Result<Vec<Run>> {
        let wanted = states.iter().map(json).collect::<Result<Vec<_>>>()?;
        let sql = format!("SELECT {RUN_COLS} FROM runs ORDER BY created_at, run_id");
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&sql).map_err(err)?;
        let rows = stmt.query_map([], run_from).map_err(err)?;
        let mut out = Vec::new();
        for row in rows {
            let run = row.map_err(err)??;
            if wanted.contains(&json(&run.state)?) {
                out.push(run);
            }
        }
        Ok(out)
    }

    fn get_steps(&self, run_id: &str) -> Result<Vec<Step>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT run_id, step_id, agent_id, model_id, attempt, status, idempotency_key, \
                 output, error FROM steps WHERE run_id = ?1 ORDER BY rowid",
            )
            .map_err(err)?;
        let rows = stmt.query_map([run_id], step_from).map_err(err)?;
        rows.map(|r| r.map_err(err)?).collect()
    }

    /// DUR-1: run row, step and events land in one `BEGIN IMMEDIATE` transaction.
    /// Any failure (e.g. a duplicate seq) drops the transaction, which rolls it back.
    fn commit(&self, c: &Commit) -> Result<()> {
        if c.step.as_ref().is_some_and(|s| s.run_id != c.run.run_id)
            || c.events.iter().any(|e| e.run_id != c.run.run_id)
        {
            return Err(Error::Invalid("commit rows must belong to commit.run".into()));
        }
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(err)?;
        let run = &c.run;
        let n = tx
            .execute(
                "UPDATE runs SET workflow_id=?2, workflow_version=?3, trigger_ref=?4, state=?5, \
                 budget_tokens=?6, budget_usd_micros=?7, spent_tokens=?8, spent_usd_micros=?9, \
                 created_at=?10, updated_at=?11, wake=?12 WHERE run_id=?1",
                params![
                    run.run_id, run.workflow_id, run.workflow_version, run.trigger_ref,
                    json(&run.state)?, run.budget_tokens as i64, run.budget_usd_micros as i64,
                    run.spent_tokens as i64, run.spent_usd_micros as i64, run.created_at as i64,
                    run.updated_at as i64, run.wake.as_ref().map(json).transpose()?,
                ],
            )
            .map_err(err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("run {}", run.run_id)));
        }
        if let Some(s) = &c.step {
            tx.execute(
                "INSERT INTO steps (run_id, step_id, agent_id, model_id, attempt, status, \
                 idempotency_key, output, error) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) \
                 ON CONFLICT(run_id, step_id) DO UPDATE SET agent_id=excluded.agent_id, \
                 model_id=excluded.model_id, attempt=excluded.attempt, status=excluded.status, \
                 idempotency_key=excluded.idempotency_key, output=excluded.output, \
                 error=excluded.error",
                params![
                    s.run_id, s.step_id, s.agent_id, s.model_id, s.attempt, json(&s.status)?,
                    s.idempotency_key, s.output.as_ref().map(json).transpose()?, s.error,
                ],
            )
            .map_err(err)?;
        }
        for e in &c.events {
            tx.execute(
                "INSERT INTO events (run_id, seq, step_id, kind, payload, ts) \
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![e.run_id, e.seq as i64, e.step_id, json(&e.kind)?, json(&e.payload)?,
                    e.ts as i64],
            )
            .map_err(err)?;
        }
        tx.commit().map_err(err)
    }

    fn read_run(&self, run_id: &str) -> Result<Vec<Event>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT run_id, seq, step_id, kind, payload, ts FROM events \
                 WHERE run_id = ?1 ORDER BY seq",
            )
            .map_err(err)?;
        let rows = stmt.query_map([run_id], event_from).map_err(err)?;
        rows.map(|r| r.map_err(err)?).collect()
    }

    fn effect_result(&self, key: &str) -> Result<Option<serde_json::Value>> {
        self.get_json("SELECT result FROM effects WHERE key = ?1", key)
    }

    /// DUR-3: the key is the primary key, so a second record is a `Conflict`.
    fn record_effect(&self, key: &str, result: &serde_json::Value) -> Result<()> {
        let v = json(result)?;
        self.lock()
            .execute("INSERT INTO effects (key, result) VALUES (?1, ?2)", params![key, v])
            .map(|_| ())
            .map_err(err)
    }

    /// DUR-5: first write wins; later deliveries of the same key are ignored.
    fn deliver(&self, key: &str, payload: &serde_json::Value) -> Result<()> {
        let v = json(payload)?;
        self.lock()
            .execute("INSERT OR IGNORE INTO deliveries (key, payload) VALUES (?1, ?2)", params![key, v])
            .map(|_| ())
            .map_err(err)
    }

    fn delivery(&self, key: &str) -> Result<Option<serde_json::Value>> {
        self.get_json("SELECT payload FROM deliveries WHERE key = ?1", key)
    }

    /// One IMMEDIATE transaction: dedupe on key, else chain onto the last row.
    fn append_audit(&self, rec: &AuditRecord) -> Result<AuditRecord> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(err)?;
        let sql = format!("SELECT {AUDIT_COLS} FROM audit WHERE key = ?1");
        if let Some(r) = tx.query_row(&sql, [&rec.key], audit_from).optional().map_err(err)? {
            return r;
        }
        let last: Option<(i64, String)> = tx
            .query_row("SELECT seq, hash FROM audit ORDER BY seq DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(err)?;
        let mut r = rec.clone();
        (r.seq, r.prev_hash) = match last {
            Some((seq, hash)) => (seq as u64 + 1, hash),
            None => (0, String::new()),
        };
        r.hash = r.compute_hash();
        tx.execute(
            &format!("INSERT INTO audit ({AUDIT_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)"),
            params![
                r.seq as i64, r.ts as i64, r.run_id, r.step_id, r.agent_id, r.key, r.action, r.target,
                r.digest, decision_str(r.decision)?, r.approver, json(&r.detail)?, r.prev_hash, r.hash
            ],
        )
        .map_err(err)?;
        tx.commit().map_err(err)?;
        Ok(r)
    }

    fn read_audit(&self, run_id: Option<&str>) -> Result<Vec<AuditRecord>> {
        let conn = self.lock();
        let sql = format!("SELECT {AUDIT_COLS} FROM audit WHERE ?1 IS NULL OR run_id = ?1 ORDER BY seq");
        let mut stmt = conn.prepare(&sql).map_err(err)?;
        let rows = stmt.query_map([run_id], audit_from).map_err(err)?;
        rows.map(|r| r.map_err(err)?).collect()
    }

    /// FBK-2: one IMMEDIATE transaction: dedupe on `feedback_id`, else take the next seq.
    fn append_feedback(&self, fb: &Feedback) -> Result<Feedback> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(err)?;
        let sql = format!("SELECT {FEEDBACK_COLS} FROM feedback WHERE feedback_id = ?1");
        if let Some(r) = tx.query_row(&sql, [&fb.feedback_id], feedback_from).optional().map_err(err)? {
            return r;
        }
        let seq: i64 = tx
            .query_row("SELECT COALESCE(MAX(seq) + 1, 0) FROM feedback", [], |r| r.get(0))
            .map_err(err)?;
        let (kind, id) = target_key(&fb.target);
        tx.execute(
            "INSERT INTO feedback (seq, feedback_id, target, target_kind, target_id, source, verdict, \
             note, author, ts, detail) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                seq, fb.feedback_id, json(&fb.target)?, kind, id, enum_str(&fb.source)?,
                enum_str(&fb.verdict)?, fb.note, fb.author, fb.ts as i64, json(&fb.detail)?
            ],
        )
        .map_err(err)?;
        tx.commit().map_err(err)?;
        Ok(Feedback { seq: seq as u64, ..fb.clone() })
    }

    fn read_feedback(&self, from_seq: u64) -> Result<Vec<Feedback>> {
        let conn = self.lock();
        let sql = format!("SELECT {FEEDBACK_COLS} FROM feedback WHERE seq >= ?1 ORDER BY seq");
        let mut stmt = conn.prepare_cached(&sql).map_err(err)?;
        let rows = stmt.query_map([from_seq.min(i64::MAX as u64) as i64], feedback_from).map_err(err)?;
        rows.map(|r| r.map_err(err)?).collect()
    }

    fn get_cursor(&self, name: &str) -> Result<Option<u64>> {
        let v: Option<i64> = self
            .lock()
            .query_row("SELECT value FROM cursors WHERE name = ?1", [name], |r| r.get(0))
            .optional()
            .map_err(err)?;
        Ok(v.map(|v| v as u64))
    }

    fn set_cursor(&self, name: &str, value: u64) -> Result<()> {
        self.lock()
            .execute(
                "INSERT INTO cursors (name, value) VALUES (?1, ?2) \
                 ON CONFLICT(name) DO UPDATE SET value = excluded.value",
                params![name, value as i64],
            )
            .map(|_| ())
            .map_err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sf_core::{EventKind, StepStatus, Wake};
    use std::sync::Arc;

    fn run(id: &str, state: RunState, created: u64) -> Run {
        Run {
            run_id: id.into(), workflow_id: "wf".into(), workflow_version: 3,
            trigger_ref: "t".into(), state, budget_tokens: 1000, budget_usd_micros: 5_000_000,
            spent_tokens: 0, spent_usd_micros: 0, created_at: created, updated_at: created,
            wake: None,
        }
    }

    fn step(run_id: &str, id: &str, status: StepStatus) -> Step {
        Step {
            step_id: id.into(), run_id: run_id.into(), agent_id: Some("a".into()), model_id: None,
            attempt: 1, status, idempotency_key: sf_core::idempotency_key(run_id, id),
            output: Some(json!({"ok": true})), error: None,
        }
    }

    fn event(run_id: &str, seq: u64) -> Event {
        Event {
            run_id: run_id.into(), step_id: "s1".into(), seq, kind: EventKind::ToolResult,
            payload: json!({"seq": seq}), ts: 10 + seq,
        }
    }

    #[test]
    fn run_round_trip_and_duplicate_conflict() {
        let s = SqliteStore::open_in_memory().unwrap();
        let mut r = run("r1", RunState::Queued, 1);
        r.wake = Some(Wake::Event("k".into()));
        s.create_run(&r).unwrap();
        assert_eq!(s.get_run("r1").unwrap(), Some(r.clone()));
        assert_eq!(s.get_run("nope").unwrap(), None);
        assert!(matches!(s.create_run(&r), Err(Error::Conflict(_))));
    }

    #[test]
    fn commit_round_trip() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.create_run(&run("r1", RunState::Queued, 1)).unwrap();
        let mut r = run("r1", RunState::WaitingOnEvent, 1);
        r.wake = Some(Wake::At(99));
        r.spent_tokens = 42;
        let st = step("r1", "s1", StepStatus::Completed);
        s.commit(&Commit { run: r.clone(), step: Some(st.clone()), events: vec![event("r1", 0), event("r1", 1)] })
            .unwrap();
        assert_eq!(s.get_run("r1").unwrap(), Some(r));
        assert_eq!(s.get_steps("r1").unwrap(), vec![st]);
        let evs = s.read_run("r1").unwrap();
        assert_eq!(evs, vec![event("r1", 0), event("r1", 1)]);
        // Step upsert: same key replaces.
        let mut st2 = step("r1", "s1", StepStatus::Failed);
        st2.attempt = 2;
        st2.output = None;
        st2.error = Some("boom".into());
        s.commit(&Commit { run: run("r1", RunState::RetryingStep, 1), step: Some(st2.clone()), events: vec![] })
            .unwrap();
        assert_eq!(s.get_steps("r1").unwrap(), vec![st2]);
    }

    #[test]
    fn commit_missing_run_is_not_found() {
        let s = SqliteStore::open_in_memory().unwrap();
        let c = Commit { run: run("x", RunState::Running, 1), step: None, events: vec![] };
        assert!(matches!(s.commit(&c), Err(Error::NotFound(_))));
    }

    #[test]
    fn duplicate_seq_rolls_back_whole_commit() {
        let s = SqliteStore::open_in_memory().unwrap();
        let before = run("r1", RunState::Queued, 1);
        s.create_run(&before).unwrap();
        s.commit(&Commit { run: before.clone(), step: None, events: vec![event("r1", 0)] }).unwrap();
        let c = Commit {
            run: run("r1", RunState::Running, 1),
            step: Some(step("r1", "s1", StepStatus::Running)),
            events: vec![event("r1", 1), event("r1", 0)],
        };
        assert!(matches!(s.commit(&c), Err(Error::Conflict(_))));
        assert_eq!(s.get_run("r1").unwrap(), Some(before));
        assert!(s.get_steps("r1").unwrap().is_empty());
        assert_eq!(s.read_run("r1").unwrap().len(), 1);
    }

    #[test]
    fn events_are_append_only() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.create_run(&run("r1", RunState::Queued, 1)).unwrap();
        s.commit(&Commit { run: run("r1", RunState::Running, 1), step: None, events: vec![event("r1", 0)] })
            .unwrap();
        let conn = s.lock();
        let u = conn.execute("UPDATE events SET payload='{}' WHERE run_id='r1'", []);
        assert!(u.unwrap_err().to_string().contains("append-only"));
        let d = conn.execute("DELETE FROM events WHERE run_id='r1'", []);
        assert!(d.unwrap_err().to_string().contains("append-only"));
    }

    #[test]
    fn effect_ledger_conflicts_on_second_record() {
        let s = SqliteStore::open_in_memory().unwrap();
        assert_eq!(s.effect_result("k").unwrap(), None);
        s.record_effect("k", &json!({"pr": 7})).unwrap();
        assert!(matches!(s.record_effect("k", &json!({"pr": 8})), Err(Error::Conflict(_))));
        assert_eq!(s.effect_result("k").unwrap(), Some(json!({"pr": 7})));
    }

    #[test]
    fn deliver_keeps_first_payload() {
        let s = SqliteStore::open_in_memory().unwrap();
        assert_eq!(s.delivery("a").unwrap(), None);
        s.deliver("a", &json!("first")).unwrap();
        s.deliver("a", &json!("second")).unwrap();
        assert_eq!(s.delivery("a").unwrap(), Some(json!("first")));
    }

    #[test]
    fn list_runs_filters_and_orders_by_created_at() {
        let s = SqliteStore::open_in_memory().unwrap();
        for (id, st, t) in [
            ("c", RunState::Running, 30), ("a", RunState::Running, 10),
            ("b", RunState::Queued, 20), ("d", RunState::Succeeded, 5),
        ] {
            s.create_run(&run(id, st, t)).unwrap();
        }
        let ids = |v: Vec<Run>| v.into_iter().map(|r| r.run_id).collect::<Vec<_>>();
        assert_eq!(ids(s.list_runs(&[RunState::Running]).unwrap()), ["a", "c"]);
        assert_eq!(ids(s.list_runs(&[RunState::Running, RunState::Queued]).unwrap()), ["a", "b", "c"]);
        assert!(s.list_runs(&[]).unwrap().is_empty());
    }

    #[test]
    fn durable_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sf.db");
        {
            let s = SqliteStore::open(&path).unwrap();
            s.create_run(&run("r1", RunState::Queued, 1)).unwrap();
            s.commit(&Commit {
                run: run("r1", RunState::Running, 1),
                step: Some(step("r1", "s1", StepStatus::Completed)),
                events: vec![event("r1", 0)],
            })
            .unwrap();
            s.record_effect("k", &json!(1)).unwrap();
            s.deliver("d", &json!("approved")).unwrap();
        }
        let s = SqliteStore::open(&path).unwrap();
        assert_eq!(s.get_run("r1").unwrap().unwrap().state, RunState::Running);
        assert_eq!(s.get_steps("r1").unwrap().len(), 1);
        assert_eq!(s.read_run("r1").unwrap().len(), 1);
        assert_eq!(s.effect_result("k").unwrap(), Some(json!(1)));
        assert_eq!(s.delivery("d").unwrap(), Some(json!("approved")));
        let v: i64 = s.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);
    }

    #[test]
    fn concurrent_use_from_8_threads() {
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(SqliteStore::open(dir.path().join("sf.db")).unwrap());
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let s = s.clone();
                std::thread::spawn(move || {
                    let id = format!("r{i}");
                    s.create_run(&run(&id, RunState::Queued, i)).unwrap();
                    for seq in 0..10 {
                        let c = Commit { run: run(&id, RunState::Running, i), step: None, events: vec![event(&id, seq)] };
                        s.commit(&c).unwrap();
                    }
                    s.deliver("shared", &json!(i)).unwrap();
                    let _ = s.record_effect("once", &json!(i));
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(s.list_runs(&[RunState::Running]).unwrap().len(), 8);
        for i in 0..8 {
            assert_eq!(s.read_run(&format!("r{i}")).unwrap().len(), 10);
        }
        assert!(s.delivery("shared").unwrap().is_some());
        assert!(s.effect_result("once").unwrap().is_some());
    }

    fn arec(run_id: &str, key: &str) -> AuditRecord {
        AuditRecord {
            seq: 99, ts: 5, run_id: run_id.into(), step_id: "s1".into(), agent_id: Some("a".into()),
            key: key.into(), action: "net.fetch".into(), target: "example.com".into(),
            digest: "d".into(), decision: Decision::Blocked, approver: None,
            detail: json!({"why": "egress"}), prev_hash: "x".into(), hash: "y".into(),
        }
    }

    #[test]
    fn audit_round_trip() {
        let s = SqliteStore::open_in_memory().unwrap();
        let a = s.append_audit(&arec("r1", "k0")).unwrap();
        let b = s.append_audit(&arec("r2", "k1")).unwrap();
        assert_eq!((a.seq, a.prev_hash.as_str()), (0, ""));
        assert_eq!((b.seq, &b.prev_hash), (1, &a.hash));
        assert_eq!(b.hash, b.compute_hash());
        assert_eq!(s.read_audit(None).unwrap(), vec![a.clone(), b.clone()]);
        assert_eq!(s.read_audit(Some("r2")).unwrap(), vec![b]);
        assert!(s.read_audit(Some("none")).unwrap().is_empty());
    }

    #[test]
    fn audit_dedupes_on_key() {
        let s = SqliteStore::open_in_memory().unwrap();
        let first = s.append_audit(&arec("r1", "k")).unwrap();
        let mut again = arec("r1", "k");
        again.action = "other".into();
        assert_eq!(s.append_audit(&again).unwrap(), first);
        assert_eq!(s.read_audit(None).unwrap().len(), 1);
    }

    #[test]
    fn audit_chain_valid_under_8_threads() {
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(SqliteStore::open(dir.path().join("sf.db")).unwrap());
        let hs: Vec<_> = (0..8)
            .map(|t| {
                let s = s.clone();
                std::thread::spawn(move || {
                    for i in 0..(if t < 2 { 7 } else { 6 }) {
                        s.append_audit(&arec(&format!("r{t}"), &format!("k{t}-{i}"))).unwrap();
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let log = s.read_audit(None).unwrap();
        assert_eq!(log.len(), 50);
        assert!(log.iter().enumerate().all(|(i, r)| r.seq == i as u64));
        assert_eq!(sf_core::verify_chain(&log), Ok(()));
    }

    #[test]
    fn audit_is_append_only() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.append_audit(&arec("r1", "k")).unwrap();
        let conn = s.lock();
        let u = conn.execute("UPDATE audit SET target='x'", []);
        assert!(u.unwrap_err().to_string().contains("append-only"));
        let d = conn.execute("DELETE FROM audit", []);
        assert!(d.unwrap_err().to_string().contains("append-only"));
    }

    #[test]
    fn audit_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sf.db");
        {
            let s = SqliteStore::open(&path).unwrap();
            s.append_audit(&arec("r1", "a")).unwrap();
            s.append_audit(&arec("r1", "b")).unwrap();
        }
        let s = SqliteStore::open(&path).unwrap();
        s.append_audit(&arec("r1", "c")).unwrap();
        let log = s.read_audit(None).unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(sf_core::verify_chain(&log), Ok(()));
    }

    #[test]
    fn m2_schema_db_migrates_and_keeps_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sf.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(MIGRATIONS[0]).unwrap();
            c.pragma_update(None, "user_version", 1).unwrap();
            run_insert_for_test(&c);
        }
        let s = SqliteStore::open(&path).unwrap();
        assert_eq!(s.read_run("r1").unwrap().len(), 1);
        assert!(s.append_audit(&arec("r1", "k")).is_ok());
        let v: i64 = s.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);
    }

    fn fb(id: &str, target: FeedbackTarget) -> Feedback {
        Feedback {
            seq: 99, feedback_id: id.into(), target, source: sf_core::FeedbackSource::Human,
            verdict: sf_core::FeedbackVerdict::Helped, note: Some("n".into()), author: "alice".into(),
            ts: 5, detail: json!({"k": 1}),
        }
    }

    fn entry(e: &str) -> FeedbackTarget {
        FeedbackTarget::Entry { entry_id: e.into() }
    }

    #[test]
    fn feedback_assigns_seq_from_zero_and_dedupes() {
        let s = SqliteStore::open_in_memory().unwrap();
        let a = s.append_feedback(&fb("a", entry("e1"))).unwrap();
        let b = s.append_feedback(&fb("b", FeedbackTarget::Step { run_id: "r".into(), step_id: "s".into() })).unwrap();
        assert_eq!((a.seq, b.seq), (0, 1));
        let mut again = fb("a", entry("other"));
        again.verdict = sf_core::FeedbackVerdict::Wrong;
        assert_eq!(s.append_feedback(&again).unwrap(), a);
        let all = s.read_feedback(0).unwrap();
        assert_eq!(all, vec![a, b.clone()]);
        assert_eq!(s.read_feedback(1).unwrap(), vec![b]);
        assert!(s.read_feedback(2).unwrap().is_empty());
    }

    #[test]
    fn feedback_is_append_only_and_cursors_roundtrip() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.append_feedback(&fb("a", entry("e1"))).unwrap();
        assert!(s.lock().execute("UPDATE feedback SET note = 'x'", []).is_err());
        assert!(s.lock().execute("DELETE FROM feedback", []).is_err());
        assert_eq!(s.get_cursor("feedback-sync").unwrap(), None);
        s.set_cursor("feedback-sync", 4).unwrap();
        s.set_cursor("feedback-sync", 7).unwrap();
        assert_eq!(s.get_cursor("feedback-sync").unwrap(), Some(7));
        assert_eq!(s.get_cursor("other").unwrap(), None);
    }

    #[test]
    fn feedback_concurrent_appends_get_distinct_seqs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sf.db");
        SqliteStore::open(&path).unwrap();
        let hs: Vec<_> = (0..8)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let s = SqliteStore::open(&path).unwrap();
                    for i in 0..10 {
                        // Ids wrap mod 50, so threads collide, so dedupe is exercised too.
                        s.append_feedback(&fb(&format!("f{}", (t * 10 + i) % 50), entry("e"))).unwrap();
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let all = SqliteStore::open(&path).unwrap().read_feedback(0).unwrap();
        assert_eq!(all.len(), 50);
        assert!(all.iter().enumerate().all(|(i, f)| f.seq == i as u64));
    }

    #[test]
    fn feedback_survives_reopen_and_v2_db_migrates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sf.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(MIGRATIONS[0]).unwrap();
            c.execute_batch(MIGRATIONS[1]).unwrap();
            c.pragma_update(None, "user_version", 2).unwrap();
            run_insert_for_test(&c);
        }
        {
            let s = SqliteStore::open(&path).unwrap();
            assert_eq!(s.read_run("r1").unwrap().len(), 1);
            s.append_feedback(&fb("a", FeedbackTarget::Template { section: "hdr".into() })).unwrap();
            s.set_cursor("c", 1).unwrap();
        }
        let s = SqliteStore::open(&path).unwrap();
        let all = s.read_feedback(0).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].detail, json!({"k": 1}));
        assert_eq!(s.get_cursor("c").unwrap(), Some(1));
        let v: i64 = s.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 3);
    }

    fn run_insert_for_test(c: &Connection) {
        insert_run(c, &run("r1", RunState::Queued, 1)).unwrap();
        c.execute(
            r#"INSERT INTO events (run_id, seq, step_id, kind, payload, ts) VALUES ('r1',0,'s','"ToolCall"','{}',1)"#,
            [],
        )
        .unwrap();
    }
}

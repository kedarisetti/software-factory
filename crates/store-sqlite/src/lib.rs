//! SQLite-backed durable store (DUR-1 to DUR-3, DUR-5). Depends only on sf-core.
//!
//! WAL plus `synchronous=FULL` makes every commit survive power loss (DUR-1).
//! The event log is append-only, enforced by triggers (DUR-2).

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, ErrorCode, OptionalExtension, Row, TransactionBehavior};
use serde::de::DeserializeOwned;
use serde::Serialize;
use sf_core::{Commit, Error, Event, Result, Run, RunState, Step, Store};

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
}

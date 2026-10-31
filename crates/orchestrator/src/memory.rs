//! In-memory `Store` test double with the same contract as the SQLite store: atomic commit,
//! duplicate-seq `Conflict`, write-once effect ledger and inbox, hash-chained audit log deduped by key.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use sf_core::{AuditRecord, Feedback, Commit, Error, Event, Result, Run, RunState, Step, Store};

#[derive(Default)]
struct Inner {
    runs: HashMap<String, Run>,
    steps: HashMap<(String, String), Step>,
    events: HashMap<String, Vec<Event>>,
    effects: HashMap<String, Value>,
    inbox: HashMap<String, Value>,
    audit: Vec<AuditRecord>,
    feedback: Vec<Feedback>,
    cursors: HashMap<String, u64>,
}

#[derive(Default)]
pub struct MemoryStore(Mutex<Inner>);

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Store for MemoryStore {
    fn create_run(&self, run: &Run) -> Result<()> {
        let mut g = self.0.lock().unwrap();
        if g.runs.contains_key(&run.run_id) {
            return Err(Error::Conflict(format!("run {}", run.run_id)));
        }
        g.runs.insert(run.run_id.clone(), run.clone());
        Ok(())
    }

    fn get_run(&self, run_id: &str) -> Result<Option<Run>> {
        Ok(self.0.lock().unwrap().runs.get(run_id).cloned())
    }

    fn list_runs(&self, states: &[RunState]) -> Result<Vec<Run>> {
        let g = self.0.lock().unwrap();
        let mut v: Vec<Run> = g.runs.values().filter(|r| states.contains(&r.state)).cloned().collect();
        v.sort_by(|a, b| (a.created_at, &a.run_id).cmp(&(b.created_at, &b.run_id)));
        Ok(v)
    }

    fn get_steps(&self, run_id: &str) -> Result<Vec<Step>> {
        let g = self.0.lock().unwrap();
        let mut v: Vec<Step> = g.steps.values().filter(|s| s.run_id == run_id).cloned().collect();
        v.sort_by(|a, b| a.step_id.cmp(&b.step_id));
        Ok(v)
    }

    fn commit(&self, c: &Commit) -> Result<()> {
        let mut g = self.0.lock().unwrap();
        let id = &c.run.run_id;
        if !g.runs.contains_key(id) {
            return Err(Error::NotFound(format!("run {id}")));
        }
        // Validate everything before applying anything, so a conflict leaves no partial write.
        let log = g.events.get(id);
        for (i, e) in c.events.iter().enumerate() {
            let dup = log.is_some_and(|l| l.iter().any(|x| x.seq == e.seq)) || c.events[..i].iter().any(|x| x.seq == e.seq);
            if e.run_id != *id || dup {
                return Err(Error::Conflict(format!("event seq {} on {id}", e.seq)));
            }
        }
        g.runs.insert(id.clone(), c.run.clone());
        if let Some(s) = &c.step {
            g.steps.insert((s.run_id.clone(), s.step_id.clone()), s.clone());
        }
        let log = g.events.entry(id.clone()).or_default();
        log.extend(c.events.iter().cloned());
        log.sort_by_key(|e| e.seq);
        Ok(())
    }

    fn read_run(&self, run_id: &str) -> Result<Vec<Event>> {
        Ok(self.0.lock().unwrap().events.get(run_id).cloned().unwrap_or_default())
    }

    fn effect_result(&self, key: &str) -> Result<Option<Value>> {
        Ok(self.0.lock().unwrap().effects.get(key).cloned())
    }

    fn record_effect(&self, key: &str, result: &Value) -> Result<()> {
        let mut g = self.0.lock().unwrap();
        if g.effects.contains_key(key) {
            return Err(Error::Conflict(format!("effect {key}")));
        }
        g.effects.insert(key.into(), result.clone());
        Ok(())
    }

    fn deliver(&self, key: &str, payload: &Value) -> Result<()> {
        self.0.lock().unwrap().inbox.entry(key.into()).or_insert_with(|| payload.clone());
        Ok(())
    }

    fn delivery(&self, key: &str) -> Result<Option<Value>> {
        Ok(self.0.lock().unwrap().inbox.get(key).cloned())
    }

    fn append_audit(&self, rec: &AuditRecord) -> Result<AuditRecord> {
        let mut g = self.0.lock().unwrap();
        if let Some(r) = g.audit.iter().find(|r| r.key == rec.key) {
            return Ok(r.clone());
        }
        let mut r = rec.clone();
        r.seq = g.audit.len() as u64;
        r.prev_hash = g.audit.last().map(|p| p.hash.clone()).unwrap_or_default();
        r.hash = r.compute_hash();
        g.audit.push(r.clone());
        Ok(r)
    }

    fn read_audit(&self, run_id: Option<&str>) -> Result<Vec<AuditRecord>> {
        let g = self.0.lock().unwrap();
        Ok(g.audit.iter().filter(|r| run_id.is_none_or(|id| r.run_id == id)).cloned().collect())
    }

    fn append_feedback(&self, fb: &Feedback) -> Result<Feedback> {
        let mut g = self.0.lock().unwrap();
        if let Some(f) = g.feedback.iter().find(|f| f.feedback_id == fb.feedback_id) {
            return Ok(f.clone());
        }
        let f = Feedback { seq: g.feedback.len() as u64, ..fb.clone() };
        g.feedback.push(f.clone());
        Ok(f)
    }

    fn read_feedback(&self, from_seq: u64) -> Result<Vec<Feedback>> {
        Ok(self.0.lock().unwrap().feedback.iter().filter(|f| f.seq >= from_seq).cloned().collect())
    }

    fn get_cursor(&self, name: &str) -> Result<Option<u64>> {
        Ok(self.0.lock().unwrap().cursors.get(name).copied())
    }

    fn set_cursor(&self, name: &str, value: u64) -> Result<()> {
        self.0.lock().unwrap().cursors.insert(name.into(), value);
        Ok(())
    }
}

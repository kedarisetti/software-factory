//! Audit writer bound to one run and step (GOV audit log). Every record carries run, step, agent, time.

use std::sync::Arc;

use serde_json::Value;
use sf_core::{AuditRecord, Clock, Decision, Result, RunId, Store};

#[derive(Clone)]
pub(crate) struct Auditor {
    pub store: Arc<dyn Store>,
    pub clock: Arc<dyn Clock>,
    pub run_id: RunId,
    pub step_id: String,
    pub agent_id: Option<String>,
}

impl Auditor {
    /// Append one record. The store dedupes on `key`, so a retried step logs an action once.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        key: String,
        action: &str,
        target: &str,
        digest: &str,
        decision: Decision,
        approver: Option<&str>,
        detail: Value,
    ) -> Result<AuditRecord> {
        self.store.append_audit(&AuditRecord {
            seq: 0,
            ts: self.clock.now(),
            run_id: self.run_id.clone(),
            step_id: self.step_id.clone(),
            agent_id: self.agent_id.clone(),
            key,
            action: action.into(),
            target: target.into(),
            digest: digest.into(),
            decision,
            approver: approver.map(String::from),
            detail,
            prev_hash: String::new(),
            hash: String::new(),
        })
    }
}

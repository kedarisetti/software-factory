//! The approval gate for new connectors (spec, Safety and governance: a new tool, model or connector
//! needs human approval). A `command` or `mcp` source runs a local process, so it is disabled until a
//! human approves it, and the approval binds to the source's digest: any change disables it again.
//! Webhook sources run nothing (the shared secret is their gate) and need no approval.
//!
//! State lives in the audit log. Approve writes `new_capability` / `intake:<name>` / digest /
//! Approved / approver; revoke writes the same with Denied. The latest approve/revoke for the
//! name and the *current* digest decides. Refused polls are audited as Blocked.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sf_core::{AuditRecord, Clock, Decision, Store, SystemClock};

use super::config::{Loaded, ProcSrc};

const RUN: &str = "intake";
const STEP: &str = "connectors";
pub const ACTION: &str = "new_capability";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// No approval was ever given for this digest.
    Pending,
    Approved {
        by: String,
    },
    Revoked {
        by: String,
    },
}

impl Approval {
    pub fn enabled(&self) -> bool {
        matches!(self, Approval::Approved { .. })
    }

    pub fn label(&self) -> &'static str {
        match self {
            Approval::Pending => "pending",
            Approval::Approved { .. } => "approved",
            Approval::Revoked { .. } => "revoked",
        }
    }
}

fn target(name: &str) -> String {
    format!("intake:{name}")
}

fn latest(store: &dyn Store, name: &str, digest: &str) -> Result<Option<AuditRecord>, String> {
    let t = target(name);
    let log = store.read_audit(None).map_err(|e| e.to_string())?;
    Ok(log
        .into_iter()
        .rev()
        .find(|r| r.action == ACTION && r.target == t && r.digest == digest))
}

pub fn state(store: &dyn Store, name: &str, digest: &str) -> Result<Approval, String> {
    let log = store.read_audit(None).map_err(|e| e.to_string())?;
    let t = target(name);
    let hit = log.into_iter().rev().find(|r| {
        r.action == ACTION && r.target == t && r.digest == digest && r.decision != Decision::Blocked
    });
    Ok(match hit {
        Some(r) if r.decision == Decision::Approved => Approval::Approved {
            by: r.approver.unwrap_or_default(),
        },
        Some(r) => Approval::Revoked {
            by: r.approver.unwrap_or_default(),
        },
        None => Approval::Pending,
    })
}

fn append(
    store: &dyn Store,
    name: &str,
    digest: &str,
    decision: Decision,
    approver: Option<&str>,
    detail: Value,
) -> Result<AuditRecord, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let key = format!("{ACTION}:{name}:{digest}:{decision:?}:{nanos}");
    store
        .append_audit(&AuditRecord {
            seq: 0,
            ts: SystemClock.now(),
            run_id: RUN.into(),
            step_id: STEP.into(),
            agent_id: None,
            key,
            action: ACTION.into(),
            target: target(name),
            digest: digest.into(),
            decision,
            approver: approver.map(String::from),
            detail,
            prev_hash: String::new(),
            hash: String::new(),
        })
        .map_err(|e| e.to_string())
}

/// Why a poll was refused; also audits Blocked once per (source, digest) until the state changes.
pub fn refuse(store: &dyn Store, name: &str, m: &ProcSrc, st: &Approval) -> Result<String, String> {
    let digest = m.digest();
    let already = latest(store, name, &digest)?.is_some_and(|r| r.decision == Decision::Blocked);
    if !already {
        append(
            store,
            name,
            &digest,
            Decision::Blocked,
            None,
            json!({"reason": "connector not approved", "state": st.label(), "kind": m.kind, "tool": m.tool}),
        )?;
    }
    Ok(format!(
        "source {name} is disabled: {} connector {} (digest {}). A human must run: sf intake connectors approve {name} --as <you> --config <intake.json>",
        m.kind,
        match st {
            Approval::Pending => "has not been approved".to_string(),
            Approval::Revoked { by } => format!("approval was revoked by {by}"),
            Approval::Approved { .. } => "is approved".to_string(),
        },
        &digest[..12]
    ))
}

fn proc<'a>(l: &'a Loaded, name: &str) -> Result<&'a ProcSrc, String> {
    match l.source(name) {
        Some(s) => s.proc().ok_or_else(|| {
            format!(
                "source {name} is a webhook and needs no approval (its shared secret is the gate)"
            )
        }),
        None => Err(format!("no source named {name:?} in the config")),
    }
}

pub fn list(store: &dyn Store, l: &Loaded) -> Result<Value, String> {
    let mut rows = Vec::new();
    for s in &l.sources {
        rows.push(match s.proc() {
            Some(m) => {
                let digest = m.digest();
                let st = state(store, &s.name, &digest)?;
                json!({"name": s.name, "kind": m.kind, "chat": m.chat, "enabled": st.enabled(), "approval": st.label(),
                       "approved_by": match &st { Approval::Approved { by } => json!(by), _ => Value::Null },
                       "digest": digest, "tool": m.tool})
            }
            None => json!({"name": s.name, "kind": s.kind_name(), "chat": s.is_chat(), "enabled": true, "approval": "not required", "approved_by": null, "digest": null}),
        });
    }
    Ok(json!(rows))
}

pub fn approve(store: &dyn Store, l: &Loaded, name: &str, by: &str) -> Result<Value, String> {
    let m = proc(l, name)?;
    let digest = m.digest();
    append(
        store,
        name,
        &digest,
        Decision::Approved,
        Some(by),
        json!({"kind": m.kind, "command": m.command, "tool": m.tool, "env_vars": m.env_refs().values().collect::<Vec<_>>()}),
    )?;
    Ok(json!({"name": name, "approval": "approved", "digest": digest, "by": by}))
}

pub fn revoke(store: &dyn Store, l: &Loaded, name: &str, by: &str) -> Result<Value, String> {
    let m = proc(l, name)?;
    let digest = m.digest();
    append(
        store,
        name,
        &digest,
        Decision::Denied,
        Some(by),
        json!({"revoked": true}),
    )?;
    Ok(json!({"name": name, "approval": "revoked", "digest": digest, "by": by}))
}

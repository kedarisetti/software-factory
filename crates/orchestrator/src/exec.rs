//! Sandboxed tool execution with credential injection, redaction and audit (SBX-1, SBX-2).

use std::sync::Arc;

use serde_json::{json, Value};
use sf_core::{
    Clock, Credentials, Decision, Error, KillSwitch, Lease, Limits, Result, Sandbox, SecretRef, ToolCall, ToolOutput,
};

use crate::audit::Auditor;

/// Everything a tool call needs, cloned out of the step context so it can run inside the ledger closure.
pub(crate) struct ToolRun {
    pub sandbox: Arc<dyn Sandbox>,
    pub kill: KillSwitch,
    pub creds: Option<Arc<dyn Credentials>>,
    pub clock: Arc<dyn Clock>,
    pub audit: Auditor,
    pub scopes: Vec<String>,
    pub ttl_ms: u64,
}

/// The engine owns the workspace name: the run id with anything outside `[A-Za-z0-9._-]` replaced.
pub(crate) fn workspace_name(run_id: &str) -> String {
    let s: String = run_id.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect();
    match s.strip_prefix('.') {
        Some(rest) => format!("_{rest}"),
        None => s,
    }
}

impl ToolRun {
    pub fn run(&self, key: &str, mut call: ToolCall, limits: &Limits) -> Result<Value> {
        let secrets = std::mem::take(&mut call.secrets);
        let mut leased = vec![];
        for s in &secrets {
            let lease = self.lease(key, s)?;
            call.env.push((s.env.clone(), lease.value.clone()));
            leased.push(lease);
        }
        self.audit_start(key, &call, &secrets, &leased)?;
        let mut out = self.sandbox.run(&call, limits, &self.kill).map_err(|e| redact_err(e, &leased))?;
        out.stdout = redact(&out.stdout, &leased);
        out.stderr = redact(&out.stderr, &leased);
        self.audit_egress(key, &out)?;
        serde_json::to_value(out).map_err(|e| Error::Invalid(e.to_string()))
    }

    fn lease(&self, key: &str, s: &SecretRef) -> Result<Lease> {
        if !self.scopes.contains(&s.scope) {
            return self.block(key, s, "scope not granted by policy");
        }
        let Some(creds) = &self.creds else { return self.block(key, s, "no credentials provider") };
        let lease = match creds.lease(&s.scope, &self.audit.run_id, self.ttl_ms) {
            Ok(l) => l,
            Err(e) => return self.block(key, s, &e.to_string()),
        };
        if lease.expires_at <= self.clock.now() {
            return self.block(key, s, "lease expired");
        }
        Ok(lease)
    }

    fn block<T>(&self, key: &str, s: &SecretRef, why: &str) -> Result<T> {
        let detail = json!({"env": s.env, "reason": why});
        self.audit.record(format!("{key}:secret:{}:{}:blocked", s.scope, s.env), "secret", &s.scope, "", Decision::Blocked, None, detail)?;
        Err(Error::Forbidden(format!("secret {}: {why}", s.scope)))
    }

    fn audit_start(&self, key: &str, call: &ToolCall, secrets: &[SecretRef], leased: &[Lease]) -> Result<()> {
        let scopes: Vec<_> = secrets.iter().map(|s| &s.scope).collect();
        let detail = json!({"args": call.args, "egress": call.egress, "scopes": scopes});
        self.audit.record(format!("{key}:tool"), "tool", &call.program, "", Decision::Auto, None, detail)?;
        for (s, l) in secrets.iter().zip(leased) {
            let detail = json!({"env": s.env, "expires_at": l.expires_at}); // never the value
            self.audit.record(format!("{key}:secret:{}:{}", s.scope, s.env), "secret", &s.scope, "", Decision::Auto, None, detail)?;
        }
        Ok(())
    }

    fn audit_egress(&self, key: &str, out: &ToolOutput) -> Result<()> {
        for a in out.egress.iter().filter(|a| !a.allowed) {
            let target = format!("{}:{}", a.host, a.port);
            self.audit.record(format!("{key}:egress:{target}"), "egress", &target, "", Decision::Blocked, None, json!({}))?;
        }
        Ok(())
    }
}

/// Replace every leased value with `[REDACTED:<scope>]` (longest first so overlaps cannot leak a tail).
pub(crate) fn redact(text: &str, leased: &[Lease]) -> String {
    let mut ls: Vec<_> = leased.iter().filter(|l| !l.value.is_empty()).collect();
    ls.sort_by_key(|l| std::cmp::Reverse(l.value.len()));
    ls.iter().fold(text.to_string(), |t, l| t.replace(&l.value, &format!("[REDACTED:{}]", l.scope)))
}

fn redact_err(e: Error, leased: &[Lease]) -> Error {
    let r = |s: String| redact(&s, leased);
    match e {
        Error::Storage(s) => Error::Storage(r(s)),
        Error::Conflict(s) => Error::Conflict(r(s)),
        Error::NotFound(s) => Error::NotFound(r(s)),
        Error::Invalid(s) => Error::Invalid(r(s)),
        Error::BudgetExceeded(s) => Error::BudgetExceeded(r(s)),
        Error::AwaitingApproval(s) => Error::AwaitingApproval(r(s)),
        Error::Denied(s) => Error::Denied(r(s)),
        Error::Injection(s) => Error::Injection(r(s)),
        Error::Forbidden(s) => Error::Forbidden(r(s)),
        e @ (Error::Timeout(_) | Error::Killed) => e,
    }
}

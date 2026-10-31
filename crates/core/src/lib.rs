//! Shared types and plugin traits. Plugins depend on this crate, never on each other.

pub mod untrusted;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub type RunId = String;
pub type StepId = String;
/// Unix time in milliseconds.
pub type Timestamp = u64;

/// Run lifecycle states (see docs/spec.md, "Run lifecycle").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RunState {
    Queued,
    Running,
    AwaitingApproval,
    WaitingOnEvent,
    RetryingStep,
    Escalated,
    Succeeded,
    Cancelled,
}

impl RunState {
    pub fn is_terminal(self) -> bool {
        matches!(self, RunState::Succeeded | RunState::Cancelled)
    }

    /// A parked run holds no compute and waits on a `Wake` (DUR-5).
    pub fn is_parked(self) -> bool {
        matches!(self, RunState::AwaitingApproval | RunState::WaitingOnEvent)
    }
}

/// What a parked run waits for (DUR-5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Wake {
    /// Durable timer: resume at or after this time.
    At(Timestamp),
    /// Resume when an event with this key is delivered (approval, CI result, reply).
    Event(String),
}

/// Run record (docs/spec.md, "Data model").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub run_id: RunId,
    pub workflow_id: String,
    /// Pinned at creation. In-flight runs finish on this version (DUR-6).
    pub workflow_version: u32,
    pub trigger_ref: String,
    pub state: RunState,
    pub budget_tokens: u64,
    pub budget_usd_micros: u64,
    pub spent_tokens: u64,
    pub spent_usd_micros: u64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// Set while the run is parked or waiting to retry a step.
    pub wake: Option<Wake>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Running,
    /// Checkpointed. `Step::output` holds the result; the step never runs again (DUR-1).
    Completed,
    /// Last attempt failed. Retried alone, per DUR-4.
    Failed,
    Parked,
}

/// Step record. One row per (run_id, step_id); `attempt` counts tries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub step_id: StepId,
    pub run_id: RunId,
    pub agent_id: Option<String>,
    pub model_id: Option<String>,
    pub attempt: u32,
    pub status: StepStatus,
    pub idempotency_key: String,
    /// Checkpoint: the step's result once `Completed`.
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// Deterministic side-effect key: hex(sha256(run_id, step_id)) (DUR-3).
/// A step with several side effects derives one per effect with `effect_key`.
pub fn idempotency_key(run_id: &str, step_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(run_id.as_bytes());
    h.update([0u8]);
    h.update(step_id.as_bytes());
    hex(&h.finalize())
}

/// Key for the `n`th side effect inside one step.
pub fn effect_key(step_key: &str, n: u32) -> String {
    format!("{step_key}:{n}")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Append-only event log entry (DUR-2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub run_id: RunId,
    pub step_id: StepId,
    /// Per-run sequence, assigned by the writer, starting at 0. A duplicate seq is a `Conflict`.
    pub seq: u64,
    pub kind: EventKind,
    pub payload: serde_json::Value,
    pub ts: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    ModelResponse,
    ToolCall,
    ToolResult,
    Retrieval,
    Approval,
    Feedback,
    Error,
    /// Run or step state change (checkpoint, park, resume, retry, escalate).
    State,
}

/// One checkpoint: written in a single transaction, or not at all (DUR-1).
#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub run: Run,
    pub step: Option<Step>,
    pub events: Vec<Event>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("storage: {0}")]
    Storage(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("killed")]
    Killed,
    #[error("budget exceeded: {0}")]
    BudgetExceeded(String),
    /// A gated action needs a human answer; the engine parks the run on this key.
    #[error("awaiting approval: {0}")]
    AwaitingApproval(String),
    /// A human denied a gated action. The engine escalates the run; no retry.
    #[error("denied: {0}")]
    Denied(String),
    /// Untrusted input looks like instructions (SBX-3). The engine escalates the run; no retry.
    #[error("possible injection: {0}")]
    Injection(String),
    /// The policy forbids this (scope not granted, lease expired, fs or network isolation unavailable).
    #[error("forbidden: {0}")]
    Forbidden(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Durable store: event log, run and step records, side-effect ledger, event inbox.
/// Implementations must make each `commit` atomic and durable before returning.
pub trait Store: Send + Sync {
    /// Insert a new run. `Conflict` if `run_id` exists.
    fn create_run(&self, run: &Run) -> Result<()>;
    fn get_run(&self, run_id: &str) -> Result<Option<Run>>;
    /// Runs in any of `states`, ordered by `created_at`.
    fn list_runs(&self, states: &[RunState]) -> Result<Vec<Run>>;
    fn get_steps(&self, run_id: &str) -> Result<Vec<Step>>;

    /// Atomically: replace the run row, upsert the step (if any), append the events.
    /// `NotFound` if the run does not exist; `Conflict` on a duplicate event seq.
    fn commit(&self, commit: &Commit) -> Result<()>;

    /// Full event log for a run, ordered by seq (DUR-2).
    fn read_run(&self, run_id: &str) -> Result<Vec<Event>>;

    /// Side-effect ledger (DUR-3). Returns the recorded result for `key`, if any.
    fn effect_result(&self, key: &str) -> Result<Option<serde_json::Value>>;
    /// Record a side effect's result. `Conflict` if `key` is already recorded.
    fn record_effect(&self, key: &str, result: &serde_json::Value) -> Result<()>;

    /// Deliver an external event (approval, CI result, reply) to whoever waits on `key`.
    /// Durable: a delivery made while no process runs is still seen on restart.
    /// Delivering the same key twice keeps the first payload.
    fn deliver(&self, key: &str, payload: &serde_json::Value) -> Result<()>;
    /// Read a delivered payload without consuming it.
    fn delivery(&self, key: &str) -> Result<Option<serde_json::Value>>;

    /// Append to the audit log (append-only, hash-chained). The store assigns `seq`, `prev_hash` and
    /// `hash`. A record whose `key` is already present is not appended again, so a re-run after a
    /// crash logs an action once; the existing record is returned.
    fn append_audit(&self, rec: &AuditRecord) -> Result<AuditRecord>;
    /// Audit records in `seq` order, all runs or one run.
    fn read_audit(&self, run_id: Option<&str>) -> Result<Vec<AuditRecord>>;
}

/// Who or what allowed an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// The policy needs no approval for this action.
    Auto,
    Approved,
    Denied,
    /// Stopped by policy (egress not allowlisted, scope not granted, injection detected).
    Blocked,
}

/// One audit log row: every action records its run, step, agent and the approval that allowed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// Assigned by the store, from 0.
    pub seq: u64,
    pub ts: Timestamp,
    pub run_id: RunId,
    pub step_id: StepId,
    pub agent_id: Option<String>,
    /// Dedupe key (for actions: the effect key plus the decision). Unique across the log.
    pub key: String,
    /// `tool`, `egress`, `secret`, or an `ActionKind` name such as `merge_protected`.
    pub action: String,
    pub target: String,
    /// sha256 of the canonical action JSON; an approval is bound to it.
    pub digest: String,
    pub decision: Decision,
    pub approver: Option<String>,
    pub detail: serde_json::Value,
    /// Assigned by the store: hash of the previous row ("" for the first) and of this row.
    pub prev_hash: String,
    pub hash: String,
}

impl AuditRecord {
    /// hex(sha256(prev_hash, seq, ts, run_id, step_id, agent_id, key, action, target, digest,
    /// decision, approver, detail)), fields separated by NUL. Stores call this; `verify_chain` checks it.
    pub fn compute_hash(&self) -> String {
        let mut h = Sha256::new();
        let parts = [
            self.prev_hash.clone(),
            self.seq.to_string(),
            self.ts.to_string(),
            self.run_id.clone(),
            self.step_id.clone(),
            self.agent_id.clone().unwrap_or_default(),
            self.key.clone(),
            self.action.clone(),
            self.target.clone(),
            self.digest.clone(),
            serde_json::to_string(&self.decision).unwrap_or_default(),
            self.approver.clone().unwrap_or_default(),
            self.detail.to_string(),
        ];
        for p in parts {
            h.update(p.as_bytes());
            h.update([0u8]);
        }
        hex(&h.finalize())
    }
}

/// Checks a full log read with `read_audit(None)`: seqs are 0.., each `prev_hash` links to the row
/// before, each `hash` matches. Returns the seq of the first bad row.
pub fn verify_chain(log: &[AuditRecord]) -> std::result::Result<(), u64> {
    let mut prev = String::new();
    for (i, r) in log.iter().enumerate() {
        if r.seq != i as u64 || r.prev_hash != prev || r.hash != r.compute_hash() {
            return Err(r.seq);
        }
        prev = r.hash.clone();
    }
    Ok(())
}

/// What a step wants to do in the world. The gate table maps the kind to a gate.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    MergeProtected,
    Deploy,
    ExternalMessage,
    /// New tool, model or connector.
    NewCapability,
    DraftPr,
    Branch,
    OwnTicketComment,
    /// Anything else, by name. Gated by default (fail closed).
    Other(String),
}

impl ActionKind {
    pub fn name(&self) -> String {
        match self {
            ActionKind::Other(s) => format!("other:{s}"),
            k => serde_json::to_value(k).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
        }
    }
}

/// A side effect a step asks the engine to perform through a gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub kind: ActionKind,
    /// What it acts on: `org/repo#12`, `prod`, `mail:someone@example.com`.
    pub target: String,
    /// Everything the approver needs to see; part of the digest.
    pub detail: serde_json::Value,
}

impl Action {
    /// hex(sha256(kind name, target, canonical detail)). serde_json maps keep keys sorted, so the
    /// same action always gives the same digest.
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        for p in [self.kind.name(), self.target.clone(), self.detail.to_string()] {
            h.update(p.as_bytes());
            h.update([0u8]);
        }
        hex(&h.finalize())
    }
}

/// A short-lived credential (SBX-2). `value` never goes into an event, ledger row, audit record or
/// model context; `Debug` hides it.
#[derive(Clone, PartialEq, Eq)]
pub struct Lease {
    pub scope: String,
    pub value: String,
    pub expires_at: Timestamp,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease").field("scope", &self.scope).field("value", &"[REDACTED]").field("expires_at", &self.expires_at).finish()
    }
}

/// Issues credentials for a scope, for one run, for at most `ttl_ms` (SBX-2).
/// `Forbidden` when the scope is unknown or not issuable to this run.
pub trait Credentials: Send + Sync {
    fn lease(&self, scope: &str, run_id: &str, ttl_ms: u64) -> Result<Lease>;
}

/// Knowledge retrieval (RET-1 to RET-4).
pub trait Retriever {
    fn search(&self, task_class: &str, query: &str, top_k: usize) -> Result<Vec<(String, f32)>>;
}

/// A command for the sandbox to run. Never contains credentials in plain text from model context (SBX-2).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub program: String,
    pub args: Vec<String>,
    /// Working directory, relative to the workspace.
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub stdin: Option<String>,
    /// Isolated filesystem the call sees (SBX-1): a folder name under the sandbox root, one per run.
    /// Set by the engine to the run id; a step cannot choose another run's workspace. Empty = root.
    #[serde(default)]
    pub workspace: String,
    /// Hosts the call may reach (SBX-1). Set by the engine from the workflow policy; empty = no network.
    #[serde(default)]
    pub egress: Vec<HostRule>,
    /// Credentials to inject as env vars at the tool layer (SBX-2). The engine resolves them;
    /// only the reference is ever logged.
    #[serde(default)]
    pub secrets: Vec<SecretRef>,
}

/// One egress allowlist entry: `host` is an exact name or IP, or `*.suffix` for subdomains (not the
/// bare suffix). `port` `None` = 443 and 80 only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostRule {
    pub host: String,
    pub port: Option<u16>,
}

impl HostRule {
    /// Parses `host`, `host:port`, `*.suffix` or `*.suffix:port`.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim().to_ascii_lowercase();
        let (host, port) = match s.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => {
                (h.to_string(), Some(p.parse::<u16>().map_err(|_| Error::Invalid(format!("bad port in {s}")))?))
            }
            _ => (s.clone(), None),
        };
        let bare = host.strip_prefix("*.").unwrap_or(&host);
        if bare.is_empty() || bare.contains('*') || !bare.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
            return Err(Error::Invalid(format!("bad host rule {s}")));
        }
        Ok(HostRule { host, port })
    }

    /// Case-insensitive match; a trailing dot on `host` is ignored.
    pub fn allows(&self, host: &str, port: u16) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let host_ok = match self.host.strip_prefix("*.") {
            Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(suffix) && host[..host.len() - suffix.len()].ends_with('.'),
            None => host == self.host,
        };
        host_ok && self.port.map_or(port == 443 || port == 80, |p| p == port)
    }
}

/// A credential the step asks for by scope; the engine injects the leased value as env var `env`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRef {
    pub env: String,
    pub scope: String,
}

/// One connection attempt seen by the egress proxy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressAttempt {
    pub host: String,
    pub port: u16,
    pub allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    /// Output was cut at `Limits::max_output_bytes`.
    pub truncated: bool,
    /// Every connection the call tried through the egress proxy, allowed or not (SBX-1, audit).
    #[serde(default)]
    pub egress: Vec<EgressAttempt>,
}

/// Per-call resource limits (SBX-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub wall_clock: Duration,
    pub max_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { wall_clock: Duration::from_secs(300), max_output_bytes: 1 << 20 }
    }
}

/// Shared kill switch (SBX-4). Tripping it stops in-flight tool calls and the run.
#[derive(Debug, Clone, Default)]
pub struct KillSwitch(Arc<AtomicBool>);

impl KillSwitch {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn trip(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_tripped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Isolated execution for tool calls (SBX-1 to SBX-4).
/// Returns `Timeout` past `limits.wall_clock` and `Killed` once `kill` trips; the child is
/// killed in both cases. A non-zero exit is an `Ok` with `exit_code` set.
pub trait Sandbox: Send + Sync {
    fn run(&self, call: &ToolCall, limits: &Limits, kill: &KillSwitch) -> Result<ToolOutput>;
}

/// Time source, injectable so timers and backoff are testable.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_key_is_stable_and_unambiguous() {
        assert_eq!(idempotency_key("r1", "s1"), idempotency_key("r1", "s1"));
        assert_ne!(idempotency_key("r1", "s1"), idempotency_key("r1", "s2"));
        // The separator keeps ("ab","c") and ("a","bc") apart.
        assert_ne!(idempotency_key("ab", "c"), idempotency_key("a", "bc"));
        assert_eq!(idempotency_key("r", "s").len(), 64);
    }
}

#[cfg(test)]
mod safety_tests {
    use super::*;

    #[test]
    fn host_rules() {
        let r = HostRule::parse("api.github.com").unwrap();
        assert!(r.allows("API.GitHub.com.", 443) && r.allows("api.github.com", 80));
        assert!(!r.allows("api.github.com", 22) && !r.allows("evil-api.github.com", 443));
        let w = HostRule::parse("*.example.com:8443").unwrap();
        assert!(w.allows("a.b.example.com", 8443) && !w.allows("example.com", 8443) && !w.allows("aexample.com", 8443));
        assert!(!w.allows("a.example.com", 443));
        assert!(HostRule::parse("127.0.0.1:9").unwrap().allows("127.0.0.1", 9));
        for bad in ["", "*", "a*.com", "host:port", "ex ample.com", "*."] {
            assert!(HostRule::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn action_digest_is_stable_and_binds_detail() {
        let a = Action { kind: ActionKind::Deploy, target: "prod".into(), detail: serde_json::json!({"b": 1, "a": 2}) };
        let b = Action { detail: serde_json::json!({"a": 2, "b": 1}), ..a.clone() };
        assert_eq!(a.digest(), b.digest());
        assert_ne!(a.digest(), Action { target: "staging".into(), ..a.clone() }.digest());
        assert_eq!(ActionKind::MergeProtected.name(), "merge_protected");
        assert_eq!(ActionKind::Other("x".into()).name(), "other:x");
    }

    #[test]
    fn audit_chain_detects_tampering() {
        let mut log = Vec::new();
        let mut prev = String::new();
        for i in 0..3u64 {
            let mut r = AuditRecord {
                seq: i, ts: i, run_id: "r".into(), step_id: "s".into(), agent_id: None, key: format!("k{i}"),
                action: "tool".into(), target: "sh".into(), digest: String::new(), decision: Decision::Auto,
                approver: None, detail: serde_json::json!({}), prev_hash: prev.clone(), hash: String::new(),
            };
            r.hash = r.compute_hash();
            prev = r.hash.clone();
            log.push(r);
        }
        assert_eq!(verify_chain(&log), Ok(()));
        log[1].approver = Some("mallory".into());
        assert_eq!(verify_chain(&log), Err(1));
    }

    #[test]
    fn lease_debug_hides_value() {
        let l = Lease { scope: "gh".into(), value: "s3cr3t".into(), expires_at: 1 };
        assert!(!format!("{l:?}").contains("s3cr3t"));
    }
}

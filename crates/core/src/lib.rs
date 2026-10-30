//! Shared types and plugin traits. Plugins depend on this crate, never on each other.

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
    /// Working directory, relative to the sandbox root.
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub stdin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    /// Output was cut at `Limits::max_output_bytes`.
    pub truncated: bool,
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

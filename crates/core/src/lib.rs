//! Shared types and plugin traits. Plugins depend on this crate, never on each other.

use serde::{Deserialize, Serialize};

pub type RunId = String;
pub type StepId = String;

/// Run lifecycle states (see docs/spec.md, "Run lifecycle").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Append-only event log entry (DUR-2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub run_id: RunId,
    pub step_id: StepId,
    pub seq: u64,
    pub kind: EventKind,
    pub payload: serde_json::Value,
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
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("storage: {0}")]
    Storage(String),
    #[error("conflict: {0}")]
    Conflict(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Durable event store (DUR-1, DUR-2).
pub trait EventStore {
    fn append(&self, event: &Event) -> Result<()>;
    fn read_run(&self, run_id: &str) -> Result<Vec<Event>>;
}

/// Knowledge retrieval (RET-1 to RET-4).
pub trait Retriever {
    fn search(&self, task_class: &str, query: &str, top_k: usize) -> Result<Vec<(String, f32)>>;
}

/// Isolated execution for tool calls (SBX-1 to SBX-4).
pub trait Sandbox {
    fn run(&self, command: &str, idempotency_key: &str) -> Result<String>;
}

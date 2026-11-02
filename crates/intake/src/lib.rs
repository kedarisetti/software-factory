//! Intake (TRG-1 to TRG-4): every source, webhook or poll, runs its configured `Pipeline` to turn a
//! JSON document into events; `Intake::accept` runs permission, dedupe, rate limit and priority, then
//! writes the work queue. Pure logic: no network, no vendor built in.

pub mod bottleneck;
pub mod connector;
pub mod event;
pub mod intake;
pub mod permissions;
pub mod pipeline;
pub mod priority;
pub mod sig;
pub mod webhook;

pub use connector::{ConnectorError, Headers};
pub use event::IntakeEvent;
pub use intake::{Intake, IntakeConfig, Outcome, RateLimits};
pub use permissions::{ConfigError, Permission, PermissionTable};
pub use pipeline::{Mapped, Pipeline};
pub use priority::{PriorityConfig, PriorityRule, Scored};
pub use webhook::{Challenge, Verify, Webhook, WebhookOutcome};

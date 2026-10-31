//! Intake (TRG-1 to TRG-4): connectors normalise webhooks into events, `Intake::accept` runs
//! permission, dedupe, rate limit and priority, then writes the work queue. Pure logic: no network.

pub mod bottleneck;
pub mod connector;
pub mod event;
pub mod github;
pub mod intake;
pub mod mcp;
pub mod permissions;
pub mod priority;
pub mod sig;
pub mod slack;

pub use connector::{ConnectorError, Headers};
pub use event::IntakeEvent;
pub use intake::{Intake, IntakeConfig, Outcome, RateLimits};
pub use permissions::{ConfigError, Permission, PermissionTable};
pub use priority::{PriorityConfig, PriorityRule, Scored};

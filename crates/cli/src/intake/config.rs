//! `intake.json` (TRG-2, TRG-3): connector secrets by env var name, permission table, limits, budgets.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use sf_core::Queue;
use sf_intake::github::GithubConfig;
use sf_intake::slack::SlackConfig;
use sf_intake::{Intake, IntakeConfig, PermissionTable, PriorityConfig, RateLimits};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    slack: Option<SlackSection>,
    github: Option<GithubSection>,
    permissions: Value,
    #[serde(default)]
    rate: RateSection,
    near_dup_window_ms: Option<u64>,
    priority: Option<PriorityConfig>,
    #[serde(default)]
    budget: Budget,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SlackSection {
    /// Name of the env var holding the signing secret; the secret itself never lives in the file.
    signing_secret_env: String,
    bot_user_id: String,
    #[serde(default)]
    channel_defaults: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubSection {
    webhook_secret_env: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RateSection {
    window_ms: Option<u64>,
    #[serde(default)]
    per_source: BTreeMap<String, u64>,
    per_actor: Option<u64>,
}

/// Budgets: per run, and per UTC day across runs (crossing the daily one pauses intake only).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Budget {
    pub run_tokens: u64,
    pub run_usd_micros: u64,
    pub daily_tokens: Option<u64>,
    pub daily_usd_micros: Option<u64>,
}

impl Default for Budget {
    fn default() -> Self {
        Budget { run_tokens: 50_000, run_usd_micros: 5_000_000, daily_tokens: None, daily_usd_micros: None }
    }
}

pub struct Loaded {
    pub intake: Intake,
    pub slack: Option<SlackConfig>,
    pub github: Option<GithubConfig>,
    pub budget: Budget,
}

fn secret(var: &str) -> Result<String, String> {
    std::env::var(var).ok().filter(|s| !s.is_empty()).ok_or_else(|| format!("env var {var} is not set"))
}

/// Loads and validates the config. `with_secrets` reads the connector secrets (serve only).
pub fn load(path: &str, queue: Arc<dyn Queue>, with_secrets: bool) -> Result<Loaded, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("config {path}: {e}"))?;
    let raw: Raw = serde_json::from_str(&text).map_err(|e| format!("config {path}: {e}"))?;
    let perms = PermissionTable::from_json(&raw.permissions.to_string()).map_err(|e| e.to_string())?;
    let mut cfg = IntakeConfig::default();
    if let Some(w) = raw.near_dup_window_ms {
        cfg.near_dup_window_ms = w;
    }
    let d = RateLimits::default();
    cfg.rate = RateLimits {
        window_ms: raw.rate.window_ms.unwrap_or(d.window_ms),
        per_source: raw.rate.per_source,
        per_actor: raw.rate.per_actor.or(d.per_actor),
    };
    if let Some(p) = raw.priority {
        cfg.priority = p;
    }
    let slack = match (raw.slack, with_secrets) {
        (Some(s), true) => Some(SlackConfig {
            signing_secret: secret(&s.signing_secret_env)?,
            bot_user_id: s.bot_user_id,
            channel_defaults: s.channel_defaults,
        }),
        _ => None,
    };
    let github = match (raw.github, with_secrets) {
        (Some(g), true) => Some(GithubConfig { webhook_secret: secret(&g.webhook_secret_env)? }),
        _ => None,
    };
    Ok(Loaded { intake: Intake::new(queue, perms, cfg), slack, github, budget: raw.budget })
}

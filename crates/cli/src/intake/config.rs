//! `intake.json` (TRG-2, TRG-3): configured sources, permission table, limits, budgets.
//!
//! Version 2 lists `sources` (each with a unique name, a kind and kind-specific settings). Version 1
//! (no `version`, top-level `slack` / `github` sections) still loads and becomes sources named
//! `slack` and `github`. Secrets are only ever env var names; the file holds no secret values.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use sf_core::Queue;
use sf_intake::github::GithubConfig;
use sf_intake::mcp::{looks_like_secret, McpMap};
use sf_intake::slack::SlackConfig;
use sf_intake::{Intake, IntakeConfig, PermissionTable, PriorityConfig, RateLimits};
use sha2::{Digest, Sha256};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    version: Option<u32>,
    sources: Option<Vec<SourceRaw>>,
    // v1 only
    slack: Option<SlackRaw>,
    github: Option<GithubRaw>,
    permissions: Value,
    #[serde(default)]
    rate: RateSection,
    near_dup_window_ms: Option<u64>,
    priority: Option<PriorityConfig>,
    #[serde(default)]
    budget: Budget,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum SourceRaw {
    Slack { name: String, #[serde(flatten)] cfg: SlackRaw },
    Github { name: String, #[serde(flatten)] cfg: GithubRaw },
    Mcp { name: String, #[serde(flatten)] cfg: Box<McpRaw> },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SlackRaw {
    /// Name of the env var holding the signing secret; the secret itself never lives in the file.
    signing_secret_env: String,
    bot_user_id: String,
    #[serde(default)]
    channel_defaults: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubRaw {
    webhook_secret_env: Option<String>,
    poll: Option<GithubPollRaw>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubPollRaw {
    repos: Vec<String>,
    label: Option<String>,
    interval_s: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpRaw {
    command: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    tool: String,
    #[serde(default)]
    arguments: Value,
    poll_interval_s: Option<u64>,
    timeout_ms: Option<u64>,
    #[serde(default)]
    chat: bool,
    map: McpMap,
    cursor: Option<String>,
    default_workflow: Option<String>,
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

pub const DEFAULT_POLL_S: u64 = 300;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone)]
pub struct SlackSrc {
    pub signing_secret_env: String,
    pub bot_user_id: String,
    pub channel_defaults: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct GithubPoll {
    pub repos: Vec<String>,
    pub label: String,
    pub interval_s: u64,
}

#[derive(Debug, Clone)]
pub struct GithubSrc {
    pub webhook_secret_env: Option<String>,
    pub poll: Option<GithubPoll>,
}

#[derive(Debug, Clone)]
pub struct McpSrc {
    pub command: Vec<String>,
    /// NAME -> `${HOST_VAR}`; validated to be references only.
    pub env: BTreeMap<String, String>,
    pub tool: String,
    pub arguments: Value,
    pub poll_interval_s: u64,
    pub timeout_ms: u64,
    pub chat: bool,
    pub map: McpMap,
    pub cursor: Option<String>,
    pub default_workflow: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Kind {
    Slack(SlackSrc),
    Github(GithubSrc),
    Mcp(Box<McpSrc>),
}

#[derive(Debug, Clone)]
pub struct Source {
    pub name: String,
    pub kind: Kind,
}

impl Source {
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            Kind::Slack(_) => "slack",
            Kind::Github(_) => "github",
            Kind::Mcp(_) => "mcp",
        }
    }

    pub fn is_chat(&self) -> bool {
        match &self.kind {
            Kind::Slack(_) => true,
            Kind::Github(_) => false,
            Kind::Mcp(m) => m.chat,
        }
    }
}

impl McpSrc {
    /// The host env var an env entry refers to (`${VAR}` -> `VAR`).
    pub fn env_refs(&self) -> BTreeMap<String, String> {
        self.env.iter().map(|(k, v)| (k.clone(), env_ref(v).unwrap_or_default().to_string())).collect()
    }

    /// Binds an approval: command, env var names (never values), tool, arguments, map, and the
    /// settings that change what the source may do (cursor, chat, default workflow).
    pub fn digest(&self) -> String {
        let canon = json!({
            "command": self.command, "env": self.env_refs(), "tool": self.tool, "arguments": self.arguments,
            "map": self.map, "cursor": self.cursor, "chat": self.chat, "default_workflow": self.default_workflow,
        });
        let h = Sha256::digest(canon.to_string().as_bytes());
        h.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Reads the referenced host env vars. Names the missing variable, never prints a value.
    pub fn resolve_env(&self) -> Result<Vec<(String, String)>, String> {
        self.env_refs()
            .into_iter()
            .map(|(k, var)| match std::env::var(&var) {
                Ok(v) if !v.is_empty() => Ok((k, v)),
                _ => Err(format!("env var {var} (for {k}) is not set")),
            })
            .collect()
    }
}

pub struct Loaded {
    pub intake: Intake,
    pub sources: Vec<Source>,
    /// Webhook configs by source name; filled only when secrets were requested.
    pub slack: BTreeMap<String, SlackConfig>,
    pub github: BTreeMap<String, GithubConfig>,
    pub budget: Budget,
}

impl Loaded {
    pub fn source(&self, name: &str) -> Option<&Source> {
        self.sources.iter().find(|s| s.name == name)
    }

    pub fn is_chat(&self, name: &str) -> bool {
        self.sources.iter().any(|s| s.name.eq_ignore_ascii_case(name) && s.is_chat())
    }
}

fn secret(var: &str) -> Result<String, String> {
    std::env::var(var).ok().filter(|s| !s.is_empty()).ok_or_else(|| format!("env var {var} is not set"))
}

fn is_ident(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_') && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `${VAR}` -> `VAR`.
pub fn env_ref(v: &str) -> Option<&str> {
    v.strip_prefix("${")?.strip_suffix('}').filter(|n| is_ident(n))
}

fn env_name(what: &str, v: &str) -> Result<String, String> {
    if is_ident(v) {
        Ok(v.to_string())
    } else {
        Err(format!("{what} must be the NAME of an env var, not a secret value"))
    }
}

fn valid_source_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn scan_secrets(v: &Value, what: &str) -> Result<(), String> {
    match v {
        Value::String(s) if looks_like_secret(s) => Err(format!("{what} contains a literal that looks like a token; secrets belong in env refs only")),
        Value::Array(a) => a.iter().try_for_each(|x| scan_secrets(x, what)),
        Value::Object(m) => m.values().try_for_each(|x| scan_secrets(x, what)),
        _ => Ok(()),
    }
}

fn mcp_source(name: &str, r: McpRaw) -> Result<McpSrc, String> {
    let at = |m: &str| format!("source {name}: {m}");
    if r.command.is_empty() || r.command[0].is_empty() {
        return Err(at("command must be a non-empty argv list"));
    }
    if r.tool.is_empty() {
        return Err(at("tool is required"));
    }
    for (k, v) in &r.env {
        if !is_ident(k) {
            return Err(at(&format!("env name {k:?} is not a valid variable name")));
        }
        if env_ref(v).is_none() {
            let why = if looks_like_secret(v) { "looks like a token" } else { "is a literal" };
            return Err(at(&format!("env.{k} {why}; values must reference the host env as \"${{VAR}}\", never hold a secret")));
        }
    }
    scan_secrets(&json!(r.command), &at("command")).map_err(|e| e.to_string())?;
    scan_secrets(&r.arguments, &at("arguments"))?;
    let arguments = if r.arguments.is_null() { json!({}) } else { r.arguments };
    if !arguments.is_object() {
        return Err(at("arguments must be a JSON object"));
    }
    r.map.validate().map_err(|e| at(&e))?;
    if let Some(c) = &r.cursor {
        if c == "max:updated_at" {
            if r.map.updated_at.is_none() {
                return Err(at("cursor \"max:updated_at\" needs map.updated_at"));
            }
        } else if !(c.is_empty() || c.starts_with('/')) {
            return Err(at("cursor must be a JSON pointer or \"max:updated_at\""));
        }
    }
    let timeout_ms = r.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    if !(50..=600_000).contains(&timeout_ms) {
        return Err(at("timeout_ms must be between 50 and 600000"));
    }
    Ok(McpSrc {
        command: r.command,
        env: r.env,
        tool: r.tool,
        arguments,
        poll_interval_s: r.poll_interval_s.unwrap_or(DEFAULT_POLL_S).max(1),
        timeout_ms,
        chat: r.chat,
        map: r.map,
        cursor: r.cursor.filter(|c| !c.is_empty()),
        default_workflow: r.default_workflow,
    })
}

fn slack_source(r: SlackRaw) -> Result<SlackSrc, String> {
    Ok(SlackSrc {
        signing_secret_env: env_name("signing_secret_env", &r.signing_secret_env)?,
        bot_user_id: r.bot_user_id,
        channel_defaults: r.channel_defaults,
    })
}

fn github_source(name: &str, r: GithubRaw) -> Result<GithubSrc, String> {
    if r.webhook_secret_env.is_none() && r.poll.is_none() {
        return Err(format!("source {name}: github needs webhook_secret_env and/or poll"));
    }
    let poll = r
        .poll
        .map(|p| {
            if p.repos.is_empty() {
                return Err(format!("source {name}: poll.repos is empty"));
            }
            Ok(GithubPoll {
                repos: p.repos,
                label: p.label.unwrap_or_else(|| "factory".into()),
                interval_s: p.interval_s.unwrap_or(DEFAULT_POLL_S).max(1),
            })
        })
        .transpose()?;
    Ok(GithubSrc { webhook_secret_env: r.webhook_secret_env.map(|v| env_name("webhook_secret_env", &v)).transpose()?, poll })
}

fn build_sources(raw: &mut Raw) -> Result<(u32, Vec<Source>), String> {
    let mut out = Vec::new();
    match raw.version.unwrap_or(1) {
        1 => {
            if raw.sources.is_some() {
                return Err("\"sources\" needs \"version\": 2".into());
            }
            if let Some(s) = raw.slack.take() {
                out.push(Source { name: "slack".into(), kind: Kind::Slack(slack_source(s)?) });
            }
            if let Some(g) = raw.github.take() {
                out.push(Source { name: "github".into(), kind: Kind::Github(github_source("github", g)?) });
            }
            Ok((1, out))
        }
        2 => {
            if raw.slack.is_some() || raw.github.is_some() {
                return Err("version 2 has no top-level slack/github sections; list them under \"sources\"".into());
            }
            for s in raw.sources.take().unwrap_or_default() {
                let (name, kind) = match s {
                    SourceRaw::Slack { name, cfg } => (name.clone(), Kind::Slack(slack_source(cfg).map_err(|e| format!("source {name}: {e}"))?)),
                    SourceRaw::Github { name, cfg } => (name.clone(), Kind::Github(github_source(&name, cfg)?)),
                    SourceRaw::Mcp { name, cfg } => (name.clone(), Kind::Mcp(Box::new(mcp_source(&name, *cfg)?))),
                };
                if !valid_source_name(&name) {
                    return Err(format!("source name {name:?}: use 1-64 characters from A-Z a-z 0-9 - _ ."));
                }
                if out.iter().any(|o: &Source| o.name.eq_ignore_ascii_case(&name)) {
                    return Err(format!("duplicate source name {name:?}"));
                }
                out.push(Source { name, kind });
            }
            Ok((2, out))
        }
        v => Err(format!("unsupported config version {v} (this build reads 1 and 2)")),
    }
}

/// Loads and validates the config. `with_secrets` reads the webhook secrets (serve only).
pub fn load(path: &str, queue: Arc<dyn Queue>, with_secrets: bool) -> Result<Loaded, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("config {path}: {e}"))?;
    let mut raw: Raw = serde_json::from_str(&text).map_err(|e| format!("config {path}: {e}"))?;
    let (version, sources) = build_sources(&mut raw).map_err(|e| format!("config {path}: {e}"))?;
    let chat: Vec<String> = sources.iter().filter(|s| s.is_chat()).map(|s| s.name.clone()).collect();
    let perms = PermissionTable::from_json_with_chat(&raw.permissions.to_string(), &chat).map_err(|e| e.to_string())?;
    if version >= 2 {
        let known: BTreeSet<String> = sources.iter().map(|s| s.name.to_ascii_lowercase()).collect();
        let unknown = perms.rules.iter().map(|r| &r.source).chain(raw.rate.per_source.keys()).find(|n| !known.contains(&n.to_ascii_lowercase()));
        if let Some(n) = unknown {
            return Err(format!("config {path}: permissions/rate name unknown source {n:?}"));
        }
    }
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
    for s in &sources {
        cfg.priority.base.entry(s.name.clone()).or_insert(if s.kind_name() == "github" { 10 } else { 5 });
    }
    let (mut slack, mut github) = (BTreeMap::new(), BTreeMap::new());
    if with_secrets {
        for s in &sources {
            match &s.kind {
                Kind::Slack(c) => {
                    let sc = SlackConfig {
                        source: s.name.clone(),
                        signing_secret: secret(&c.signing_secret_env)?,
                        bot_user_id: c.bot_user_id.clone(),
                        channel_defaults: c.channel_defaults.clone(),
                    };
                    slack.insert(s.name.clone(), sc);
                }
                Kind::Github(GithubSrc { webhook_secret_env: Some(v), .. }) => {
                    github.insert(s.name.clone(), GithubConfig { source: s.name.clone(), webhook_secret: secret(v)? });
                }
                _ => {}
            }
        }
    }
    Ok(Loaded { intake: Intake::new(queue, perms, cfg), sources, slack, github, budget: raw.budget })
}

//! `intake.json` (TRG-2, TRG-3): configured sources, permission table, limits, budgets.
//!
//! Version 2 lists `sources`, each with a unique name and a `kind`: `webhook` (signed POST), `command`
//! (an argv that prints JSON) or `mcp` (an MCP server tool). Each carries an `events` pipeline that turns
//! its JSON into events. No vendor is built in; vendors are presets (docs/presets/).
//! Secrets are only ever env var names; the file holds no secret values.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use sf_core::Queue;
use sf_intake::pipeline::looks_like_secret;
use sf_intake::{
    Challenge, Intake, IntakeConfig, PermissionTable, Pipeline, PriorityConfig, RateLimits, Verify,
    Webhook,
};
use sha2::{Digest, Sha256};

const PRESETS: &str = "see the presets in docs/intake.md (docs/presets/*.json)";

#[derive(Debug, Deserialize)]
struct Raw {
    version: Option<u32>,
    #[serde(default)]
    sources: Vec<Value>,
    /// Any other top-level key (e.g. a vendor section from the old format) is refused with a pointer to the presets.
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
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
struct WebhookRaw {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    kind: String,
    #[serde(default)]
    chat: bool,
    verify: Verify,
    challenge: Option<Challenge>,
    events: Pipeline,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandRaw {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    kind: String,
    command: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    poll_interval_s: Option<u64>,
    timeout_ms: Option<u64>,
    #[serde(default)]
    chat: bool,
    cursor: Option<String>,
    cursor_start: Option<String>,
    events: Pipeline,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpRaw {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    kind: String,
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
    cursor: Option<String>,
    cursor_start: Option<String>,
    events: Pipeline,
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
        Budget {
            run_tokens: 50_000,
            run_usd_micros: 5_000_000,
            daily_tokens: None,
            daily_usd_micros: None,
        }
    }
}

pub const DEFAULT_POLL_S: u64 = 300;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone)]
pub struct WebhookSrc {
    pub verify: Verify,
    pub challenge: Option<Challenge>,
    pub events: Pipeline,
    pub chat: bool,
}

/// A source that runs a process each poll: `command` (stdout is the JSON) or `mcp` (a tool call).
#[derive(Debug, Clone)]
pub struct ProcSrc {
    /// `command` or `mcp`.
    pub kind: &'static str,
    pub command: Vec<String>,
    /// NAME -> `${HOST_VAR}`; validated to be references only.
    pub env: BTreeMap<String, String>,
    pub tool: Option<String>,
    pub arguments: Value,
    pub poll_interval_s: u64,
    pub timeout_ms: u64,
    pub chat: bool,
    pub events: Pipeline,
    pub cursor: Option<String>,
    pub cursor_start: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Kind {
    Webhook(Box<WebhookSrc>),
    Proc(Box<ProcSrc>),
}

#[derive(Debug, Clone)]
pub struct Source {
    pub name: String,
    pub kind: Kind,
}

impl Source {
    pub fn kind_name(&self) -> &'static str {
        match &self.kind {
            Kind::Webhook(_) => "webhook",
            Kind::Proc(p) => p.kind,
        }
    }

    pub fn is_chat(&self) -> bool {
        match &self.kind {
            Kind::Webhook(w) => w.chat,
            Kind::Proc(p) => p.chat,
        }
    }

    pub fn proc(&self) -> Option<&ProcSrc> {
        match &self.kind {
            Kind::Proc(p) => Some(p),
            Kind::Webhook(_) => None,
        }
    }
}

impl ProcSrc {
    /// The host env var an env entry refers to (`${VAR}` -> `VAR`).
    pub fn env_refs(&self) -> BTreeMap<String, String> {
        self.env
            .iter()
            .map(|(k, v)| (k.clone(), env_ref(v).unwrap_or_default().to_string()))
            .collect()
    }

    /// Binds an approval: kind, command, env var names (never values), tool, arguments, the events
    /// pipeline, and the settings that change what the source may do (cursor, cursor_start, chat).
    pub fn digest(&self) -> String {
        let canon = json!({
            "kind": self.kind, "command": self.command, "env": self.env_refs(), "tool": self.tool, "arguments": self.arguments,
            "events": self.events, "cursor": self.cursor, "cursor_start": self.cursor_start, "chat": self.chat,
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
    /// Webhook handlers by source name; filled only when secrets were requested.
    pub webhooks: BTreeMap<String, Webhook>,
    pub budget: Budget,
}

impl Loaded {
    pub fn source(&self, name: &str) -> Option<&Source> {
        self.sources.iter().find(|s| s.name == name)
    }

    pub fn is_chat(&self, name: &str) -> bool {
        self.sources
            .iter()
            .any(|s| s.name.eq_ignore_ascii_case(name) && s.is_chat())
    }
}

fn secret(var: &str) -> Result<String, String> {
    std::env::var(var)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("env var {var} is not set"))
}

fn is_ident(s: &str) -> bool {
    let mut c = s.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `${VAR}` -> `VAR`.
pub fn env_ref(v: &str) -> Option<&str> {
    v.strip_prefix("${")?
        .strip_suffix('}')
        .filter(|n| is_ident(n))
}

fn valid_source_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn scan_secrets(v: &Value, what: &str) -> Result<(), String> {
    match v {
        Value::String(s) if looks_like_secret(s) => Err(format!(
            "{what} contains a literal that looks like a token; secrets belong in env refs only"
        )),
        Value::Array(a) => a.iter().try_for_each(|x| scan_secrets(x, what)),
        Value::Object(m) => m.values().try_for_each(|x| scan_secrets(x, what)),
        _ => Ok(()),
    }
}

fn check_pipeline(name: &str, p: &Pipeline) -> Result<(), String> {
    p.validate()
        .map_err(|e| format!("source {name}: events: {e}"))?;
    scan_secrets(&json!(p), &format!("source {name}: events"))
}

#[allow(clippy::too_many_arguments)]
fn proc_source(
    name: &str,
    kind: &'static str,
    command: Vec<String>,
    env: BTreeMap<String, String>,
    tool: Option<String>,
    arguments: Value,
    (poll_interval_s, timeout_ms, chat): (Option<u64>, Option<u64>, bool),
    cursor: Option<String>,
    cursor_start: Option<String>,
    events: Pipeline,
) -> Result<ProcSrc, String> {
    let at = |m: &str| format!("source {name}: {m}");
    if command.is_empty() || command[0].is_empty() {
        return Err(at("command must be a non-empty argv list"));
    }
    if tool.as_deref().is_some_and(str::is_empty) {
        return Err(at("tool is required"));
    }
    for (k, v) in &env {
        if !is_ident(k) {
            return Err(at(&format!("env name {k:?} is not a valid variable name")));
        }
        if env_ref(v).is_none() {
            let why = if looks_like_secret(v) {
                "looks like a token"
            } else {
                "is a literal"
            };
            return Err(at(&format!("env.{k} {why}; values must reference the host env as \"${{VAR}}\", never hold a secret")));
        }
    }
    scan_secrets(&json!(command), &at("command"))?;
    scan_secrets(&arguments, &at("arguments"))?;
    let arguments = if arguments.is_null() {
        json!({})
    } else {
        arguments
    };
    if !arguments.is_object() {
        return Err(at("arguments must be a JSON object"));
    }
    check_pipeline(name, &events)?;
    if let Some(c) = &cursor {
        if c == "max:updated_at" {
            if events.map.updated_at.is_none() {
                return Err(at("cursor \"max:updated_at\" needs events.map.updated_at"));
            }
        } else if !(c.is_empty() || c.starts_with('/')) {
            return Err(at("cursor must be a JSON pointer or \"max:updated_at\""));
        }
    }
    if let Some(s) = &cursor_start {
        scan_secrets(&json!(s), &at("cursor_start"))?;
    }
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    if !(50..=600_000).contains(&timeout_ms) {
        return Err(at("timeout_ms must be between 50 and 600000"));
    }
    Ok(ProcSrc {
        kind,
        command,
        env,
        tool,
        arguments,
        poll_interval_s: poll_interval_s.unwrap_or(DEFAULT_POLL_S).max(1),
        timeout_ms,
        chat,
        events,
        cursor: cursor.filter(|c| !c.is_empty()),
        cursor_start,
    })
}

fn webhook_source(name: &str, r: WebhookRaw) -> Result<WebhookSrc, String> {
    let at = |m: &str| format!("source {name}: {m}");
    if !is_ident(&r.verify.secret_env) {
        return Err(at(
            "verify.secret_env must be the NAME of an env var, not a secret value",
        ));
    }
    r.verify.validate().map_err(|e| at(&e))?;
    if let Some(c) = &r.challenge {
        c.validate().map_err(|e| at(&e))?;
        scan_secrets(&json!(c), &at("challenge"))?;
    }
    check_pipeline(name, &r.events)?;
    Ok(WebhookSrc {
        verify: r.verify,
        challenge: r.challenge,
        events: r.events,
        chat: r.chat,
    })
}

fn parse<T: for<'de> Deserialize<'de>>(name: &str, v: Value) -> Result<T, String> {
    serde_json::from_value(v).map_err(|e| format!("source {name}: {e}"))
}

fn build_sources(raw: &mut Raw) -> Result<Vec<Source>, String> {
    if let Some(k) = raw.extra.keys().next() {
        return Err(format!("unknown top-level key {k:?}; per-vendor sections are gone, list a webhook, command or mcp source under \"sources\" ({PRESETS})"));
    }
    match raw.version {
        Some(2) => {}
        Some(v) => {
            return Err(format!(
                "unsupported config version {v} (this build reads 2 only; {PRESETS})"
            ))
        }
        None => {
            return Err(format!(
                "\"version\": 2 is required (version 1 is no longer read; {PRESETS})"
            ))
        }
    }
    let mut out: Vec<Source> = Vec::new();
    for s in std::mem::take(&mut raw.sources) {
        let name = s
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !valid_source_name(&name) {
            return Err(format!(
                "source name {name:?}: use 1-64 characters from A-Z a-z 0-9 - _ ."
            ));
        }
        let kind = s
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let kind = match kind.as_str() {
            "webhook" => Kind::Webhook(Box::new(webhook_source(&name, parse(&name, s)?)?)),
            "command" => {
                let r: CommandRaw = parse(&name, s)?;
                Kind::Proc(Box::new(proc_source(&name, "command", r.command, r.env, None, Value::Null, (r.poll_interval_s, r.timeout_ms, r.chat), r.cursor, r.cursor_start, r.events)?))
            }
            "mcp" => {
                let r: McpRaw = parse(&name, s)?;
                Kind::Proc(Box::new(proc_source(&name, "mcp", r.command, r.env, Some(r.tool), r.arguments, (r.poll_interval_s, r.timeout_ms, r.chat), r.cursor, r.cursor_start, r.events)?))
            }
            other => return Err(format!("source {name}: unknown kind {other:?} (webhook | command | mcp); vendors are not kinds any more, {PRESETS}")),
        };
        if out.iter().any(|o| o.name.eq_ignore_ascii_case(&name)) {
            return Err(format!("duplicate source name {name:?}"));
        }
        out.push(Source { name, kind });
    }
    Ok(out)
}

/// Loads and validates the config. `with_secrets` reads the webhook secrets (serving only).
pub fn load(path: &str, queue: Arc<dyn Queue>, with_secrets: bool) -> Result<Loaded, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("config {path}: {e}"))?;
    let mut raw: Raw = serde_json::from_str(&text).map_err(|e| format!("config {path}: {e}"))?;
    let sources = build_sources(&mut raw).map_err(|e| format!("config {path}: {e}"))?;
    let chat: Vec<String> = sources
        .iter()
        .filter(|s| s.is_chat())
        .map(|s| s.name.clone())
        .collect();
    let perms = PermissionTable::from_json_with_chat(&raw.permissions.to_string(), &chat)
        .map_err(|e| e.to_string())?;
    let known: BTreeSet<String> = sources
        .iter()
        .map(|s| s.name.to_ascii_lowercase())
        .collect();
    let unknown = perms
        .rules
        .iter()
        .map(|r| &r.source)
        .chain(raw.rate.per_source.keys())
        .find(|n| !known.contains(&n.to_ascii_lowercase()));
    if let Some(n) = unknown {
        return Err(format!(
            "config {path}: permissions/rate name unknown source {n:?}"
        ));
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
    let mut webhooks = BTreeMap::new();
    if with_secrets {
        for s in &sources {
            if let Kind::Webhook(w) = &s.kind {
                let hook = Webhook {
                    source: s.name.clone(),
                    secret: secret(&w.verify.secret_env)?,
                    verify: w.verify.clone(),
                    challenge: w.challenge.clone(),
                    pipeline: w.events.clone(),
                };
                webhooks.insert(s.name.clone(), hook);
            }
        }
    }
    Ok(Loaded {
        intake: Intake::new(queue, perms, cfg),
        sources,
        webhooks,
        budget: raw.budget,
    })
}

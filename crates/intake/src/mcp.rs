//! MCP source mapping (TRG-1): turns a tool result into `IntakeEvent`s through JSON pointers named in
//! the config. Pure logic; the process and protocol live in the CLI. Everything it reads is untrusted.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sf_core::Timestamp;

use crate::event::IntakeEvent;

const MAX_TITLE: usize = 200;
const MAX_TEXT: usize = 20_000;

/// JSON pointers into the tool result (`items`) and into each item (the rest).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpMap {
    pub items: String,
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub labels: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub workflow: Option<String>,
}

impl McpMap {
    /// Every pointer must be `""` or start with `/`.
    pub fn validate(&self) -> Result<(), String> {
        let all = [
            ("items", Some(&self.items)),
            ("id", Some(&self.id)),
            ("title", Some(&self.title)),
            ("text", self.text.as_ref()),
            ("actor", self.actor.as_ref()),
            ("channel", self.channel.as_ref()),
            ("labels", self.labels.as_ref()),
            ("url", self.url.as_ref()),
            ("updated_at", self.updated_at.as_ref()),
            ("workflow", self.workflow.as_ref()),
        ];
        for (name, p) in all {
            if let Some(p) = p {
                if !(p.is_empty() || p.starts_with('/')) {
                    return Err(format!("map.{name}: {p:?} is not a JSON pointer (must start with /)"));
                }
            }
        }
        Ok(())
    }
}

/// A scalar rendered as text; objects, arrays and null are not scalars.
pub fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn at(item: &Value, ptr: &Option<String>) -> Option<String> {
    item.pointer(ptr.as_deref()?).and_then(scalar)
}

fn labels(item: &Value, ptr: &Option<String>) -> Vec<String> {
    let Some(v) = ptr.as_deref().and_then(|p| item.pointer(p)) else { return vec![] };
    match v {
        Value::Array(a) => a.iter().filter_map(|l| scalar(l).or_else(|| l.get("name").and_then(scalar))).collect(),
        other => scalar(other).into_iter().collect(),
    }
}

fn clip(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        s
    } else {
        s.chars().take(max).collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mapped {
    pub events: Vec<IntakeEvent>,
    /// Items without an id or title.
    pub skipped: usize,
    /// Each event's `updated_at`, in the same order (for a `max:updated_at` cursor).
    pub updated: Vec<Option<String>>,
}

/// Maps `result` through `map`. `Err` if `map.items` does not point at an array.
pub fn map_items(source: &str, map: &McpMap, default_workflow: Option<&str>, result: &Value, now: Timestamp) -> Result<Mapped, String> {
    let list = result
        .pointer(&map.items)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("map.items {:?} does not point at an array in the tool result", map.items))?;
    let (mut events, mut updated, mut skipped) = (vec![], vec![], 0);
    for item in list {
        let (Some(id), Some(title)) = (at(item, &Some(map.id.clone())), at(item, &Some(map.title.clone()))) else {
            skipped += 1;
            continue;
        };
        let upd = at(item, &map.updated_at);
        let mut text = at(item, &map.text).unwrap_or_default();
        if let Some(url) = at(item, &map.url) {
            text = format!("{text}\n{url}").trim_start().to_string();
        }
        let requested_workflow = at(item, &map.workflow).filter(|w| !w.is_empty()).or_else(|| default_workflow.map(String::from));
        events.push(IntakeEvent {
            source: source.into(),
            source_ref: id.clone(),
            dedupe_key: format!("{source}:{id}:{}", upd.as_deref().unwrap_or("")),
            actor: at(item, &map.actor).unwrap_or_else(|| "unknown".into()),
            channel: at(item, &map.channel).unwrap_or_default(),
            title: clip(title, MAX_TITLE),
            text: clip(text, MAX_TEXT),
            labels: labels(item, &map.labels),
            requested_workflow,
            received_at: now,
        });
        updated.push(upd);
    }
    Ok(Mapped { events, skipped, updated })
}

/// Greater cursor: numerically when both are integers, else by string (ISO timestamps sort as text).
pub fn cursor_gt(a: &str, b: &str) -> bool {
    match (a.parse::<i128>(), b.parse::<i128>()) {
        (Ok(x), Ok(y)) => x > y,
        _ => a > b,
    }
}

/// Next cursor: `max:updated_at` is the greatest mapped `updated_at`, else a JSON pointer into the result.
pub fn next_cursor(spec: &str, result: &Value, mapped: &Mapped) -> Option<String> {
    if spec == "max:updated_at" {
        return mapped.updated.iter().flatten().fold(None::<&String>, |m, u| match m {
            Some(c) if !cursor_gt(u, c) => Some(c),
            _ => Some(u),
        }).cloned();
    }
    result.pointer(spec).and_then(scalar).filter(|s| !s.is_empty())
}

/// True for a literal that looks like a credential: well-known token prefixes, a bearer header, or a
/// long mixed-case alphanumeric blob. Used to refuse such literals in the config file.
pub fn looks_like_secret(s: &str) -> bool {
    const PREFIXES: [&str; 14] = [
        "xoxb-", "xoxp-", "xoxa-", "xoxs-", "xapp-", "ghp_", "gho_", "ghs_", "ghu_", "github_pat_", "sk-", "lin_api_", "AKIA", "glpat-",
    ];
    let t = s.trim();
    if PREFIXES.iter().any(|p| t.starts_with(p) && t.len() > p.len() + 6) {
        return true;
    }
    if t.to_ascii_lowercase().starts_with("bearer ") || t.starts_with("eyJ") && t.len() > 30 {
        return true;
    }
    t.len() >= 32
        && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        && t.chars().any(|c| c.is_ascii_uppercase())
        && t.chars().any(|c| c.is_ascii_lowercase())
        && t.chars().any(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map() -> McpMap {
        serde_json::from_value(json!({"items": "/tickets", "id": "/key", "title": "/summary", "text": "/body", "actor": "/who/name",
            "channel": "/project", "labels": "/labels", "url": "/url", "updated_at": "/updated", "workflow": "/wf"}))
        .unwrap()
    }

    #[test]
    fn maps_items_and_builds_dedupe_keys() {
        let r = json!({"tickets": [
            {"key": "PRJ-1", "summary": "Fix it", "body": "b", "who": {"name": "ann"}, "project": "PRJ", "labels": ["a", {"name": "b"}], "updated": "2025-01-01", "wf": "triage"},
            {"summary": "no id"},
            {"key": 7, "summary": "bare"}]});
        let m = map_items("jira", &map(), Some("demo"), &r, 5).unwrap();
        assert_eq!((m.events.len(), m.skipped), (2, 1));
        let e = &m.events[0];
        assert_eq!((e.dedupe_key.as_str(), e.actor.as_str(), e.channel.as_str()), ("jira:PRJ-1:2025-01-01", "ann", "PRJ"));
        assert_eq!((e.labels.clone(), e.requested_workflow.as_deref()), (vec!["a".to_string(), "b".into()], Some("triage")));
        let e = &m.events[1];
        assert_eq!((e.dedupe_key.as_str(), e.actor.as_str(), e.requested_workflow.as_deref()), ("jira:7:", "unknown", Some("demo")));
        assert_eq!(next_cursor("max:updated_at", &r, &m).as_deref(), Some("2025-01-01"));
        assert_eq!(next_cursor("/tickets/0/key", &r, &m).as_deref(), Some("PRJ-1"));
    }

    #[test]
    fn bad_items_pointer_is_an_error_and_secrets_are_spotted() {
        assert!(map_items("j", &map(), None, &json!({"x": 1}), 0).is_err());
        assert!(looks_like_secret("xoxb-123456789-abcdefghij"));
        assert!(looks_like_secret("ghp_abcdefghijklmnop"));
        assert!(looks_like_secret("Zk3jQ9x2LmN8vB4tR7wY1pC6sD5fG0hJa"));
        assert!(!looks_like_secret("@linear/mcp-server"));
        assert!(!looks_like_secret("{cursor}"));
        assert!(!looks_like_secret("mcp-server-atlassian"));
    }
}

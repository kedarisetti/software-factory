//! Configurable event pipeline (TRG-1): every source, webhook or poll, turns a JSON document into
//! `IntakeEvent`s through the same steps, all named in the config. Nothing here knows a vendor.
//!
//! 1. `items` picks the list of items in the document (absent: the document is one item).
//! 2. `only` / `ignore` keep or drop items by matching fields and headers.
//! 3. `map` builds each event's fields from templates.
//! 4. `workflow` rules pick the requested workflow; the first that yields one wins.
//!
//! A reference is `/json/pointer` (into the item), `header:Name` (webhooks only) or `source`. A
//! template is a single reference, or text with `{reference}` holes; a missing reference renders "".
//! Everything read here is untrusted content (SBX-3).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sf_core::Timestamp;

use crate::connector::Headers;
use crate::event::IntakeEvent;

const MAX_TITLE: usize = 200;
const MAX_TEXT: usize = 20_000;
const MAX_DEFAULT_TITLE: usize = 80;

/// What a template can read: the item, the request headers (webhooks), the source name.
pub struct Ctx<'a> {
    pub source: &'a str,
    pub item: &'a Value,
    pub headers: Option<&'a Headers>,
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

fn valid_ref(r: &str) -> bool {
    r == "source" || r.starts_with('/') || r.strip_prefix("header:").is_some_and(|h| !h.is_empty())
}

/// The raw value of a reference, if present (pointers only; headers and source are strings).
fn lookup(r: &str, ctx: &Ctx) -> Option<Value> {
    if r == "source" {
        return Some(Value::String(ctx.source.into()));
    }
    if let Some(h) = r.strip_prefix("header:") {
        return ctx.headers?.get(h).map(|v| Value::String(v.into()));
    }
    ctx.item.pointer(r).cloned()
}

fn lookup_str(r: &str, ctx: &Ctx) -> Option<String> {
    lookup(r, ctx).as_ref().and_then(scalar)
}

/// `"/a/b"`, `"header:X"`, `"source"`, or text with `{ref}` holes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Template(pub String);

impl Template {
    fn holes(&self) -> Result<Vec<(usize, usize)>, String> {
        let s = &self.0;
        let mut out = vec![];
        let mut i = 0;
        while let Some(o) = s[i..].find('{').map(|o| o + i) {
            let c = s[o..].find('}').map(|c| c + o).ok_or_else(|| format!("template {s:?}: unclosed {{"))?;
            out.push((o, c));
            i = c + 1;
        }
        Ok(out)
    }

    pub fn validate(&self) -> Result<(), String> {
        if valid_ref(&self.0) {
            return Ok(());
        }
        for (o, c) in self.holes()? {
            let r = &self.0[o + 1..c];
            if !valid_ref(r) {
                return Err(format!("template {:?}: {{{r}}} is not a reference (/pointer, header:Name or source)", self.0));
            }
        }
        Ok(())
    }

    pub fn render(&self, ctx: &Ctx) -> String {
        if valid_ref(&self.0) {
            return lookup_str(&self.0, ctx).unwrap_or_default();
        }
        let Ok(holes) = self.holes() else { return self.0.clone() };
        let (mut out, mut last) = (String::new(), 0);
        for (o, c) in holes {
            out.push_str(&self.0[last..o]);
            out.push_str(&lookup_str(&self.0[o + 1..c], ctx).unwrap_or_default());
            last = c + 1;
        }
        out.push_str(&self.0[last..]);
        out
    }
}

/// One test on a reference. Shorthands: a string means `equals`, an array means one of.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Cond {
    Equals(String),
    OneOf(Vec<String>),
    Test(Test),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Test {
    #[serde(default)]
    pub exists: Option<bool>,
    #[serde(default)]
    pub equals: Option<String>,
    #[serde(default)]
    pub contains: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
}

impl Cond {
    fn holds(&self, v: Option<Value>) -> bool {
        let s = v.as_ref().and_then(scalar);
        match self {
            Cond::Equals(e) => s.as_deref() == Some(e),
            Cond::OneOf(a) => s.is_some_and(|s| a.contains(&s)),
            Cond::Test(t) => {
                let present = v.as_ref().is_some_and(|v| !v.is_null());
                t.exists.is_none_or(|e| e == present)
                    && t.equals.as_ref().is_none_or(|e| s.as_deref() == Some(e))
                    && t.contains.as_ref().is_none_or(|c| s.as_deref().is_some_and(|s| s.contains(c.as_str())))
                    && t.prefix.as_ref().is_none_or(|p| s.as_deref().is_some_and(|s| s.starts_with(p.as_str())))
            }
        }
    }
}

/// Every reference must satisfy its condition.
pub type Match = BTreeMap<String, Cond>;

pub fn matches(m: &Match, ctx: &Ctx) -> bool {
    m.iter().all(|(r, c)| c.holds(lookup(r, ctx)))
}

pub fn validate_match(m: &Match, what: &str) -> Result<(), String> {
    match m.keys().find(|r| !valid_ref(r)) {
        Some(r) => Err(format!("{what}: {r:?} is not a reference (/pointer, header:Name or source)")),
        None if m.is_empty() => Err(format!("{what}: empty match (would match everything)")),
        None => Ok(()),
    }
}

/// Event fields. `id` is required; `title` defaults to the first line of `text`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventMap {
    pub id: Template,
    #[serde(default)]
    pub title: Option<Template>,
    #[serde(default)]
    pub text: Option<Template>,
    #[serde(default)]
    pub actor: Option<Template>,
    #[serde(default)]
    pub channel: Option<Template>,
    /// Pointer to an array (of strings or `{name}` objects) or to one string.
    #[serde(default)]
    pub labels: Option<String>,
    /// Appended to the text.
    #[serde(default)]
    pub url: Option<Template>,
    #[serde(default)]
    pub updated_at: Option<Template>,
    /// Exact-duplicate key, prefixed with `{source}:`. Default `{id}:{updated_at}`.
    #[serde(default)]
    pub dedupe: Option<Template>,
}

/// How a workflow is chosen. The first rule that yields a non-empty name wins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum WorkflowRule {
    /// The value of a template (`{"field": "/wf"}`).
    Field(Template),
    /// A label `<prefix><workflow>`, from the mapped labels or from `from` (one string).
    LabelPrefix { prefix: String, #[serde(default)] from: Option<String> },
    /// Text starting with this mention, then `<workflow> <rest>`: the text becomes `<rest>`.
    Command { mention: String },
    /// Channel -> workflow.
    ByChannel(BTreeMap<String, String>),
    Default(String),
}

/// What happens to an item no workflow rule matched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Unmatched {
    /// Dropped without a trace (counted as ignored).
    #[default]
    Ignore,
    /// Passed on with no workflow, so intake records it as rejected.
    Reject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pipeline {
    /// Pointer to the array of items; absent = the document is the one item.
    #[serde(default)]
    pub items: Option<String>,
    /// Keep an item only if one of these matches (empty: keep all).
    #[serde(default)]
    pub only: Vec<Match>,
    /// Drop an item if any of these matches.
    #[serde(default)]
    pub ignore: Vec<Match>,
    pub map: EventMap,
    #[serde(default)]
    pub workflow: Vec<WorkflowRule>,
    #[serde(default)]
    pub unmatched: Unmatched,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mapped {
    pub events: Vec<IntakeEvent>,
    /// Items without an id, or without both title and text.
    pub skipped: usize,
    /// Items dropped by `only` / `ignore`, or with no workflow under `unmatched: ignore`.
    pub ignored: usize,
    /// Each event's `updated_at`, in the same order (for a `max:updated_at` cursor).
    pub updated: Vec<Option<String>>,
}

fn clip(s: String, max: usize) -> String {
    if s.chars().count() <= max { s } else { s.chars().take(max).collect() }
}

fn opt(t: &Option<Template>, ctx: &Ctx) -> String {
    t.as_ref().map(|t| t.render(ctx)).unwrap_or_default()
}

fn labels(item: &Value, ptr: &Option<String>) -> Vec<String> {
    let Some(v) = ptr.as_deref().and_then(|p| item.pointer(p)) else { return vec![] };
    match v {
        Value::Array(a) => a.iter().filter_map(|l| scalar(l).or_else(|| l.get("name").and_then(scalar))).collect(),
        other => scalar(other).into_iter().collect(),
    }
}

fn is_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `<mention> <workflow> <rest>` -> (Some(workflow), rest); anything else -> (None, text).
pub fn parse_command(text: &str, mention: &str) -> (Option<String>, String) {
    let t = text.trim();
    let Some(after) = t.strip_prefix(mention) else { return (None, t.to_string()) };
    let after = after.trim();
    let (tok, rest) = after.split_once(char::is_whitespace).unwrap_or((after, ""));
    if is_name(tok) { (Some(tok.to_string()), rest.trim().to_string()) } else { (None, after.to_string()) }
}

impl Pipeline {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(p) = &self.items {
            if !(p.is_empty() || p.starts_with('/')) {
                return Err(format!("items: {p:?} is not a JSON pointer"));
            }
        }
        self.only.iter().try_for_each(|m| validate_match(m, "only"))?;
        self.ignore.iter().try_for_each(|m| validate_match(m, "ignore"))?;
        let m = &self.map;
        m.id.validate().map_err(|e| format!("map.id: {e}"))?;
        for (name, t) in [("title", &m.title), ("text", &m.text), ("actor", &m.actor), ("channel", &m.channel), ("url", &m.url), ("updated_at", &m.updated_at), ("dedupe", &m.dedupe)] {
            if let Some(t) = t {
                t.validate().map_err(|e| format!("map.{name}: {e}"))?;
            }
        }
        for p in m.labels.iter().chain(self.workflow.iter().filter_map(|w| match w {
            WorkflowRule::LabelPrefix { from, .. } => from.as_ref(),
            _ => None,
        })) {
            if !p.starts_with('/') {
                return Err(format!("{p:?} is not a JSON pointer"));
            }
        }
        for w in &self.workflow {
            match w {
                WorkflowRule::Field(t) => t.validate().map_err(|e| format!("workflow.field: {e}"))?,
                WorkflowRule::LabelPrefix { prefix, .. } if prefix.is_empty() => return Err("workflow.label_prefix: empty prefix".into()),
                WorkflowRule::Command { mention } if mention.trim().is_empty() => return Err("workflow.command: empty mention".into()),
                WorkflowRule::ByChannel(m) if m.values().any(|w| !is_name(w)) => return Err("workflow.by_channel: bad workflow name".into()),
                WorkflowRule::Default(w) if !is_name(w) => return Err(format!("workflow.default: bad workflow name {w:?}")),
                _ => {}
            }
        }
        Ok(())
    }

    /// Runs the pipeline over `doc`. `Err` only if `items` does not point at an array.
    pub fn events(&self, source: &str, doc: &Value, headers: Option<&Headers>, now: Timestamp) -> Result<Mapped, String> {
        let list: Vec<&Value> = match &self.items {
            None => vec![doc],
            Some(p) => doc.pointer(p).and_then(Value::as_array).ok_or_else(|| format!("items {p:?} does not point at an array"))?.iter().collect(),
        };
        let mut out = Mapped::default();
        for item in list {
            let ctx = Ctx { source, item, headers };
            if (!self.only.is_empty() && !self.only.iter().any(|m| matches(m, &ctx))) || self.ignore.iter().any(|m| matches(m, &ctx)) {
                out.ignored += 1;
                continue;
            }
            match self.event(&ctx, now) {
                Some((ev, _)) if ev.requested_workflow.is_none() && self.unmatched == Unmatched::Ignore => out.ignored += 1,
                Some((ev, upd)) => {
                    out.events.push(ev);
                    out.updated.push(upd);
                }
                None => out.skipped += 1,
            }
        }
        Ok(out)
    }

    fn event(&self, ctx: &Ctx, now: Timestamp) -> Option<(IntakeEvent, Option<String>)> {
        let m = &self.map;
        let id = m.id.render(ctx);
        if id.is_empty() {
            return None;
        }
        let channel = opt(&m.channel, ctx);
        let labels = labels(ctx.item, &m.labels);
        let mut text = opt(&m.text, ctx);
        let mut wf = None;
        for rule in &self.workflow {
            wf = match rule {
                WorkflowRule::Field(t) => Some(t.render(ctx)),
                WorkflowRule::LabelPrefix { prefix, from } => {
                    let pool = match from {
                        Some(p) => ctx.item.pointer(p).and_then(scalar).into_iter().collect(),
                        None => labels.clone(),
                    };
                    pool.iter().find_map(|l| l.strip_prefix(prefix.as_str()).map(String::from))
                }
                WorkflowRule::Command { mention } => {
                    let (w, rest) = parse_command(&text, mention);
                    if w.is_some() {
                        text = rest;
                    }
                    w
                }
                WorkflowRule::ByChannel(map) => map.get(&channel).cloned(),
                WorkflowRule::Default(w) => Some(w.clone()),
            }
            .filter(|w| is_name(w));
            if wf.is_some() {
                break;
            }
        }
        let title = match opt(&m.title, ctx) {
            t if t.is_empty() => text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(MAX_DEFAULT_TITLE).collect(),
            t => t,
        };
        if title.is_empty() {
            return None;
        }
        let url = opt(&m.url, ctx);
        if !url.is_empty() {
            text = format!("{text}\n{url}").trim_start().to_string();
        }
        let upd = Some(opt(&m.updated_at, ctx)).filter(|u| !u.is_empty());
        let dedupe = match &m.dedupe {
            Some(t) => t.render(ctx),
            None => format!("{id}:{}", upd.as_deref().unwrap_or("")),
        };
        let actor = Some(opt(&m.actor, ctx)).filter(|a| !a.is_empty()).unwrap_or_else(|| "unknown".into());
        let ev = IntakeEvent {
            source: ctx.source.into(),
            source_ref: id,
            dedupe_key: format!("{}:{dedupe}", ctx.source),
            actor,
            channel,
            title: clip(title, MAX_TITLE),
            text: clip(text, MAX_TEXT),
            labels,
            requested_workflow: wf,
            received_at: now,
        };
        Some((ev, upd))
    }
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
        return mapped
            .updated
            .iter()
            .flatten()
            .fold(None::<&String>, |m, u| match m {
                Some(c) if !cursor_gt(u, c) => Some(c),
                _ => Some(u),
            })
            .cloned();
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

    fn pipe(v: Value) -> Pipeline {
        let p: Pipeline = serde_json::from_value(v).unwrap();
        p.validate().unwrap();
        p
    }

    #[test]
    fn maps_items_with_pointers_templates_and_default_dedupe() {
        let p = pipe(json!({"items": "/tickets",
            "map": {"id": "/key", "title": "/summary", "text": "/body", "actor": "/who/name", "channel": "/project",
                    "labels": "/labels", "url": "/url", "updated_at": "/updated"},
            "workflow": [{"field": "/wf"}, {"default": "demo"}]}));
        let r = json!({"tickets": [
            {"key": "PRJ-1", "summary": "Fix it", "body": "b", "who": {"name": "ann"}, "project": "PRJ", "labels": ["a", {"name": "b"}], "updated": "2025-01-01", "wf": "triage"},
            {"summary": "no id"},
            {"key": 7, "summary": "bare"}]});
        let m = p.events("jira", &r, None, 5).unwrap();
        assert_eq!((m.events.len(), m.skipped), (2, 1));
        let e = &m.events[0];
        assert_eq!((e.dedupe_key.as_str(), e.actor.as_str(), e.channel.as_str()), ("jira:PRJ-1:2025-01-01", "ann", "PRJ"));
        assert_eq!((e.labels.clone(), e.requested_workflow.as_deref()), (vec!["a".to_string(), "b".into()], Some("triage")));
        let e = &m.events[1];
        assert_eq!((e.dedupe_key.as_str(), e.actor.as_str(), e.requested_workflow.as_deref()), ("jira:7:", "unknown", Some("demo")));
        assert_eq!(next_cursor("max:updated_at", &r, &m).as_deref(), Some("2025-01-01"));
        assert_eq!(next_cursor("/tickets/0/key", &r, &m).as_deref(), Some("PRJ-1"));
        assert!(p.events("j", &json!({"x": 1}), None, 0).is_err());
    }

    #[test]
    fn chat_style_filters_command_and_channel_defaults() {
        let p = pipe(json!({
            "only": [{"/type": "event_callback", "/event/type": ["message", "app_mention"]}],
            "ignore": [{"/event/bot_id": {"exists": true}}, {"/event/user": "UBOT"},
                       {"/event/type": "message", "/event/text": {"contains": "<@UBOT>"}}],
            "map": {"id": "{/event/channel}/{/event/ts}", "text": "/event/text", "actor": "/event/user",
                    "channel": "/event/channel", "dedupe": "{/event_id}|{header:X-Retry}"},
            "workflow": [{"command": {"mention": "<@UBOT>"}}, {"by_channel": {"C1": "demo"}}]}));
        let h = Headers::from_pairs(&[("X-Retry", "0")]);
        let ev = |t: &str, text: &str, extra: Value| {
            let mut e = json!({"type": "app_mention", "user": "U1", "channel": "C1", "ts": "1.0", "text": text});
            e.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            e["type"] = json!(t);
            json!({"type": "event_callback", "event_id": "Ev1", "event": e})
        };
        let m = p.events("chat", &ev("app_mention", "<@UBOT> triage the build\nis red", json!({})), Some(&h), 9).unwrap();
        let e = &m.events[0];
        assert_eq!((e.requested_workflow.as_deref(), e.text.as_str(), e.title.as_str()), (Some("triage"), "the build\nis red", "the build"));
        assert_eq!((e.source_ref.as_str(), e.dedupe_key.as_str()), ("C1/1.0", "chat:Ev1|0"));
        let m = p.events("chat", &ev("message", "plain words", json!({})), Some(&h), 9).unwrap();
        assert_eq!(m.events[0].requested_workflow.as_deref(), Some("demo"));
        for (t, text, extra) in [("message", "<@UBOT> x", json!({})), ("app_mention", "hi", json!({"bot_id": "B"})), ("app_mention", "hi", json!({"user": "UBOT"})), ("reaction_added", "hi", json!({}))] {
            let m = p.events("chat", &ev(t, text, extra), Some(&h), 9).unwrap();
            assert_eq!((m.events.len(), m.ignored), (0, 1), "{t} {text}");
        }
    }

    #[test]
    fn label_prefix_and_unmatched() {
        let base = json!({"map": {"id": "{/repo}#{/number}", "title": "/title", "labels": "/labels"},
            "workflow": [{"label_prefix": {"prefix": "factory:", "from": "/label/name"}}, {"label_prefix": {"prefix": "factory:"}}]});
        let p = pipe(base.clone());
        let item = json!({"repo": "o/r", "number": 3, "title": "t", "labels": [{"name": "bug"}, {"name": "factory:triage"}], "label": {"name": "factory:demo"}});
        assert_eq!(p.events("gh", &item, None, 0).unwrap().events[0].requested_workflow.as_deref(), Some("demo"));
        let plain = json!({"repo": "o/r", "number": 3, "title": "t", "labels": ["bug"]});
        assert_eq!(p.events("gh", &plain, None, 0).unwrap().ignored, 1);
        let mut rej = base;
        rej["unmatched"] = json!("reject");
        let m = pipe(rej).events("gh", &plain, None, 0).unwrap();
        assert_eq!((m.events.len(), m.events[0].requested_workflow.clone()), (1, None));
    }

    #[test]
    fn validation_catches_bad_references() {
        for bad in [
            json!({"map": {"id": "{nope}"}}),
            json!({"map": {"id": "/x"}, "only": [{}]}),
            json!({"map": {"id": "/x"}, "ignore": [{"x": "y"}]}),
            json!({"map": {"id": "/x", "labels": "labels"}}),
            json!({"map": {"id": "/x"}, "workflow": [{"default": "a b"}]}),
            json!({"map": {"id": "{/x"}}),
        ] {
            let p: Pipeline = serde_json::from_value(bad.clone()).unwrap();
            assert!(p.validate().is_err(), "{bad}");
        }
        assert!(serde_json::from_value::<Pipeline>(json!({"map": {"id": "/x"}, "workflow": [{"guess": 1}]})).is_err());
    }

    #[test]
    fn secrets_are_spotted() {
        assert!(looks_like_secret(concat!("xo", "xb-123456789-abcdefghij")));
        assert!(looks_like_secret(concat!("gh", "p_abcdefghijklmnop")));
        assert!(looks_like_secret("Zk3jQ9x2LmN8vB4tR7wY1pC6sD5fG0hJa"));
        assert!(!looks_like_secret("@linear/mcp-server"));
        assert!(!looks_like_secret("{cursor}"));
    }
}

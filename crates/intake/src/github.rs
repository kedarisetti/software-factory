//! GitHub Issues connector (TRG-1): webhook and polling both yield the same event shape.

use serde_json::Value;
use sf_core::Timestamp;

use crate::connector::{req, str_at, ConnectorError, Headers};
use crate::event::IntakeEvent;
use crate::sig;

/// Label prefix that requests a workflow: `factory:fix-bug`.
pub const LABEL_PREFIX: &str = "factory:";

#[derive(Debug, Clone)]
pub struct GithubConfig {
    /// Configured source name: the event's `source`, and the prefix of its dedupe key.
    pub source: String,
    pub webhook_secret: String,
}

pub fn verify(secret: &str, headers: &Headers, body: &[u8]) -> Result<(), ConnectorError> {
    let provided = headers.require("X-Hub-Signature-256")?;
    match provided.strip_prefix("sha256=") {
        Some(h) if sig::verify_hex(secret.as_bytes(), body, h) => Ok(()),
        _ => Err(ConnectorError::BadSignature),
    }
}

/// Handles an `issues` delivery with action opened, labeled or reopened.
pub fn handle(cfg: &GithubConfig, headers: &Headers, body: &[u8], now_ms: Timestamp) -> Result<IntakeEvent, ConnectorError> {
    verify(&cfg.webhook_secret, headers, body)?;
    let kind = headers.require("X-GitHub-Event")?;
    if kind != "issues" {
        return Err(ConnectorError::Unsupported(format!("github event {kind}")));
    }
    let v: Value = serde_json::from_slice(body).map_err(|e| ConnectorError::Malformed(e.to_string()))?;
    let action = req(&v, "action")?;
    if !matches!(action, "opened" | "labeled" | "reopened") {
        return Err(ConnectorError::Unsupported(format!("issues action {action}")));
    }
    let repo = v.get("repository").map(|r| req(r, "full_name")).transpose()?.ok_or_else(|| malformed("repository"))?;
    let issue = v.get("issue").ok_or_else(|| malformed("issue"))?;
    let actor = v.get("sender").map(|s| req(s, "login")).transpose()?.or_else(|| issue.pointer("/user/login").and_then(Value::as_str));
    let added = v.pointer("/label/name").and_then(Value::as_str);
    let labels = labels_of(issue);
    let requested_workflow = if action == "labeled" {
        added.and_then(workflow_of)
    } else {
        labels.iter().find_map(|l| workflow_of(l))
    };
    if requested_workflow.is_none() {
        return Err(ConnectorError::Unsupported("no factory:<workflow> label".into()));
    }
    let number = issue.get("number").and_then(Value::as_u64).ok_or_else(|| malformed("issue.number"))?;
    let base = format!("{}:{repo}#{number}:{action}", cfg.source);
    let dedupe_key = match (action, added, headers.get("X-GitHub-Delivery")) {
        ("labeled", Some(l), _) => format!("{base}:{l}"),
        // A reopen can legitimately recur, so it is keyed by delivery: redelivery dedupes, a new reopen does not.
        ("reopened", _, Some(d)) => format!("{base}:{d}"),
        _ => base,
    };
    Ok(build(&cfg.source, repo, issue, actor.unwrap_or("unknown"), labels, requested_workflow, dedupe_key, now_ms))
}

/// Normalises one issue object from `GET /repos/{repo}/issues` (polling). Pull requests are skipped.
pub fn from_issue(source: &str, repo: &str, issue: &Value, now_ms: Timestamp) -> Result<IntakeEvent, ConnectorError> {
    if issue.get("pull_request").is_some() {
        return Err(ConnectorError::Unsupported("pull request".into()));
    }
    let labels = labels_of(issue);
    let wf = labels.iter().find_map(|l| workflow_of(l));
    if wf.is_none() {
        return Err(ConnectorError::Unsupported("no factory:<workflow> label".into()));
    }
    let number = issue.get("number").and_then(Value::as_u64).ok_or_else(|| malformed("number"))?;
    let actor = issue.pointer("/user/login").and_then(Value::as_str).unwrap_or("unknown");
    Ok(build(source, repo, issue, actor, labels, wf, format!("{source}:{repo}#{number}:opened"), now_ms))
}

#[allow(clippy::too_many_arguments)]
fn build(source: &str, repo: &str, issue: &Value, actor: &str, labels: Vec<String>, wf: Option<String>, key: String, now: Timestamp) -> IntakeEvent {
    let number = issue.get("number").and_then(Value::as_u64).unwrap_or(0);
    IntakeEvent {
        source: source.into(),
        source_ref: format!("{repo}#{number}"),
        dedupe_key: key,
        actor: actor.into(),
        channel: repo.into(),
        title: str_at(issue, "title").unwrap_or("").into(),
        text: str_at(issue, "body").unwrap_or("").into(),
        labels,
        requested_workflow: wf,
        received_at: now,
    }
}

fn malformed(what: &str) -> ConnectorError {
    ConnectorError::Malformed(format!("missing {what}"))
}

fn workflow_of(label: &str) -> Option<String> {
    label.strip_prefix(LABEL_PREFIX).filter(|w| !w.is_empty()).map(String::from)
}

fn labels_of(issue: &Value) -> Vec<String> {
    let names = issue.get("labels").and_then(Value::as_array).into_iter().flatten();
    names.filter_map(|l| l.get("name").and_then(Value::as_str).or_else(|| l.as_str())).map(String::from).collect()
}

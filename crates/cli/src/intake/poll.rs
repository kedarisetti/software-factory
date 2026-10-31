//! GitHub issue polling (TRG-1): same events as the webhook, cursor on `updated_at` (read-only).

use std::process::Command;

use serde_json::{json, Value};
use std::time::Duration;

use sf_core::{Store, Timestamp};
use sf_intake::mcp::{cursor_gt, map_items, next_cursor};
use sf_intake::{github, ConnectorError, Intake};

use super::config::{Kind, Loaded, McpSrc, Source};
use super::state::State;
use super::{connectors, mcp_client};

/// `2024-05-06T07:08:09Z` -> epoch milliseconds.
pub fn parse_iso(s: &str) -> Option<Timestamp> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'Z' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(((days * 24 + hh) * 60 + mm) * 60 * 1000 + ss * 1000).ok()
}

fn safe(s: &str, extra: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

/// Open issues carrying `label`, via `gh api` (no shell, read-only GET).
pub fn fetch(repo: &str, label: &str) -> Result<Value, String> {
    if !safe(repo, "-_./") || repo.matches('/').count() != 1 || !safe(label, "-_.:") {
        return Err("repo must be org/repo and label plain characters".into());
    }
    let label = label.replace(':', "%3A");
    let path = format!("repos/{repo}/issues?state=open&labels={label}&sort=updated&direction=asc&per_page=100");
    let out = Command::new("gh").args(["api", "--method", "GET", &path]).output().map_err(|e| format!("gh: {e}"))?;
    if !out.status.success() {
        return Err(format!("gh api failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("gh api output: {e}"))
}

/// Feeds issues updated after the `github-poll:{repo}` cursor to `Intake::accept`, then advances it.
/// The source named `github` keeps the original cursor name; other github sources add their name.
pub fn poll(store: &dyn Store, intake: &Intake, source: &str, repo: &str, issues: &Value, now: Timestamp) -> Result<Value, String> {
    let name = if source == "github" { format!("github-poll:{repo}") } else { format!("github-poll:{source}:{repo}") };
    let cursor = store.get_cursor(&name).map_err(|e| e.to_string())?.unwrap_or(0);
    let list = issues.as_array().ok_or("issues must be a JSON array")?;
    let (mut newest, mut seen, mut skipped) = (cursor, 0, 0);
    let mut accepted = Vec::new();
    for issue in list {
        let updated = issue.get("updated_at").and_then(Value::as_str).and_then(parse_iso).unwrap_or(0);
        if updated <= cursor {
            continue;
        }
        seen += 1;
        newest = newest.max(updated);
        match github::from_issue(source, repo, issue, now) {
            Ok(ev) => {
                let o = intake.accept(&ev).map_err(|e| e.to_string())?;
                accepted.push(json!({"item_id": o.item.item_id, "status": o.status, "inserted": o.inserted}));
            }
            Err(ConnectorError::Unsupported(_)) => skipped += 1,
            Err(e) => return Err(e.to_string()),
        }
    }
    store.set_cursor(&name, newest).map_err(|e| e.to_string())?;
    Ok(json!({"source": source, "repo": repo, "considered": seen, "skipped": skipped, "items": accepted, "cursor": newest}))
}

/// Last poll time per source (epoch ms), in the store's integer cursors.
fn last_poll(store: &dyn Store, source: &str) -> Result<u64, String> {
    Ok(store.get_cursor(&format!("poll-at:{source}")).map_err(|e| e.to_string())?.unwrap_or(0))
}

fn due(store: &dyn Store, source: &str, interval_s: u64, now: Timestamp, force: bool) -> Result<bool, String> {
    Ok(force || now.saturating_sub(last_poll(store, source)?) >= interval_s * 1000)
}

/// One MCP poll: approval gate, call the tool with the saved cursor, map items, `accept`, save the
/// cursor. The cursor only moves after every item was accepted, so a failure re-reads, never skips.
pub fn poll_mcp(store: &dyn Store, state: &State, l: &Loaded, src: &Source, m: &McpSrc, now: Timestamp) -> Result<Value, String> {
    let approval = connectors::state(store, &src.name, &m.digest())?;
    if !approval.enabled() {
        return Err(connectors::refuse(store, &src.name, m, &approval)?);
    }
    let env = m.resolve_env()?;
    let ckey = format!("mcp-cursor:{}", src.name);
    let cursor = state.get(&ckey)?;
    let arguments = mcp_client::substitute(&m.arguments, cursor.as_deref().unwrap_or(""));
    let call = mcp_client::Call { command: &m.command, env, tool: &m.tool, arguments, timeout: Duration::from_millis(m.timeout_ms) };
    let result = mcp_client::call_tool(&call).map_err(|e| format!("source {}: {e}", src.name))?;
    let mapped = map_items(&src.name, &m.map, m.default_workflow.as_deref(), &result, now).map_err(|e| format!("source {}: {e}", src.name))?;
    let mut items = Vec::new();
    for ev in &mapped.events {
        let o = l.intake.accept(ev).map_err(|e| e.to_string())?;
        items.push(json!({"item_id": o.item.item_id, "status": o.status, "inserted": o.inserted}));
    }
    let next = m.cursor.as_deref().and_then(|spec| next_cursor(spec, &result, &mapped));
    let advanced = match (next, &cursor) {
        (Some(n), Some(old)) if m.cursor.as_deref() == Some("max:updated_at") && !cursor_gt(&n, old) => None,
        (n, _) => n,
    };
    if let Some(n) = &advanced {
        state.set(&ckey, n)?;
    }
    store.set_cursor(&format!("poll-at:{}", src.name), now).map_err(|e| e.to_string())?;
    Ok(json!({"source": src.name, "kind": "mcp", "considered": mapped.events.len(), "skipped": mapped.skipped,
              "items": items, "cursor": advanced.or(cursor)}))
}

/// `sf intake poll [--source name] [--now]`: every enabled pollable source whose interval elapsed.
/// A source named explicitly that is not approved is an error; in the sweep it is reported and skipped.
pub fn poll_sources(store: &dyn Store, state: &State, l: &Loaded, only: Option<&str>, force: bool, now: Timestamp) -> Result<Value, String> {
    if let Some(n) = only {
        if l.source(n).is_none() {
            return Err(format!("no source named {n:?} in the config"));
        }
    }
    let mut results = Vec::new();
    for src in l.sources.iter().filter(|s| only.is_none_or(|n| n == s.name)) {
        match &src.kind {
            Kind::Mcp(m) => {
                if !due(store, &src.name, m.poll_interval_s, now, force)? {
                    results.push(json!({"source": src.name, "status": "not due"}));
                    continue;
                }
                match poll_mcp(store, state, l, src, m, now) {
                    Err(e) if only.is_none() && e.contains("is disabled") => results.push(json!({"source": src.name, "status": "blocked", "error": e})),
                    Err(e) if only.is_none() => results.push(json!({"source": src.name, "status": "error", "error": e})),
                    r => results.push(r?),
                }
            }
            Kind::Github(g) => {
                let Some(p) = &g.poll else {
                    if only.is_some() {
                        return Err(format!("source {} has no poll settings (webhook only)", src.name));
                    }
                    continue;
                };
                if !due(store, &src.name, p.interval_s, now, force)? {
                    results.push(json!({"source": src.name, "status": "not due"}));
                    continue;
                }
                for repo in &p.repos {
                    match fetch(repo, &p.label).and_then(|issues| poll(store, &l.intake, &src.name, repo, &issues, now)) {
                        Err(e) if only.is_none() => results.push(json!({"source": src.name, "repo": repo, "status": "error", "error": e})),
                        r => results.push(r?),
                    }
                }
                store.set_cursor(&format!("poll-at:{}", src.name), now).map_err(|e| e.to_string())?;
            }
            Kind::Slack(_) => {
                if only.is_some() {
                    return Err(format!("source {} is webhook only and cannot be polled", src.name));
                }
            }
        }
    }
    Ok(json!({"polled": results}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso_timestamps() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2024-03-01T00:00:01Z"), Some(1_709_251_201_000));
        assert_eq!(parse_iso("2024-02-29T12:00:00Z"), Some(1_709_208_000_000));
        assert_eq!(parse_iso("nope"), None);
    }
}

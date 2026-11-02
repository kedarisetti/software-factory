//! Polling (TRG-1): `command` and `mcp` sources share one path: approval gate, run the process with
//! the saved cursor, feed the JSON through the source's `events` pipeline, `Intake::accept` each event,
//! then save the cursor. Webhook sources are never polled.

use std::time::Duration;

use serde_json::{json, Value};
use sf_core::{Store, Timestamp};
use sf_intake::pipeline::{cursor_gt, next_cursor};

use super::config::{Kind, Loaded, ProcSrc, Source};
use super::state::State;
use super::{command, connectors, mcp_client};

/// Last poll time per source (epoch ms), in the store's integer cursors.
fn last_poll(store: &dyn Store, source: &str) -> Result<u64, String> {
    Ok(store
        .get_cursor(&format!("poll-at:{source}"))
        .map_err(|e| e.to_string())?
        .unwrap_or(0))
}

fn due(
    store: &dyn Store,
    source: &str,
    interval_s: u64,
    now: Timestamp,
    force: bool,
) -> Result<bool, String> {
    Ok(force || now.saturating_sub(last_poll(store, source)?) >= interval_s * 1000)
}

/// Runs the source's process once and returns its JSON result.
fn fetch(src: &Source, p: &ProcSrc, cursor: &str) -> Result<Value, String> {
    let env = p.resolve_env()?;
    let timeout = Duration::from_millis(p.timeout_ms);
    let r = match &p.tool {
        Some(tool) => mcp_client::call_tool(&mcp_client::Call {
            command: &p.command,
            env,
            tool,
            arguments: mcp_client::substitute(&p.arguments, cursor),
            timeout,
        }),
        None => command::run(&command::Run {
            command: &command::substitute(&p.command, cursor),
            env,
            timeout,
        }),
    };
    r.map_err(|e| format!("source {}: {e}", src.name))
}

/// One poll: approval gate, run with the saved cursor, map, `accept`, save the cursor. The cursor only
/// moves after every event was accepted, so a failure re-reads, never skips; `max:updated_at` never
/// moves backwards.
pub fn poll_proc(
    store: &dyn Store,
    state: &State,
    l: &Loaded,
    src: &Source,
    p: &ProcSrc,
    now: Timestamp,
) -> Result<Value, String> {
    let approval = connectors::state(store, &src.name, &p.digest())?;
    if !approval.enabled() {
        return Err(connectors::refuse(store, &src.name, p, &approval)?);
    }
    let ckey = format!("cursor:{}", src.name);
    let cursor = state.get(&ckey)?;
    let used = cursor
        .clone()
        .or_else(|| p.cursor_start.clone())
        .unwrap_or_default();
    let result = fetch(src, p, &used)?;
    let mapped = p
        .events
        .events(&src.name, &result, None, now)
        .map_err(|e| format!("source {}: {e}", src.name))?;
    let mut items = Vec::new();
    for ev in &mapped.events {
        let o = l.intake.accept(ev).map_err(|e| e.to_string())?;
        items.push(json!({"item_id": o.item.item_id, "status": o.status, "inserted": o.inserted}));
    }
    let next = p
        .cursor
        .as_deref()
        .and_then(|spec| next_cursor(spec, &result, &mapped));
    let advanced = match (next, &cursor) {
        (Some(n), Some(old))
            if p.cursor.as_deref() == Some("max:updated_at") && !cursor_gt(&n, old) =>
        {
            None
        }
        (n, _) => n,
    };
    if let Some(n) = &advanced {
        state.set(&ckey, n)?;
    }
    store
        .set_cursor(&format!("poll-at:{}", src.name), now)
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"source": src.name, "kind": p.kind, "considered": mapped.events.len(), "skipped": mapped.skipped, "ignored": mapped.ignored,
              "items": items, "cursor": advanced.or(cursor)}),
    )
}

/// `sf intake poll [--source name] [--now]`: every enabled pollable source whose interval elapsed.
/// A source named explicitly that is not approved is an error; in the sweep it is reported and skipped.
pub fn poll_sources(
    store: &dyn Store,
    state: &State,
    l: &Loaded,
    only: Option<&str>,
    force: bool,
    now: Timestamp,
) -> Result<Value, String> {
    if let Some(n) = only {
        if l.source(n).is_none() {
            return Err(format!("no source named {n:?} in the config"));
        }
    }
    let mut results = Vec::new();
    for src in l
        .sources
        .iter()
        .filter(|s| only.is_none_or(|n| n == s.name))
    {
        match &src.kind {
            Kind::Webhook(_) => {
                if only.is_some() {
                    return Err(format!(
                        "source {} is webhook only and cannot be polled",
                        src.name
                    ));
                }
            }
            Kind::Proc(p) => {
                if !due(store, &src.name, p.poll_interval_s, now, force)? {
                    results.push(json!({"source": src.name, "status": "not due"}));
                    continue;
                }
                match poll_proc(store, state, l, src, p, now) {
                    Err(e) if only.is_none() && e.contains("is disabled") => {
                        results.push(json!({"source": src.name, "status": "blocked", "error": e}))
                    }
                    Err(e) if only.is_none() => {
                        // Back off for one interval, so a daemon does not hammer a failing source every tick.
                        store
                            .set_cursor(&format!("poll-at:{}", src.name), now)
                            .map_err(|e| e.to_string())?;
                        results.push(json!({"source": src.name, "status": "error", "error": e}));
                    }
                    r => results.push(r?),
                }
            }
        }
    }
    Ok(json!({"polled": results}))
}

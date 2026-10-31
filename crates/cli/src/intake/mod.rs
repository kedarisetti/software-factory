//! `sf intake ...` (TRG-1 to TRG-4): serve webhooks, poll GitHub, dispatch the queue, list, bottlenecks.

mod config;
mod http;
mod poll;

use std::net::TcpListener;
use std::sync::Arc;

use serde_json::{json, Value};
use sf_core::{Clock, ItemStatus, Queue, SystemClock, WorkItem};
use sf_orchestrator::{Engine, StepDef, StepResult, StaticWorkflow};
use sf_store_sqlite::SqliteStore;

use config::{load, Budget, Loaded};

const DAY_MS: u64 = 86_400_000;

/// Built-in `triage` v1: reads the work item and wraps its text as untrusted content (SBX-3).
pub fn triage() -> Arc<StaticWorkflow> {
    StaticWorkflow::new(
        "triage",
        1,
        vec![StepDef::new("read-item", |ctx| {
            let Some(input) = ctx.input() else { return StepResult::Fail("no input".into()) };
            let text = input["payload"]["text"].as_str().unwrap_or("").to_string();
            match ctx.untrusted(input["source"].as_str().unwrap_or("unknown"), &text) {
                Ok(u) => StepResult::Done(json!({"item_id": input["item_id"], "chars": u.content.len()})),
                Err(e) => e.into(),
            }
        })],
    )
}

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn num(args: &[String], name: &str, default: u64) -> Result<u64, String> {
    opt(args, name).map_or(Ok(default), |v| v.parse().map_err(|_| format!("{name} needs a number")))
}

fn config_path(args: &[String]) -> Result<String, String> {
    opt(args, "--config").ok_or_else(|| "--config <intake.json> is required".into())
}

/// `args` start at the intake subcommand (`serve`, `poll`, `dispatch`, `list`, `bottlenecks`).
pub fn run(args: &[String], store: Arc<SqliteStore>, engine: &Engine) -> Result<Value, String> {
    let queue: Arc<dyn Queue> = store.clone();
    let sub = args.first().map(String::as_str).unwrap_or("");
    match sub {
        "serve" => serve(args, queue),
        "poll" => {
            if args.get(1).map(String::as_str) != Some("github") {
                return Err("usage: sf intake poll github --repo <org/repo> [--label factory] [--from-file f.json] --config c".into());
            }
            let loaded = load(&config_path(args)?, queue, false)?;
            let repo = opt(args, "--repo").ok_or("--repo <org/repo> is required")?;
            let issues = match opt(args, "--from-file") {
                Some(f) => serde_json::from_str(&std::fs::read_to_string(&f).map_err(|e| format!("{f}: {e}"))?).map_err(|e| format!("{f}: {e}"))?,
                None => poll::fetch(&repo, &opt(args, "--label").unwrap_or_else(|| "factory".into()))?,
            };
            poll::poll(&*store, &loaded.intake, &repo, &issues, SystemClock.now())
        }
        "dispatch" => {
            let loaded = load(&config_path(args)?, queue, false)?;
            dispatch(&loaded, engine, num(args, "--max", 10)? as usize)
        }
        "list" => {
            let statuses = match opt(args, "--status") {
                Some(s) => s.split(',').map(parse_status).collect::<Result<Vec<_>, _>>()?,
                None => ALL.to_vec(),
            };
            Ok(json!(store.list_items(&statuses, 1000).map_err(|e| e.to_string())?))
        }
        "bottlenecks" => {
            let loaded = load(&config_path(args)?, queue, false)?;
            let window = num(args, "--window-days", 7)? * DAY_MS;
            let out = loaded.intake.run_bottlenecks(SystemClock.now(), window, num(args, "--min", 3)? as usize).map_err(|e| e.to_string())?;
            Ok(json!(out.iter().map(|o| json!({"item_id": o.item.item_id, "inserted": o.inserted, "title": o.item.payload["title"], "count": o.item.payload["count"]})).collect::<Vec<_>>()))
        }
        other => Err(format!("unknown intake subcommand {other:?}: serve | poll | dispatch | list | bottlenecks")),
    }
}

const ALL: [ItemStatus; 6] =
    [ItemStatus::Queued, ItemStatus::Started, ItemStatus::Rejected, ItemStatus::RateLimited, ItemStatus::Backlog, ItemStatus::Done];

fn parse_status(s: &str) -> Result<ItemStatus, String> {
    serde_json::from_value(json!(s.trim())).map_err(|_| format!("unknown status {s:?}"))
}

fn serve(args: &[String], queue: Arc<dyn Queue>) -> Result<Value, String> {
    let addr = opt(args, "--addr").ok_or("--addr <host:port> is required")?;
    let loaded = Arc::new(load(&config_path(args)?, queue, true)?);
    let listener = TcpListener::bind(&addr).map_err(|e| format!("bind {addr}: {e}"))?;
    println!("{}", json!({"listening": listener.local_addr().map_err(|e| e.to_string())?.to_string()}));
    http::serve(listener, loaded)
}

/// Why intake should stop starting work: spend since 00:00 UTC is at or over a daily budget.
fn over_budget(engine: &Engine, b: &Budget, now: u64) -> Result<Option<(u64, u64)>, String> {
    let (tokens, usd) = engine.spent_since(now / DAY_MS * DAY_MS).map_err(|e| e.to_string())?;
    let over = b.daily_tokens.is_some_and(|m| tokens >= m) || b.daily_usd_micros.is_some_and(|m| usd >= m);
    Ok(over.then_some((tokens, usd)))
}

/// Queued items in priority order become runs. Crossing the daily budget stops dispatch only (GOV).
fn dispatch(l: &Loaded, engine: &Engine, max: usize) -> Result<Value, String> {
    let q = l.intake.queue();
    let (mut started, mut failed, mut paused) = (vec![], vec![], Value::Null);
    for mut item in q.list_items(&[ItemStatus::Queued], max).map_err(|e| e.to_string())? {
        if let Some((tokens, usd)) = over_budget(engine, &l.budget, SystemClock.now())? {
            paused = json!({"reason": "paused: daily budget", "spent_tokens": tokens, "spent_usd_micros": usd});
            break;
        }
        match start(l, engine, &item) {
            Ok(run_id) => {
                started.push(json!({"item_id": item.item_id, "run_id": run_id, "workflow": item.workflow}));
                item.status = ItemStatus::Started;
                item.run_id = Some(run_id);
            }
            Err(why) => {
                failed.push(json!({"item_id": item.item_id, "error": why}));
                item.status = ItemStatus::Rejected;
                item.reason = Some(why);
            }
        }
        item.updated_at = SystemClock.now();
        q.update_item(&item).map_err(|e| e.to_string())?;
    }
    Ok(json!({"started": started, "failed": failed, "paused": paused}))
}

/// The trigger ref is `{source}:{source_ref}`; the engine uses its prefix for the chat-deploy rule.
fn start(l: &Loaded, engine: &Engine, item: &WorkItem) -> Result<String, String> {
    let wf = item.workflow.as_deref().ok_or("item has no workflow")?;
    let input = json!({
        "item_id": item.item_id, "source": item.source, "source_ref": item.source_ref,
        "actor": item.actor, "channel": item.channel, "payload": item.payload,
    });
    let trigger = format!("{}:{}", item.source, item.source_ref);
    engine.start_run_with_input(wf, &trigger, l.budget.run_tokens, l.budget.run_usd_micros, input).map_err(|e| e.to_string())
}

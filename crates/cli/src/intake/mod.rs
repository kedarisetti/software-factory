//! `sf intake ...` (TRG-1 to TRG-4): serve webhooks, poll sources (command, MCP), approve connectors,
//! dispatch the queue, run the whole loop as a daemon, list, bottlenecks. Every source is configured;
//! no vendor is built in (docs/intake.md, docs/presets/).

mod command;
mod config;
mod connectors;
mod http;
mod mcp_client;
mod poll;
mod state;

use std::net::TcpListener;
use std::sync::Arc;

use serde_json::{json, Value};
use sf_core::{Clock, ItemStatus, Queue, SystemClock, WorkItem};
use sf_orchestrator::{Engine, StaticWorkflow, StepDef, StepResult};
use sf_store_sqlite::SqliteStore;

use config::{load, Budget, Loaded};

const DAY_MS: u64 = 86_400_000;

/// Built-in `triage` v1: reads the work item and wraps its text as untrusted content (SBX-3).
pub fn triage() -> Arc<StaticWorkflow> {
    StaticWorkflow::new(
        "triage",
        1,
        vec![StepDef::new("read-item", |ctx| {
            let Some(input) = ctx.input() else {
                return StepResult::Fail("no input".into());
            };
            let text = input["payload"]["text"].as_str().unwrap_or("").to_string();
            match ctx.untrusted(input["source"].as_str().unwrap_or("unknown"), &text) {
                Ok(u) => {
                    StepResult::Done(json!({"item_id": input["item_id"], "chars": u.content.len()}))
                }
                Err(e) => e.into(),
            }
        })],
    )
}

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn num(args: &[String], name: &str, default: u64) -> Result<u64, String> {
    opt(args, name).map_or(Ok(default), |v| {
        v.parse().map_err(|_| format!("{name} needs a number"))
    })
}

fn config_path(args: &[String]) -> Result<String, String> {
    opt(args, "--config").ok_or_else(|| "--config <intake.json> is required".into())
}

/// `args` start at the intake subcommand (`serve`, `poll`, `connectors`, `dispatch`, `daemon`, `list`, `bottlenecks`).
/// `db` locates the cursor state file; `approver` is the global `--as`.
pub fn run(
    args: &[String],
    store: Arc<SqliteStore>,
    engine: &Engine,
    db: &str,
    approver: Option<String>,
) -> Result<Value, String> {
    let queue: Arc<dyn Queue> = store.clone();
    let sub = args.first().map(String::as_str).unwrap_or("");
    match sub {
        "serve" => serve(args, queue),
        "poll" => {
            let loaded = load(&config_path(args)?, queue, false)?;
            let only = opt(args, "--source");
            let force = args.iter().any(|a| a == "--now");
            poll::poll_sources(&*store, &state::State::beside(db), &loaded, only.as_deref(), force, SystemClock.now())
        }
        "connectors" => {
            let loaded = load(&config_path(args)?, queue, false)?;
            let name = || args.get(2).filter(|n| !n.starts_with("--")).cloned().ok_or("usage: sf intake connectors approve|revoke <name> --as <human> --config c.json");
            let who = || approver.clone().filter(|a| !a.trim().is_empty()).ok_or("--as <human> is required and must not be empty");
            match args.get(1).map(String::as_str) {
                Some("list") => connectors::list(&*store, &loaded),
                Some("approve") => connectors::approve(&*store, &loaded, &name()?, &who()?),
                Some("revoke") => connectors::revoke(&*store, &loaded, &name()?, &who()?),
                _ => Err("usage: sf intake connectors list | approve <name> --as <human> | revoke <name> --as <human>".into()),
            }
        }
        "dispatch" => {
            let loaded = load(&config_path(args)?, queue, false)?;
            dispatch(&loaded, engine, num(args, "--max", 10)? as usize)
        }
        "daemon" => daemon(args, store, engine, db),
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
        other => Err(format!("unknown intake subcommand {other:?}: serve | poll | connectors | dispatch | daemon | list | bottlenecks")),
    }
}

const ALL: [ItemStatus; 6] = [
    ItemStatus::Queued,
    ItemStatus::Started,
    ItemStatus::Rejected,
    ItemStatus::RateLimited,
    ItemStatus::Backlog,
    ItemStatus::Done,
];

fn parse_status(s: &str) -> Result<ItemStatus, String> {
    serde_json::from_value(json!(s.trim())).map_err(|_| format!("unknown status {s:?}"))
}

fn serve(args: &[String], queue: Arc<dyn Queue>) -> Result<Value, String> {
    let addr = opt(args, "--addr").ok_or("--addr <host:port> is required")?;
    let loaded = Arc::new(load(&config_path(args)?, queue, true)?);
    let listener = TcpListener::bind(&addr).map_err(|e| format!("bind {addr}: {e}"))?;
    println!(
        "{}",
        json!({"listening": listener.local_addr().map_err(|e| e.to_string())?.to_string()})
    );
    http::serve(listener, loaded)
}

/// Why intake should stop starting work: spend since 00:00 UTC is at or over a daily budget.
fn over_budget(engine: &Engine, b: &Budget, now: u64) -> Result<Option<(u64, u64)>, String> {
    let (tokens, usd) = engine
        .spent_since(now / DAY_MS * DAY_MS)
        .map_err(|e| e.to_string())?;
    let over =
        b.daily_tokens.is_some_and(|m| tokens >= m) || b.daily_usd_micros.is_some_and(|m| usd >= m);
    Ok(over.then_some((tokens, usd)))
}

/// Queued items in priority order become runs. Crossing the daily budget stops dispatch only (GOV).
fn dispatch(l: &Loaded, engine: &Engine, max: usize) -> Result<Value, String> {
    let q = l.intake.queue();
    let (mut started, mut failed, mut paused) = (vec![], vec![], Value::Null);
    for mut item in q
        .list_items(&[ItemStatus::Queued], max)
        .map_err(|e| e.to_string())?
    {
        if let Some((tokens, usd)) = over_budget(engine, &l.budget, SystemClock.now())? {
            paused = json!({"reason": "paused: daily budget", "spent_tokens": tokens, "spent_usd_micros": usd});
            break;
        }
        match start(l, engine, &item) {
            Ok(run_id) => {
                started.push(
                    json!({"item_id": item.item_id, "run_id": run_id, "workflow": item.workflow}),
                );
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

/// The trigger ref is `{source}:{source_ref}`, or `chat:{source}:{source_ref}` for a source with
/// `chat: true`; the engine uses the `chat` prefix for its no-chat-deploy rule.
fn start(l: &Loaded, engine: &Engine, item: &WorkItem) -> Result<String, String> {
    let wf = item.workflow.as_deref().ok_or("item has no workflow")?;
    let input = json!({
        "item_id": item.item_id, "source": item.source, "source_ref": item.source_ref,
        "actor": item.actor, "channel": item.channel, "payload": item.payload,
    });
    let trigger = if l.is_chat(&item.source) {
        format!("chat:{}:{}", item.source, item.source_ref)
    } else {
        format!("{}:{}", item.source, item.source_ref)
    };
    engine
        .start_run_with_input(
            wf,
            &trigger,
            l.budget.run_tokens,
            l.budget.run_usd_micros,
            input,
        )
        .map_err(|e| e.to_string())
}

/// `sf intake daemon --config c.json [--addr host:port] [--tick-s 5]`: one process for the whole loop.
/// Webhooks (if `--addr`) are served on a thread. Each tick polls due sources, dispatches queued items
/// (the daily budget can pause that), then drives runs until nothing progresses. Poll errors are logged
/// to stderr as JSON lines and never stop the loop. `--ticks N` (hidden, for tests) stops after N ticks.
fn daemon(
    args: &[String],
    store: Arc<SqliteStore>,
    engine: &Engine,
    db: &str,
) -> Result<Value, String> {
    let queue: Arc<dyn Queue> = store.clone();
    let addr = opt(args, "--addr");
    let loaded = Arc::new(load(&config_path(args)?, queue, addr.is_some())?);
    let tick_s = num(args, "--tick-s", 5)?.max(1);
    let max_ticks = opt(args, "--ticks")
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| "--ticks needs a number".to_string())
        })
        .transpose()?;
    if let Some(a) = &addr {
        let listener = TcpListener::bind(a).map_err(|e| format!("bind {a}: {e}"))?;
        println!(
            "{}",
            json!({"listening": listener.local_addr().map_err(|e| e.to_string())?.to_string()})
        );
        let l = loaded.clone();
        std::thread::spawn(move || http::serve(listener, l));
    }
    let state = state::State::beside(db);
    let log = |v: Value| eprintln!("{v}");
    let mut n = 0u64;
    loop {
        let now = SystemClock.now();
        let (mut polled, mut dispatched, mut ticked) = (0, 0, 0);
        match poll::poll_sources(&*store, &state, &loaded, None, false, now) {
            Ok(r) => {
                for p in r["polled"].as_array().into_iter().flatten() {
                    match p["status"].as_str() {
                        None => polled += p["items"].as_array().map_or(0, Vec::len),
                        Some("error" | "blocked") => log(json!({"ts": now, "poll_error": p})),
                        _ => {}
                    }
                }
            }
            Err(e) => log(json!({"ts": now, "poll_error": e})),
        }
        match dispatch(&loaded, engine, 100) {
            Ok(r) => dispatched = r["started"].as_array().map_or(0, Vec::len),
            Err(e) => log(json!({"ts": now, "dispatch_error": e})),
        }
        match crate::drive_runs(engine, &*store, false) {
            Ok(ids) => ticked = ids.len(),
            Err(e) => log(json!({"ts": now, "run_error": e})),
        }
        if polled + dispatched + ticked > 0 {
            println!(
                "{}",
                json!({"tick": now, "polled": polled, "dispatched": dispatched, "ticked": ticked})
            );
        }
        n += 1;
        if max_ticks.is_some_and(|m| n >= m) {
            return Ok(json!({"ticks": n}));
        }
        std::thread::sleep(std::time::Duration::from_secs(tick_s));
    }
}

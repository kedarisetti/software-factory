//! `sf`: composition root for the durable core. Prints one JSON document to stdout.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use sf_core::{Error, Limits, RunState, Store, SystemClock, ToolCall, Wake};
use sf_orchestrator::{Engine, Registry, StaticWorkflow, StepDef, StepResult};
use sf_sandbox_local::LocalSandbox;
use sf_store_sqlite::SqliteStore;

fn demo() -> Arc<StaticWorkflow> {
    StaticWorkflow::new(
        "demo",
        1,
        vec![
            StepDef::new("prepare", |ctx| {
                ctx.charge(1200, 3000);
                StepResult::Done(json!({"prepared": true}))
            }),
            StepDef::new("side-effect", |ctx| {
                let call = ToolCall {
                    program: "sh".into(),
                    args: vec!["-c".into(), "echo demo-tool".into()],
                    ..Default::default()
                };
                let out = match ctx.tool(&call, &Limits::default()) {
                    Ok(o) => o,
                    Err(e) => return StepResult::Fail(e.to_string()),
                };
                let file = std::env::var("SF_DEMO_SIDE_EFFECT_FILE").ok();
                // The file stands in for an external system that dedupes on the idempotency key
                // (DUR-3), so a crash between the write and the ledger record cannot duplicate it.
                let r = ctx.effect("append-line", |key| {
                    if let Some(f) = &file {
                        use std::io::Write;
                        let seen = std::fs::read_to_string(f).unwrap_or_default();
                        if seen.lines().any(|l| l.ends_with(key)) {
                            return Ok(json!({"appended": false, "deduped": true}));
                        }
                        let mut h = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(f)
                            .map_err(|e| Error::Storage(e.to_string()))?;
                        writeln!(h, "side-effect {key}").map_err(|e| Error::Storage(e.to_string()))?;
                        h.sync_all().map_err(|e| Error::Storage(e.to_string()))?;
                        if std::env::var("SF_DEMO_CRASH_AFTER_WRITE").as_deref() == Ok("1") {
                            std::process::abort(); // external write done, ledger row not yet recorded
                        }
                    }
                    Ok(json!({"appended": file.is_some()}))
                });
                if let Err(e) = r {
                    return StepResult::Fail(e.to_string());
                }
                if std::env::var("SF_DEMO_CRASH_AFTER_EFFECT").as_deref() == Ok("1") {
                    std::process::abort();
                }
                StepResult::Done(json!({"tool_stdout": out.stdout.trim()}))
            }),
            StepDef::new("slow", |_| {
                let ms = std::env::var("SF_DEMO_SLEEP_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(0u64);
                std::thread::sleep(Duration::from_millis(ms));
                StepResult::Done(json!({"slept_ms": ms}))
            }),
            StepDef::approval("approve"),
        ],
    )
}

fn die(msg: impl std::fmt::Display) -> ! {
    println!("{}", json!({"error": msg.to_string()}));
    std::process::exit(1);
}

fn ex<T, E: std::fmt::Display>(r: Result<T, E>) -> T {
    r.unwrap_or_else(|e| die(e))
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut db: Option<String> = None;
    let mut sandbox: Option<String> = None;
    // Global flags may appear anywhere before the subcommand's positionals; strip them first.
    let mut rest = Vec::new();
    let mut budget_tokens = u64::MAX / 4;
    let mut budget_usd = u64::MAX / 4;
    while !args.is_empty() {
        let a = args.remove(0);
        let val = |args: &mut Vec<String>| {
            if args.is_empty() {
                die(format!("{a} needs a value"));
            }
            args.remove(0)
        };
        match a.as_str() {
            "--db" => db = Some(val(&mut args)),
            "--sandbox" => sandbox = Some(val(&mut args)),
            "--budget-tokens" => budget_tokens = ex(val(&mut args).parse()),
            "--budget-usd-micros" => budget_usd = ex(val(&mut args).parse()),
            _ => rest.push(a),
        }
    }
    let db = db.unwrap_or_else(|| die("--db <path> is required"));
    let cmd = rest.first().cloned().unwrap_or_else(|| die("missing subcommand"));
    let store = Arc::new(ex(SqliteStore::open(&db)));
    let sandbox_dir = sandbox.map(PathBuf::from).unwrap_or_else(|| PathBuf::from(format!("{db}.sandbox")));
    let sb = Arc::new(ex(LocalSandbox::new(sandbox_dir)));
    let registry = Registry::new();
    registry.register(demo());
    let engine = Engine::new(store.clone(), sb, Arc::new(SystemClock), registry);
    let arg = |i: usize| rest.get(i).cloned().unwrap_or_else(|| die("missing argument"));

    let out = match cmd.as_str() {
        "start" => {
            let id = ex(engine.start_run(&arg(1), "cli", budget_tokens, budget_usd));
            json!({"run_id": id})
        }
        "run" => {
            let mut ticked = Vec::new();
            ticked.extend(ex(engine.recover()));
            loop {
                let ids = ex(engine.run_due());
                let progressed = !ids.is_empty();
                ticked.extend(ids);
                let ids = ex(engine.recover());
                let progressed = progressed || !ids.is_empty();
                ticked.extend(ids);
                if progressed {
                    continue;
                }
                let now = sf_core::Clock::now(&SystemClock);
                let runs = ex(store.list_runs(&[RunState::AwaitingApproval, RunState::WaitingOnEvent, RunState::RetryingStep]));
                let soon = runs.iter().any(|r| matches!(r.wake, Some(Wake::At(t)) if t <= now + 10_000));
                if !soon {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            json!({"ticked": ticked})
        }
        "status" => {
            let id = arg(1);
            let run = ex(store.get_run(&id)).unwrap_or_else(|| die("run not found"));
            let steps = ex(store.get_steps(&id));
            json!({
                "run_id": run.run_id,
                "workflow": run.workflow_id,
                "workflow_version": run.workflow_version,
                "state": format!("{:?}", run.state),
                "steps": steps.iter().map(|s| json!({"id": s.step_id, "status": format!("{:?}", s.status), "attempt": s.attempt})).collect::<Vec<_>>(),
                "spent_tokens": run.spent_tokens,
                "spent_usd_micros": run.spent_usd_micros,
                "wake": serde_json::to_value(&run.wake).unwrap_or(Value::Null),
            })
        }
        "approve" => {
            ex(engine.approve(&arg(1), "cli"));
            json!({"approved": arg(1)})
        }
        "deliver" => {
            let payload: Value = ex(serde_json::from_str(&arg(2)));
            ex(store.deliver(&arg(1), &payload));
            json!({"delivered": arg(1)})
        }
        "events" => json!(ex(store.read_run(&arg(1)))),
        other => die(format!("unknown subcommand {other}")),
    };
    println!("{out}");
}

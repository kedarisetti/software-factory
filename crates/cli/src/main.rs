//! `sf`: composition root for the durable core. Prints one JSON document to stdout.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use sf_core::{verify_chain, Action, ActionKind, Error, EventKind, Limits, RunState, Store, SystemClock, ToolCall, Wake};
use sf_orchestrator::{Engine, Policy, Registry, StaticWorkflow, StepDef, StepResult};
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

/// Set by `--unconfined`; every output then carries `"unconfined": true`.
static UNCONFINED: AtomicBool = AtomicBool::new(false);

fn emit(mut v: Value) {
    if UNCONFINED.load(Ordering::SeqCst) {
        match &mut v {
            Value::Object(m) => {
                m.insert("unconfined".into(), json!(true));
            }
            other => v = json!({"unconfined": true, "records": other.take()}),
        }
    }
    println!("{v}");
}

fn die(msg: impl std::fmt::Display) -> ! {
    emit(json!({"error": msg.to_string()}));
    std::process::exit(1);
}

fn ex<T, E: std::fmt::Display>(r: Result<T, E>) -> T {
    r.unwrap_or_else(|e| die(e))
}

/// Appends `"<label> <key>"` to `file` unless a line for `key` is already there. The file stands
/// in for an external system that dedupes on the idempotency key (DUR-3).
fn append_once(file: &str, label: &str, key: &str) -> sf_core::Result<Value> {
    let seen = std::fs::read_to_string(file).unwrap_or_default();
    if seen.lines().any(|l| l.split(' ').next_back() == Some(key)) {
        return Ok(json!({"appended": false, "deduped": true}));
    }
    let io = |e: std::io::Error| Error::Storage(e.to_string());
    let mut h = std::fs::OpenOptions::new().create(true).append(true).open(file).map_err(io)?;
    writeln!(h, "{label} {key}").map_err(io)?;
    h.sync_all().map_err(io)?;
    if std::env::var("SF_SHIP_CRASH_AFTER_WRITE").as_deref() == Ok(label) {
        std::process::abort(); // external write done, ledger row not yet recorded
    }
    Ok(json!({"appended": true}))
}

/// Built-in `ship` v1: one step per gated action kind plus one auto kind. Egress is empty.
fn ship() -> Arc<StaticWorkflow> {
    let kinds = [
        (ActionKind::DraftPr, "org/repo#1"),
        (ActionKind::MergeProtected, "org/repo#1"),
        (ActionKind::Deploy, "prod"),
        (ActionKind::ExternalMessage, "mail:team@example.com"),
        (ActionKind::NewCapability, "tool:linter"),
    ];
    let steps = kinds
        .into_iter()
        .map(|(kind, target)| {
            let name = kind.name();
            StepDef::new(&name.clone(), move |ctx| {
                let action = Action { kind: kind.clone(), target: target.into(), detail: json!({"step": name}) };
                let file = std::env::var("SF_SHIP_EFFECT_FILE").ok();
                let r = ctx.action(&action, |key| match &file {
                    Some(f) => append_once(f, &name, key),
                    None => Ok(json!({"appended": false})),
                });
                match r {
                    Ok(v) => StepResult::Done(v),
                    Err(e) => e.into(),
                }
            })
        })
        .collect();
    StaticWorkflow::new("ship", 1, steps).with_policy(Policy::default().egress(vec![]))
}

fn named(flags: &[(String, String)], name: &str) -> Option<String> {
    flags.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
}

/// The action a parked run waits on and the reason an escalated run stopped, from the event log.
fn run_notes(events: &[sf_core::Event], state: RunState) -> (Value, Value) {
    let state_events = events.iter().rev().filter(|e| e.kind == EventKind::State);
    let pending = match state {
        RunState::AwaitingApproval => state_events
            .clone()
            .find(|e| e.payload["state"] == "parked")
            .map(|e| e.payload["detail"]["action"].clone())
            .filter(|a| !a.is_null())
            .unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let escalation = match state {
        RunState::Escalated => {
            let e = state_events.clone().find(|e| e.payload["state"].as_str().is_some_and(|s| s.starts_with("escalated")));
            e.map_or(Value::Null, |e| match e.payload["detail"]["reason"].as_str() {
                Some(r) => json!(r),
                None => {
                    let err = events.iter().rev().find(|x| x.kind == EventKind::Error).map(|x| x.payload["error"].clone());
                    json!({"state": e.payload["state"], "last_error": err})
                }
            })
        }
        _ => Value::Null,
    };
    (pending, escalation)
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    // Busybox style: this binary is also the in-sandbox egress shim.
    let prog = argv.first().and_then(|a| std::path::Path::new(a).file_name()).and_then(|n| n.to_str()).unwrap_or("");
    if prog == "sf-netshim" {
        std::process::exit(sf_sandbox_local::netshim::main(&argv[1..]));
    }
    if argv.get(1).map(String::as_str) == Some("__netshim") {
        std::process::exit(sf_sandbox_local::netshim::main(&argv[2..]));
    }
    let mut args: Vec<String> = argv.into_iter().skip(1).collect();
    let mut db: Option<String> = None;
    let mut sandbox: Option<String> = None;
    let mut unconfined = false;
    // Global flags may appear anywhere before the subcommand's positionals; strip them first.
    // Per-command flags (`--as`, `--reason`) are collected separately.
    let mut rest = Vec::new();
    let mut flags: Vec<(String, String)> = Vec::new();
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
            "--unconfined" => {
                unconfined = true;
                UNCONFINED.store(true, Ordering::SeqCst);
            }
            "--budget-tokens" => budget_tokens = ex(val(&mut args).parse()),
            "--budget-usd-micros" => budget_usd = ex(val(&mut args).parse()),
            "--as" | "--reason" => flags.push((a.clone(), val(&mut args))),
            _ => rest.push(a),
        }
    }
    let db = db.unwrap_or_else(|| die("--db <path> is required"));
    let cmd = rest.first().cloned().unwrap_or_else(|| die("missing subcommand"));
    let store = Arc::new(ex(SqliteStore::open(&db)));
    let sandbox_dir = sandbox.map(PathBuf::from).unwrap_or_else(|| PathBuf::from(format!("{db}.sandbox")));
    let sb = if unconfined {
        ex(LocalSandbox::unconfined(sandbox_dir))
    } else {
        let exe = ex(std::env::current_exe());
        ex(LocalSandbox::new(sandbox_dir)).with_shim(exe) // this binary serves as the shim
    };
    let registry = Registry::new();
    registry.register(demo());
    registry.register(ship());
    let engine = Engine::new(store.clone(), Arc::new(sb), Arc::new(SystemClock), registry);
    let arg = |i: usize| rest.get(i).cloned().unwrap_or_else(|| die("missing argument"));
    let approver = || {
        named(&flags, "--as").filter(|b| !b.trim().is_empty()).unwrap_or_else(|| die("--as <name> is required and must not be empty"))
    };

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
            let (pending, escalation) = run_notes(&ex(store.read_run(&id)), run.state);
            let mut v = json!({
                "run_id": run.run_id,
                "workflow": run.workflow_id,
                "workflow_version": run.workflow_version,
                "state": format!("{:?}", run.state),
                "steps": steps.iter().map(|s| json!({"id": s.step_id, "status": format!("{:?}", s.status), "attempt": s.attempt})).collect::<Vec<_>>(),
                "spent_tokens": run.spent_tokens,
                "spent_usd_micros": run.spent_usd_micros,
                "wake": serde_json::to_value(&run.wake).unwrap_or(Value::Null),
            });
            if !pending.is_null() {
                v["pending_action"] = pending;
            }
            if !escalation.is_null() {
                v["escalation"] = escalation;
            }
            v
        }
        "approve" => {
            ex(engine.approve(&arg(1), &approver()));
            json!({"approved": arg(1), "by": approver()})
        }
        "deny" => {
            let reason = named(&flags, "--reason").unwrap_or_else(|| die("--reason <text> is required"));
            ex(engine.deny(&arg(1), &approver(), &reason));
            json!({"denied": arg(1), "by": approver()})
        }
        "resume" => {
            ex(engine.resume_escalated(&arg(1), &approver()));
            json!({"resumed": arg(1), "by": approver()})
        }
        "cancel" => {
            ex(engine.cancel(&arg(1)));
            json!({"cancelled": arg(1)})
        }
        "deliver" => {
            let payload: Value = ex(serde_json::from_str(&arg(2)));
            ex(engine.deliver(&arg(1), &payload));
            json!({"delivered": arg(1)})
        }
        "events" => json!(ex(store.read_run(&arg(1)))),
        "audit" => json!(ex(store.read_audit(rest.get(1).map(String::as_str)))),
        "audit-verify" => match verify_chain(&ex(store.read_audit(None))) {
            Ok(()) => json!({"ok": true}),
            Err(seq) => {
                emit(json!({"ok": false, "bad_seq": seq}));
                std::process::exit(1);
            }
        },
        other => die(format!("unknown subcommand {other}")),
    };
    emit(out);
}

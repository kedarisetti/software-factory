//! `sf`: composition root for the durable core. Prints one JSON document to stdout.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

mod intake;
mod sync;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sf_core::{
    verify_chain, Action, ActionKind, Clock, Error, EventKind, Feedback, FeedbackSource, FeedbackTarget, FeedbackVerdict,
    Limits, RunState, Store, SystemClock, ToolCall, Wake,
};
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

fn hex_prefix(parts: &[&[u8]], n: usize) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
        h.update([0]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()[..n].to_string()
}

/// Parse a feedback target (FBK-2): `run:<run>`, `step:<run>/<step>`,
/// `artifact:<path-or-url>[#<span>][@<run>]`, `entry:<id>`, `template:<section>`.
fn parse_target(t: &str) -> Result<FeedbackTarget, String> {
    let bad = || format!("bad target {t:?}: want run:<run> | step:<run>/<step> | artifact:<path>[#span][@run] | entry:<id> | template:<section>");
    let (kind, rest) = t.split_once(':').ok_or_else(bad)?;
    if rest.is_empty() {
        return Err(bad());
    }
    Ok(match kind {
        "run" => FeedbackTarget::Run { run_id: rest.into() },
        "step" => {
            let (r, s) = rest.split_once('/').filter(|(r, s)| !r.is_empty() && !s.is_empty()).ok_or_else(bad)?;
            FeedbackTarget::Step { run_id: r.into(), step_id: s.into() }
        }
        "artifact" => {
            // A run suffix has no '/' or ':', so `https://user@host/x` stays a URL.
            let (rest, run_id) = match rest.rsplit_once('@') {
                Some((a, r)) if !a.is_empty() && !r.is_empty() && !r.contains(['/', ':', '#']) => (a, Some(r.to_string())),
                _ => (rest, None),
            };
            let (artifact, span) = match rest.rsplit_once('#') {
                Some((a, sp)) if !a.is_empty() && !sp.is_empty() && !sp.contains('/') => (a, Some(sp.to_string())),
                _ => (rest, None),
            };
            FeedbackTarget::Artifact { run_id, artifact: artifact.into(), span }
        }
        "entry" => FeedbackTarget::Entry { entry_id: rest.into() },
        "template" => FeedbackTarget::Template { section: rest.into() },
        _ => return Err(bad()),
    })
}

fn parse_verdict(v: &str) -> FeedbackVerdict {
    ex(serde_json::from_value(json!(v)).map_err(|_| format!("unknown verdict {v:?}")))
}

/// Run and step targets must exist; the rest are free-form.
fn validate_target(store: &dyn Store, t: &FeedbackTarget) {
    match t {
        FeedbackTarget::Run { run_id } => {
            if ex(store.get_run(run_id)).is_none() {
                die(format!("run {run_id} not found"));
            }
        }
        FeedbackTarget::Step { run_id, step_id } => {
            if ex(store.get_run(run_id)).is_none() {
                die(format!("run {run_id} not found"));
            }
            if !ex(store.get_steps(run_id)).iter().any(|s| s.step_id == *step_id) {
                die(format!("step {run_id}/{step_id} not found"));
            }
        }
        _ => {}
    }
}

/// Append, reporting whether the id was already recorded (a deduped re-send).
fn record(store: &dyn Store, fb: &Feedback) -> Value {
    let dup = ex(store.read_feedback(0)).iter().any(|f| f.feedback_id == fb.feedback_id);
    let stored = ex(store.append_feedback(fb));
    json!({"feedback": stored, "deduped": dup})
}

fn tail_2k(out: &[u8]) -> String {
    String::from_utf8_lossy(&out[out.len().saturating_sub(2048)..]).into_owned()
}

/// FBK-1 auto source: run a trusted CI tool (no sandbox), record Pass/Fail, exit with its code.
fn check(store: &dyn Store, flags: &[(String, String)], cmd: &[String]) -> ! {
    let name = named(flags, "--name").unwrap_or_else(|| die("--name <check> is required"));
    let tstr = named(flags, "--target").unwrap_or_else(|| die("--target <t> is required"));
    let target = ex(parse_target(&tstr));
    validate_target(store, &target);
    let (prog, args) = cmd.split_first().unwrap_or_else(|| die("missing command after --"));
    let t0 = std::time::Instant::now();
    let o = ex(std::process::Command::new(prog).args(args).stdin(std::process::Stdio::null()).output());
    let duration_ms = t0.elapsed().as_millis() as u64;
    let mut all = o.stdout.clone();
    all.extend_from_slice(&o.stderr);
    let code = o.status.code();
    let exit = code.unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        128 + o.status.signal().unwrap_or(0)
    });
    let fb = Feedback {
        seq: 0,
        feedback_id: format!("{name}:{}:{}", hex_prefix(&[tstr.as_bytes()], 12), hex_prefix(&[&all], 16)),
        target,
        source: FeedbackSource::Auto,
        verdict: if o.status.success() { FeedbackVerdict::Pass } else { FeedbackVerdict::Fail },
        note: None,
        author: name,
        ts: SystemClock.now(),
        detail: json!({"exit_code": code, "duration_ms": duration_ms, "output_tail": tail_2k(&all)}),
    };
    let mut out = record(store, &fb);
    out["exit_code"] = json!(exit);
    emit(out);
    std::process::exit(exit);
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
    // Everything after `--` is a command line (`sf check`) and must not be parsed as flags.
    let passthrough = args.iter().position(|a| a == "--").map(|i| {
        let tail = args.split_off(i);
        tail[1..].to_vec()
    });
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
            "--as" | "--reason" | "--target" | "--verdict" | "--note" | "--id" | "--name" | "--from" | "--kb" | "--golden" => flags.push((a.clone(), val(&mut args))),
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
    registry.register(intake::triage());
    let engine = Engine::new(store.clone(), Arc::new(sb), Arc::new(SystemClock), registry);
    let arg = |i: usize| rest.get(i).cloned().unwrap_or_else(|| die("missing argument"));
    let approver = || {
        named(&flags, "--as").filter(|b| !b.trim().is_empty()).unwrap_or_else(|| die("--as <name> is required and must not be empty"))
    };

    let who = |what: &str| named(&flags, "--as").filter(|b| !b.trim().is_empty()).unwrap_or_else(|| die(format!("--as <{what}> is required and must not be empty")));
    let new_fb = |target: FeedbackTarget, source, verdict, author: String| {
        validate_target(&*store, &target);
        let note = named(&flags, "--note");
        let ts = SystemClock.now();
        let default_id = || {
            let t = serde_json::to_string(&target).unwrap_or_default();
            let v = format!("{verdict:?}");
            format!("fb-{}", hex_prefix(&[t.as_bytes(), v.as_bytes(), author.as_bytes(), note.as_deref().unwrap_or("").as_bytes(), ts.to_string().as_bytes()], 24))
        };
        let feedback_id = named(&flags, "--id").unwrap_or_else(default_id);
        Feedback { seq: 0, feedback_id, target, source, verdict, note, author, ts, detail: Value::Null }
    };

    let out = match cmd.as_str() {
        "intake" => ex(intake::run(&rest[1..], store.clone(), &engine)),
        "feedback" => match arg(1).as_str() {
            "add" => {
                let target = ex(parse_target(&named(&flags, "--target").unwrap_or_else(|| die("--target <t> is required"))));
                let verdict = parse_verdict(&named(&flags, "--verdict").unwrap_or_else(|| die("--verdict <v> is required")));
                record(&*store, &new_fb(target, FeedbackSource::Human, verdict, who("name")))
            }
            "list" => {
                let from = named(&flags, "--from").map_or(0, |f| ex(f.parse::<u64>()));
                let want = named(&flags, "--target").map(|t| ex(parse_target(&t)));
                let all = ex(store.read_feedback(from));
                json!(all.into_iter().filter(|f| want.as_ref().is_none_or(|w| f.target == *w)).collect::<Vec<_>>())
            }
            "sync" => {
                let kb_dir = named(&flags, "--kb").unwrap_or_else(|| die("--kb <kb dir> is required"));
                let golden = named(&flags, "--golden").unwrap_or_else(|| "golden".into());
                let kb = ex(sync::open_kb(std::path::Path::new(&kb_dir)));
                ex(sync::sync(&*store, &kb, std::path::Path::new(&golden)))
            }
            "status" => {
                let (v, over) = ex(sync::status(&*store));
                if over {
                    emit(v);
                    std::process::exit(2); // FBK-3: oldest pending feedback is past the one-day bound
                }
                v
            }
            other => die(format!("unknown feedback subcommand {other}")),
        },
        "check" => check(&*store, &flags, passthrough.as_deref().unwrap_or(&[])),
        "outcome" => {
            let verdict = parse_verdict(&arg(2));
            if !matches!(verdict, FeedbackVerdict::Survived | FeedbackVerdict::Reverted | FeedbackVerdict::Incident) {
                die("outcome must be survived, reverted or incident");
            }
            let target = FeedbackTarget::Run { run_id: arg(1) };
            record(&*store, &new_fb(target, FeedbackSource::Outcome, verdict, who("name-or-monitor")))
        }
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

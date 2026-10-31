use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;

use super::*;
use crate::memory::MemoryStore;

struct FakeClock(AtomicU64);
impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct FakeSandbox(AtomicUsize);
impl Sandbox for FakeSandbox {
    fn run(&self, call: &ToolCall, _: &Limits, kill: &KillSwitch) -> Result<ToolOutput> {
        if kill.is_tripped() {
            return Err(Error::Killed);
        }
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput { exit_code: Some(0), stdout: call.program.clone(), stderr: String::new(), duration_ms: 1, truncated: false })
    }
}

struct Env {
    store: Arc<MemoryStore>,
    sandbox: Arc<FakeSandbox>,
    clock: Arc<FakeClock>,
    reg: Registry,
}

impl Env {
    fn new() -> Self {
        Env { store: Arc::new(MemoryStore::new()), sandbox: Arc::default(), clock: Arc::new(FakeClock(AtomicU64::new(1_000))), reg: Registry::new() }
    }
    fn engine(&self) -> Engine {
        Engine::new(self.store.clone(), self.sandbox.clone(), self.clock.clone(), self.reg.clone())
    }
    fn advance(&self, ms: u64) {
        self.clock.0.fetch_add(ms, Ordering::SeqCst);
    }
    fn wf(&self, version: u32, steps: Vec<StepDef>) {
        self.reg.register(StaticWorkflow::new("wf", version, steps));
    }
    fn state(&self, id: &str) -> RunState {
        self.store.get_run(id).unwrap().unwrap().state
    }
    fn assert_seq_contiguous(&self, id: &str) {
        let seqs: Vec<u64> = self.store.read_run(id).unwrap().iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());
    }
}

fn counter() -> Arc<AtomicUsize> {
    Arc::default()
}
fn bump(c: &Arc<AtomicUsize>) -> usize {
    c.fetch_add(1, Ordering::SeqCst) + 1
}
fn n(c: &Arc<AtomicUsize>) -> usize {
    c.load(Ordering::SeqCst)
}

#[test]
fn happy_path() {
    let env = Env::new();
    env.wf(1, vec![
        StepDef::new("a", |_| StepResult::Done(json!(1))),
        StepDef::new("b", |c| StepResult::Done(json!(c.outputs["a"].as_i64().unwrap() + 1))),
    ]);
    let e = env.engine();
    let id = e.start_run("wf", "t-1", 100, 100).unwrap();
    assert_eq!(env.state(&id), RunState::Queued);
    assert_eq!(e.tick(&id).unwrap(), RunState::Succeeded);
    let steps = env.store.get_steps(&id).unwrap();
    assert_eq!(steps[1].output, Some(json!(2)));
    assert!(steps.iter().all(|s| s.status == StepStatus::Completed));
    env.assert_seq_contiguous(&id);
}

#[test]
fn crash_after_step_one_does_not_rerun_it() {
    let env = Env::new();
    let (runs1, effects, runs2) = (counter(), counter(), counter());
    let killer: Arc<Mutex<Option<KillSwitch>>> = Arc::default();
    let (r1, ef, k) = (runs1.clone(), effects.clone(), killer.clone());
    let r2 = runs2.clone();
    env.wf(1, vec![
        StepDef::new("one", move |c| {
            bump(&r1);
            let ef = ef.clone();
            c.effect("send", move |_| Ok(json!(bump(&ef)))).unwrap();
            k.lock().unwrap().as_ref().unwrap().trip(); // "crash" right after the checkpoint
            StepResult::Done(json!("one"))
        }),
        StepDef::new("two", move |_| {
            bump(&r2);
            StepResult::Done(json!("two"))
        }),
    ]);
    let e1 = env.engine();
    *killer.lock().unwrap() = Some(e1.kill_switch());
    let id = e1.start_run("wf", "t", 0, 0).unwrap();
    assert_eq!(e1.tick(&id).unwrap(), RunState::Running);
    assert_eq!(n(&runs2), 0);
    drop(e1);

    let e2 = env.engine();
    assert_eq!(e2.recover().unwrap(), vec![id.clone()]);
    assert_eq!(env.state(&id), RunState::Succeeded);
    assert_eq!((n(&runs1), n(&effects), n(&runs2)), (1, 1, 1));
    env.assert_seq_contiguous(&id);
}

#[test]
fn effect_not_rerun_when_step_fails_after_it() {
    let env = Env::new();
    let (effects, last) = (counter(), Arc::new(Mutex::new(Value::Null)));
    let (ef, l) = (effects.clone(), last.clone());
    env.wf(1, vec![StepDef::new("s", move |c| {
        let ef = ef.clone();
        let v = c.effect("charge", move |_| Ok(json!(40 + bump(&ef)))).unwrap();
        *l.lock().unwrap() = v;
        if c.attempt == 1 { StepResult::Fail("flaky".into()) } else { StepResult::Done(json!(null)) }
    }).retries(3, 100, 1000)]);
    let e = env.engine();
    let id = e.start_run("wf", "t", 0, 0).unwrap();
    assert_eq!(e.tick(&id).unwrap(), RunState::RetryingStep);
    env.advance(100);
    assert_eq!(e.tick(&id).unwrap(), RunState::Succeeded);
    assert_eq!(n(&effects), 1);
    assert_eq!(*last.lock().unwrap(), json!(41)); // recorded value returned on retry
}

#[test]
fn tool_goes_through_ledger() {
    let env = Env::new();
    env.wf(1, vec![StepDef::new("s", |c| {
        let out = c.tool(&ToolCall { program: "ls".into(), ..Default::default() }, &Limits::default()).unwrap();
        assert_eq!(out.stdout, "ls");
        if c.attempt == 1 { StepResult::Fail("x".into()) } else { StepResult::Done(json!(null)) }
    })]);
    let e = env.engine();
    let id = e.start_run("wf", "t", 0, 0).unwrap();
    e.tick(&id).unwrap();
    env.advance(5_000);
    assert_eq!(e.tick(&id).unwrap(), RunState::Succeeded);
    assert_eq!(env.sandbox.0.load(Ordering::SeqCst), 1);
    let kinds: Vec<_> = env.store.read_run(&id).unwrap().iter().map(|e| e.kind).collect();
    assert_eq!(kinds.iter().filter(|k| **k == EventKind::ToolCall).count(), 1);
}

#[test]
fn retry_with_backoff_then_escalate() {
    let env = Env::new();
    let (first, bad) = (counter(), counter());
    let (f, b) = (first.clone(), bad.clone());
    env.wf(1, vec![
        StepDef::new("ok", move |_| { bump(&f); StepResult::Done(json!(1)) }),
        StepDef::new("bad", move |_| { bump(&b); StepResult::Fail("nope".into()) }).retries(3, 100, 250),
    ]);
    let e = env.engine();
    let id = e.start_run("wf", "t", 0, 0).unwrap();
    let t0 = env.clock.now();
    assert_eq!(e.tick(&id).unwrap(), RunState::RetryingStep);
    assert_eq!(env.store.get_run(&id).unwrap().unwrap().wake, Some(Wake::At(t0 + 100)));
    assert_eq!(e.tick(&id).unwrap(), RunState::RetryingStep); // not due: no rerun
    assert_eq!(n(&bad), 1);
    env.advance(100);
    assert_eq!(e.run_due().unwrap(), vec![id.clone()]);
    assert_eq!(env.store.get_run(&id).unwrap().unwrap().wake, Some(Wake::At(t0 + 100 + 200)));
    env.advance(200);
    assert_eq!(e.tick(&id).unwrap(), RunState::Escalated);
    assert_eq!((n(&first), n(&bad)), (1, 3));
    assert_eq!(e.tick(&id).unwrap(), RunState::Escalated);
    e.resume_escalated(&id).unwrap();
    assert_eq!(e.tick(&id).unwrap(), RunState::RetryingStep);
    assert_eq!((n(&first), n(&bad)), (1, 4));
    env.assert_seq_contiguous(&id);
}

#[test]
fn park_on_event_and_resume_with_payload() {
    let env = Env::new();
    env.wf(1, vec![StepDef::new("ci", |c| match c.payload.clone() {
        Some(p) => StepResult::Done(p),
        None => StepResult::Park(Wake::Event("ci:1".into())),
    })]);
    let e = env.engine();
    let id = e.start_run("wf", "t", 0, 0).unwrap();
    assert_eq!(e.tick(&id).unwrap(), RunState::WaitingOnEvent);
    assert!(e.run_due().unwrap().is_empty());
    // A fresh engine (process restart) sees the durable delivery.
    env.store.deliver("ci:1", &json!({"green": true})).unwrap();
    assert_eq!(env.engine().run_due().unwrap(), vec![id.clone()]);
    assert_eq!(env.state(&id), RunState::Succeeded);
    assert_eq!(env.store.get_steps(&id).unwrap()[0].output, Some(json!({"green": true})));
}

#[test]
fn park_on_timer() {
    let env = Env::new();
    let deadline = env.clock.now() + 500;
    env.wf(1, vec![StepDef::new("sleep", move |c| match c.payload {
        Some(_) => StepResult::Done(json!("woke")),
        None => StepResult::Park(Wake::At(deadline)),
    })]);
    let e = env.engine();
    let id = e.start_run("wf", "t", 0, 0).unwrap();
    assert_eq!(e.tick(&id).unwrap(), RunState::WaitingOnEvent);
    assert!(e.run_due().unwrap().is_empty());
    env.advance(500);
    assert_eq!(e.run_due().unwrap(), vec![id.clone()]);
    assert_eq!(env.state(&id), RunState::Succeeded);
}

#[test]
fn version_pinning() {
    let env = Env::new();
    let (v1, v2) = (counter(), counter());
    let (a, b) = (v1.clone(), v2.clone());
    env.wf(1, vec![StepDef::new("s", move |_| { bump(&a); StepResult::Park(Wake::Event("go".into())) })]);
    let e = env.engine();
    let old = e.start_run("wf", "t", 0, 0).unwrap();
    e.tick(&old).unwrap();
    env.wf(2, vec![StepDef::new("s", move |_| { bump(&b); StepResult::Done(json!(2)) })]);
    let new = e.start_run("wf", "t", 0, 0).unwrap();
    assert_eq!(env.store.get_run(&new).unwrap().unwrap().workflow_version, 2);
    assert_eq!(env.store.get_run(&old).unwrap().unwrap().workflow_version, 1);
    env.store.deliver("go", &json!(null)).unwrap();
    e.run_due().unwrap();
    e.tick(&new).unwrap();
    assert_eq!((n(&v1), n(&v2)), (2, 1)); // old run re-ran its own v1 step, not v2
    assert_eq!(env.state(&new), RunState::Succeeded);
}

#[test]
fn missing_version_escalates() {
    let env = Env::new();
    env.wf(1, vec![]);
    let e = env.engine();
    let id = e.start_run("wf", "t", 0, 0).unwrap();
    let other = Engine::new(env.store.clone(), env.sandbox.clone(), env.clock.clone(), Registry::new());
    assert_eq!(other.tick(&id).unwrap(), RunState::Escalated);
}

#[test]
fn budget_exceeded_parks_until_approved() {
    let env = Env::new();
    let last = counter();
    let l = last.clone();
    env.wf(1, vec![
        StepDef::new("spend", |c| { c.charge(150, 0); StepResult::Done(json!(1)) }),
        StepDef::new("more", move |c| { c.charge(10, 0); bump(&l); StepResult::Done(json!(2)) }),
    ]);
    let e = env.engine();
    let id = e.start_run("wf", "t", 100, 1_000).unwrap();
    assert_eq!(e.tick(&id).unwrap(), RunState::AwaitingApproval);
    assert_eq!(n(&last), 0);
    let run = env.store.get_run(&id).unwrap().unwrap();
    assert_eq!(run.spent_tokens, 150);
    assert_eq!(run.wake, Some(Wake::Event(format!("budget:{id}"))));
    assert!(e.run_due().unwrap().is_empty());
    e.approve(&id).unwrap();
    assert_eq!(e.run_due().unwrap(), vec![id.clone()]);
    assert_eq!(env.state(&id), RunState::Succeeded); // approved once: not re-parked by step two
    assert_eq!(n(&last), 1);
    env.assert_seq_contiguous(&id);
}

#[test]
fn approval_step() {
    let env = Env::new();
    env.wf(1, vec![StepDef::approval("gate")]);
    let e = env.engine();
    let id = e.start_run("wf", "t", 0, 0).unwrap();
    assert_eq!(e.tick(&id).unwrap(), RunState::AwaitingApproval);
    assert!(e.approve("nope").is_err());
    e.approve(&id).unwrap();
    assert!(e.approve(&id).is_ok()); // idempotent delivery keeps the first payload
    e.run_due().unwrap();
    assert_eq!(env.store.get_steps(&id).unwrap()[0].output, Some(json!({"approved": true})));
    assert_eq!(env.state(&id), RunState::Succeeded);
}

#[test]
fn kill_switch_stops_before_next_step_and_stays_resumable() {
    let env = Env::new();
    let second = counter();
    let s = second.clone();
    env.wf(1, vec![
        StepDef::new("a", |_| StepResult::Done(json!(1))),
        StepDef::new("b", move |_| { bump(&s); StepResult::Done(json!(2)) }),
    ]);
    let e = env.engine();
    let id = e.start_run("wf", "t", 0, 0).unwrap();
    e.kill_switch().trip();
    assert_eq!(e.tick(&id).unwrap(), RunState::Queued);
    assert_eq!(n(&second), 0);
    assert_eq!(env.engine().tick(&id).unwrap(), RunState::Succeeded); // fresh engine, untripped
}

#[test]
fn seq_continues_after_reload() {
    let env = Env::new();
    env.wf(1, vec![StepDef::new("a", |_| StepResult::Done(json!(1))), StepDef::approval("gate")]);
    let id = env.engine().start_run("wf", "t", 0, 0).unwrap();
    env.engine().tick(&id).unwrap();
    let before = env.store.read_run(&id).unwrap().len();
    env.engine().approve(&id).unwrap();
    assert_eq!(env.engine().run_due().unwrap(), vec![id.clone()]);
    assert!(env.store.read_run(&id).unwrap().len() > before + 1);
    env.assert_seq_contiguous(&id);
    assert_eq!(env.state(&id), RunState::Succeeded);
}

#[test]
fn cancel_is_terminal() {
    let env = Env::new();
    env.wf(1, vec![StepDef::approval("gate")]);
    let e = env.engine();
    let id = e.start_run("wf", "t", 0, 0).unwrap();
    e.tick(&id).unwrap();
    e.cancel(&id).unwrap();
    assert_eq!(e.tick(&id).unwrap(), RunState::Cancelled);
    assert!(e.cancel(&id).is_err());
}

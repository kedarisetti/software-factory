//! Durable run engine (DUR-1..DUR-6, SBX-4 budget and kill switch). See docs/spec.md.
//! Depends only on sf-core; talks to storage, the sandbox and time through its traits.

pub mod memory;

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde_json::{json, Value};
use sf_core::{
    effect_key, idempotency_key, Clock, Commit, Error, Event, EventKind, KillSwitch, Limits, Result, Run,
    RunId, RunState, Sandbox, Step, StepStatus, Store, Timestamp, ToolCall, ToolOutput, Wake,
};

/// Outcome of one step attempt.
pub enum StepResult {
    /// Checkpoint this output; the step never runs again (DUR-1).
    Done(Value),
    /// Park the run (DUR-5). Wake keys starting `approval:` or `budget:` park as `AwaitingApproval`,
    /// everything else as `WaitingOnEvent`. On resume the step re-runs with `ctx.payload` set
    /// (the delivered payload, or `Null` after a timer).
    Park(Wake),
    /// Attempt failed; retried alone with backoff (DUR-4).
    Fail(String),
}

type StepFn = dyn Fn(&mut StepCtx) -> StepResult + Send + Sync;

/// One step of a workflow, with its retry policy.
#[derive(Clone)]
pub struct StepDef {
    pub id: String,
    pub max_attempts: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    pub run: Arc<StepFn>,
}

impl StepDef {
    /// Defaults: 3 attempts, 1s base backoff, 60s cap.
    pub fn new(id: &str, run: impl Fn(&mut StepCtx) -> StepResult + Send + Sync + 'static) -> Self {
        StepDef { id: id.into(), max_attempts: 3, backoff_base_ms: 1_000, backoff_max_ms: 60_000, run: Arc::new(run) }
    }

    pub fn retries(mut self, max_attempts: u32, base_ms: u64, max_ms: u64) -> Self {
        (self.max_attempts, self.backoff_base_ms, self.backoff_max_ms) = (max_attempts, base_ms, max_ms);
        self
    }

    /// Parks `AwaitingApproval` on `approval:{run}:{step}`, completes with the delivered payload.
    pub fn approval(id: &str) -> Self {
        StepDef::new(id, |ctx| match ctx.payload.clone() {
            Some(p) => StepResult::Done(p),
            None => StepResult::Park(Wake::Event(format!("approval:{}:{}", ctx.run_id, ctx.step_id))),
        })
    }
}

/// A versioned workflow definition (DUR-6).
pub trait Workflow: Send + Sync {
    fn id(&self) -> &str;
    fn version(&self) -> u32;
    fn steps(&self) -> Vec<StepDef>;
}

/// A workflow built from a fixed step list.
pub struct StaticWorkflow {
    id: String,
    version: u32,
    steps: Vec<StepDef>,
}

impl StaticWorkflow {
    pub fn new(id: &str, version: u32, steps: Vec<StepDef>) -> Arc<Self> {
        Arc::new(StaticWorkflow { id: id.into(), version, steps })
    }
}

impl Workflow for StaticWorkflow {
    fn id(&self) -> &str {
        &self.id
    }
    fn version(&self) -> u32 {
        self.version
    }
    fn steps(&self) -> Vec<StepDef> {
        self.steps.clone()
    }
}

/// Workflows keyed by (id, version). Clones share state, so a version registered later is visible
/// to an engine already running.
type Workflows = HashMap<(String, u32), Arc<dyn Workflow>>;

#[derive(Clone, Default)]
pub struct Registry(Arc<RwLock<Workflows>>);

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&self, wf: Arc<dyn Workflow>) {
        self.0.write().unwrap().insert((wf.id().to_string(), wf.version()), wf);
    }
    pub fn get(&self, id: &str, version: u32) -> Option<Arc<dyn Workflow>> {
        self.0.read().unwrap().get(&(id.to_string(), version)).cloned()
    }
    pub fn latest(&self, id: &str) -> Option<Arc<dyn Workflow>> {
        let map = self.0.read().unwrap();
        map.iter().filter(|((i, _), _)| i == id).max_by_key(|((_, v), _)| *v).map(|(_, w)| w.clone())
    }
}

/// Handed to a step attempt: inputs, the idempotency ledger, the sandbox and budget accounting.
pub struct StepCtx {
    pub run_id: RunId,
    pub step_id: String,
    /// 1-based attempt number.
    pub attempt: u32,
    /// Outputs of completed earlier steps, by step id.
    pub outputs: HashMap<String, Value>,
    /// `Some` when resuming from a park (delivered payload, or `Null` after a timer).
    pub payload: Option<Value>,
    store: Arc<dyn Store>,
    sandbox: Arc<dyn Sandbox>,
    kill: KillSwitch,
    step_key: String,
    n: u32,
    events: Vec<(EventKind, Value)>,
    spent_tokens: u64,
    spent_usd: u64,
    budget_tokens: u64,
    budget_usd: u64,
    over_budget: bool,
}

impl StepCtx {
    /// Append an event to this step's checkpoint commit (DUR-2).
    pub fn event(&mut self, kind: EventKind, payload: Value) {
        self.events.push((kind, payload));
    }
    pub fn model_response(&mut self, payload: Value) {
        self.event(EventKind::ModelResponse, payload);
    }
    pub fn tool_call(&mut self, payload: Value) {
        self.event(EventKind::ToolCall, payload);
    }
    pub fn tool_result(&mut self, payload: Value) {
        self.event(EventKind::ToolResult, payload);
    }

    /// Side effect guarded by the idempotency ledger (DUR-3). Key = `effect_key(idempotency_key(run, step), n)`
    /// with `n` counting effects in this step, so a retry maps each call to the same ledger row and
    /// gets the recorded result instead of running `f` again.
    ///
    /// `f` receives the key and must pass it to the external system (an `Idempotency-Key` header,
    /// a dedupe column). The ledger alone is at-least-once: a crash after `f` succeeds but before
    /// the ledger row commits re-runs `f`, and only the receiver's dedupe makes that a no-op.
    pub fn effect(&mut self, name: &str, f: impl FnOnce(&str) -> Result<Value>) -> Result<Value> {
        let (v, fresh) = self.guarded(f)?;
        if fresh {
            self.tool_result(json!({"effect": name, "result": v}));
        }
        Ok(v)
    }

    /// Run a sandboxed tool call through the ledger, honouring the engine's kill switch (SBX-4).
    pub fn tool(&mut self, call: &ToolCall, limits: &Limits) -> Result<ToolOutput> {
        let (sandbox, kill, c, l) = (self.sandbox.clone(), self.kill.clone(), call.clone(), *limits);
        let (v, fresh) = self.guarded(move |_key| {
            let out = sandbox.run(&c, &l, &kill)?;
            serde_json::to_value(out).map_err(|e| Error::Invalid(e.to_string()))
        })?;
        if fresh {
            self.tool_call(json!({"program": call.program, "args": call.args}));
            self.tool_result(v.clone());
        }
        serde_json::from_value(v).map_err(|e| Error::Invalid(e.to_string()))
    }

    /// Add to the run's spend. Returns true once over budget; the engine then parks the run for
    /// approval after this step's checkpoint.
    pub fn charge(&mut self, tokens: u64, usd_micros: u64) -> bool {
        self.spent_tokens += tokens;
        self.spent_usd += usd_micros;
        self.over_budget |= self.spent_tokens > self.budget_tokens || self.spent_usd > self.budget_usd;
        self.over_budget
    }

    fn guarded(&mut self, f: impl FnOnce(&str) -> Result<Value>) -> Result<(Value, bool)> {
        let key = effect_key(&self.step_key, self.n);
        self.n += 1;
        if let Some(v) = self.store.effect_result(&key)? {
            return Ok((v, false));
        }
        let v = f(&key)?;
        match self.store.record_effect(&key, &v) {
            Ok(()) => Ok((v, true)),
            Err(Error::Conflict(_)) => Ok((self.store.effect_result(&key)?.unwrap_or(v), false)),
            Err(e) => Err(e),
        }
    }
}

/// The durable run engine. Holds no per-run state in memory: everything is reloaded from the store.
pub struct Engine {
    store: Arc<dyn Store>,
    sandbox: Arc<dyn Sandbox>,
    clock: Arc<dyn Clock>,
    registry: Registry,
    kill: KillSwitch,
    counter: AtomicU64,
}

impl Engine {
    pub fn new(store: Arc<dyn Store>, sandbox: Arc<dyn Sandbox>, clock: Arc<dyn Clock>, registry: Registry) -> Self {
        Engine { store, sandbox, clock, registry, kill: KillSwitch::new(), counter: AtomicU64::new(0) }
    }

    pub fn kill_switch(&self) -> KillSwitch {
        self.kill.clone()
    }

    /// Create a run pinned to the latest registered version of `workflow_id` (DUR-6).
    pub fn start_run(&self, workflow_id: &str, trigger_ref: &str, budget_tokens: u64, budget_usd_micros: u64) -> Result<RunId> {
        let wf = self.registry.latest(workflow_id).ok_or_else(|| Error::NotFound(format!("workflow {workflow_id}")))?;
        let now = self.clock.now();
        loop {
            let run = Run {
                run_id: format!("run-{now}-{}-{}", std::process::id(), self.counter.fetch_add(1, Ordering::SeqCst)),
                workflow_id: workflow_id.into(),
                workflow_version: wf.version(),
                trigger_ref: trigger_ref.into(),
                state: RunState::Queued,
                budget_tokens,
                budget_usd_micros,
                spent_tokens: 0,
                spent_usd_micros: 0,
                created_at: now,
                updated_at: now,
                wake: None,
            };
            match self.store.create_run(&run) {
                Ok(()) => {
                    let ev = state_event("", "created", json!({"version": run.workflow_version}));
                    self.commit(&run, None, &mut 0, vec![ev])?;
                    return Ok(run.run_id);
                }
                Err(Error::Conflict(_)) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Advance a run until it parks, escalates, succeeds, waits for backoff, or the kill switch trips.
    /// A parked or retrying run that is not yet due is returned unchanged.
    pub fn tick(&self, run_id: &str) -> Result<RunState> {
        let mut run = self.load(run_id)?;
        if run.state.is_terminal() || run.state == RunState::Escalated {
            return Ok(run.state);
        }
        if run.state != RunState::Queued && run.state != RunState::Running && !self.is_due(&run)? {
            return Ok(run.state);
        }
        let mut seq = self.next_seq(run_id)?;
        let Some(wf) = self.registry.get(&run.workflow_id, run.workflow_version) else {
            let msg = format!("workflow {} v{} not registered", run.workflow_id, run.workflow_version);
            run.state = RunState::Escalated;
            run.wake = None;
            let evs = vec![(String::new(), EventKind::Error, json!({"error": msg})), state_event("", "escalated", json!({}))];
            self.commit(&run, None, &mut seq, evs)?;
            return Ok(run.state);
        };
        let defs = wf.steps();
        let mut steps: HashMap<String, Step> =
            self.store.get_steps(run_id)?.into_iter().map(|s| (s.step_id.clone(), s)).collect();
        let mut wake = run.wake.take();
        loop {
            if self.kill.is_tripped() {
                return Ok(run.state); // stays resumable (SBX-4)
            }
            let Some(def) = defs.iter().find(|d| steps.get(&d.id).is_none_or(|s| s.status != StepStatus::Completed)) else {
                run.state = RunState::Succeeded;
                run.wake = None;
                self.commit(&run, None, &mut seq, vec![state_event("", "succeeded", json!({}))])?;
                return Ok(run.state);
            };
            let prev = steps.get(&def.id);
            let attempt = match prev {
                Some(s) if s.status == StepStatus::Failed => s.attempt + 1,
                Some(s) => s.attempt.max(1),
                None => 1,
            };
            let payload = match (wake.take(), prev.map(|s| s.status)) {
                (Some(Wake::Event(k)), Some(StepStatus::Parked)) => Some(self.store.delivery(&k)?.unwrap_or(Value::Null)),
                (Some(Wake::At(_)), Some(StepStatus::Parked)) => Some(Value::Null),
                _ => None,
            };
            let step_key = idempotency_key(run_id, &def.id);
            let mut ctx = StepCtx {
                run_id: run_id.into(),
                step_id: def.id.clone(),
                attempt,
                outputs: steps.values().filter_map(|s| Some((s.step_id.clone(), s.output.clone()?))).collect(),
                payload,
                store: self.store.clone(),
                sandbox: self.sandbox.clone(),
                kill: self.kill.clone(),
                step_key: step_key.clone(),
                n: 0,
                events: vec![],
                spent_tokens: run.spent_tokens,
                spent_usd: run.spent_usd_micros,
                budget_tokens: run.budget_tokens,
                budget_usd: run.budget_usd_micros,
                over_budget: false,
            };
            let res = catch_unwind(AssertUnwindSafe(|| (def.run)(&mut ctx))).unwrap_or_else(|_| StepResult::Fail("step panicked".into()));
            if self.kill.is_tripped() && !matches!(res, StepResult::Done(_)) {
                return Ok(run.state); // killed mid-step: nothing committed, step re-runs on resume
            }
            run.spent_tokens = ctx.spent_tokens;
            run.spent_usd_micros = ctx.spent_usd;
            run.wake = None;
            let mut evs: Vec<_> = ctx.events.drain(..).map(|(k, p)| (def.id.clone(), k, p)).collect();
            let mut step = Step {
                step_id: def.id.clone(),
                run_id: run_id.into(),
                agent_id: None,
                model_id: None,
                attempt,
                status: StepStatus::Completed,
                idempotency_key: step_key,
                output: None,
                error: None,
            };
            let mut done = false;
            match res {
                StepResult::Done(v) => {
                    step.output = Some(v);
                    run.state = RunState::Running;
                    evs.push(state_event(&def.id, "step completed", json!({"attempt": attempt})));
                    let key = format!("budget:{run_id}");
                    if ctx.over_budget && self.store.delivery(&key)?.is_none() {
                        run.state = RunState::AwaitingApproval;
                        run.wake = Some(Wake::Event(key));
                        evs.push(state_event(&def.id, "parked: over budget", json!({})));
                        done = true;
                    }
                }
                StepResult::Park(w) => {
                    step.status = StepStatus::Parked;
                    run.state = match &w {
                        Wake::Event(k) if k.starts_with("approval:") || k.starts_with("budget:") => RunState::AwaitingApproval,
                        _ => RunState::WaitingOnEvent,
                    };
                    evs.push(state_event(&def.id, "parked", json!({"wake": w})));
                    run.wake = Some(w);
                    done = true;
                }
                StepResult::Fail(e) => {
                    step.status = StepStatus::Failed;
                    step.error = Some(e.clone());
                    evs.push((def.id.clone(), EventKind::Error, json!({"error": e, "attempt": attempt})));
                    if attempt >= def.max_attempts {
                        run.state = RunState::Escalated;
                        evs.push(state_event(&def.id, "escalated: retries exhausted", json!({})));
                    } else {
                        let at = self.clock.now().saturating_add(backoff(def, attempt));
                        run.state = RunState::RetryingStep;
                        run.wake = Some(Wake::At(at));
                        evs.push(state_event(&def.id, "retrying", json!({"next_attempt": attempt + 1, "at": at})));
                    }
                    done = true;
                }
            }
            self.commit(&run, Some(step.clone()), &mut seq, evs)?; // the checkpoint (DUR-1)
            if done {
                return Ok(run.state);
            }
            steps.insert(step.step_id.clone(), step);
        }
    }

    /// On process start: tick every Queued/Running/RetryingStep run that is due (DUR-1).
    pub fn recover(&self) -> Result<Vec<RunId>> {
        self.tick_due(&[RunState::Queued, RunState::Running, RunState::RetryingStep])
    }

    /// Wake runs whose timer passed or whose awaited event was delivered (DUR-5).
    pub fn run_due(&self) -> Result<Vec<RunId>> {
        self.tick_due(&[RunState::AwaitingApproval, RunState::WaitingOnEvent, RunState::RetryingStep])
    }

    /// Deliver the approval the run is awaiting. The next `run_due`/`tick` resumes it.
    pub fn approve(&self, run_id: &str) -> Result<()> {
        let run = self.load(run_id)?;
        let Some(Wake::Event(key)) = run.wake.clone().filter(|_| run.state == RunState::AwaitingApproval) else {
            return Err(Error::Invalid(format!("run {run_id} is not awaiting approval")));
        };
        self.store.deliver(&key, &json!({"approved": true}))?;
        let ev = (String::new(), EventKind::Approval, json!({"key": key}));
        self.commit(&run, None, &mut self.next_seq(run_id)?, vec![ev])
    }

    /// Human resumes an escalated run; the failed step gets a fresh retry budget.
    pub fn resume_escalated(&self, run_id: &str) -> Result<()> {
        let mut run = self.load(run_id)?;
        if run.state != RunState::Escalated {
            return Err(Error::Invalid(format!("run {run_id} is not escalated")));
        }
        let failed = self.store.get_steps(run_id)?.into_iter().find(|s| s.status == StepStatus::Failed);
        let failed = failed.map(|s| Step { attempt: 0, ..s });
        (run.state, run.wake) = (RunState::Running, None);
        self.commit(&run, failed, &mut self.next_seq(run_id)?, vec![state_event("", "resumed by human", json!({}))])
    }

    pub fn cancel(&self, run_id: &str) -> Result<()> {
        let mut run = self.load(run_id)?;
        if run.state.is_terminal() {
            return Err(Error::Invalid(format!("run {run_id} already finished")));
        }
        (run.state, run.wake) = (RunState::Cancelled, None);
        self.commit(&run, None, &mut self.next_seq(run_id)?, vec![state_event("", "cancelled", json!({}))])
    }

    fn tick_due(&self, states: &[RunState]) -> Result<Vec<RunId>> {
        let mut ticked = vec![];
        for run in self.store.list_runs(states)? {
            if self.is_due(&run)? {
                self.tick(&run.run_id)?;
                ticked.push(run.run_id);
            }
        }
        Ok(ticked)
    }

    fn is_due(&self, run: &Run) -> Result<bool> {
        Ok(match &run.wake {
            None => true,
            Some(Wake::At(t)) => self.clock.now() >= *t,
            Some(Wake::Event(k)) => self.store.delivery(k)?.is_some(),
        })
    }

    fn load(&self, run_id: &str) -> Result<Run> {
        self.store.get_run(run_id)?.ok_or_else(|| Error::NotFound(format!("run {run_id}")))
    }

    /// Last seq + 1, read from the log, so a reloaded engine never collides (DUR-2).
    fn next_seq(&self, run_id: &str) -> Result<u64> {
        Ok(self.store.read_run(run_id)?.last().map_or(0, |e| e.seq + 1))
    }

    /// One atomic commit: run row, step row, events. Assigns seqs from `seq`.
    fn commit(&self, run: &Run, step: Option<Step>, seq: &mut u64, evs: Vec<(String, EventKind, Value)>) -> Result<()> {
        let ts: Timestamp = self.clock.now();
        let mut run = run.clone();
        run.updated_at = ts;
        let events = evs
            .into_iter()
            .map(|(step_id, kind, payload)| {
                *seq += 1;
                Event { run_id: run.run_id.clone(), step_id, seq: *seq - 1, kind, payload, ts }
            })
            .collect();
        self.store.commit(&Commit { run, step, events })
    }
}

fn state_event(step_id: &str, what: &str, extra: Value) -> (String, EventKind, Value) {
    (step_id.into(), EventKind::State, json!({"state": what, "detail": extra}))
}

/// `min(base * 2^(attempt-1), max)` (DUR-4).
fn backoff(def: &StepDef, attempt: u32) -> u64 {
    let mult = 1u64.checked_shl(attempt - 1).unwrap_or(u64::MAX);
    def.backoff_base_ms.saturating_mul(mult).min(def.backoff_max_ms)
}

#[cfg(test)]
mod tests;

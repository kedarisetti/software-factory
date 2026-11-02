//! Durable run engine (DUR-1..DUR-6, SBX-4 budget and kill switch). See docs/spec.md.
//! Depends only on sf-core; talks to storage, the sandbox and time through its traits.

mod audit;
pub mod credentials;
mod exec;
pub mod memory;
pub mod policy;

pub use policy::{Gate, Policy};

/// Trigger ref prefixes that are chat. A run triggered from chat can never deploy (GOV, SBX-3).
/// Intake dispatches every chat source (`chat: true` in its config) as `chat:<source>:<ref>`.
pub const CHAT_SOURCES: [&str; 1] = ["chat"];

fn from_chat(trigger_ref: &str) -> bool {
    CHAT_SOURCES.iter().any(|s| trigger_ref.strip_prefix(s).is_some_and(|r| r.starts_with(':')))
}

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde_json::{json, Value};
use sf_core::{
    effect_key, idempotency_key, Action, ActionKind, Clock, Commit, Credentials, Decision, Error, Event, EventKind, KillSwitch,
    Limits, Result, Run, RunId, RunState, Sandbox, Step, StepStatus, Store, Timestamp, ToolCall, ToolOutput, Wake,
};
use sf_core::untrusted::{scan, Untrusted};

use crate::audit::Auditor;
use crate::exec::{workspace_name, ToolRun};

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
    /// Escalate to a human at once, no retry (denied action, injection, policy breach).
    Escalate(String),
}

impl From<Error> for StepResult {
    fn from(e: Error) -> Self {
        StepResult::Fail(e.to_string())
    }
}

type StepFn = dyn Fn(&mut StepCtx) -> StepResult + Send + Sync;

/// One step of a workflow, with its retry policy.
#[derive(Clone)]
pub struct StepDef {
    pub id: String,
    pub max_attempts: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    /// Agent that performs the step; recorded on the step row and on audit records.
    pub agent: Option<String>,
    pub run: Arc<StepFn>,
}

impl StepDef {
    /// Defaults: 3 attempts, 1s base backoff, 60s cap.
    pub fn new(id: &str, run: impl Fn(&mut StepCtx) -> StepResult + Send + Sync + 'static) -> Self {
        StepDef { id: id.into(), max_attempts: 3, backoff_base_ms: 1_000, backoff_max_ms: 60_000, agent: None, run: Arc::new(run) }
    }

    pub fn retries(mut self, max_attempts: u32, base_ms: u64, max_ms: u64) -> Self {
        (self.max_attempts, self.backoff_base_ms, self.backoff_max_ms) = (max_attempts, base_ms, max_ms);
        self
    }

    pub fn agent(mut self, agent: &str) -> Self {
        self.agent = Some(agent.into());
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
    /// Egress, credential scopes and gate overrides for runs of this workflow (SBX-1, SBX-2).
    fn policy(&self) -> Policy {
        Policy::default()
    }
}

/// A workflow built from a fixed step list.
pub struct StaticWorkflow {
    id: String,
    version: u32,
    steps: Vec<StepDef>,
    policy: Policy,
}

impl StaticWorkflow {
    pub fn new(id: &str, version: u32, steps: Vec<StepDef>) -> Arc<Self> {
        Arc::new(StaticWorkflow { id: id.into(), version, steps, policy: Policy::default() })
    }

    pub fn with_policy(self: Arc<Self>, policy: Policy) -> Arc<Self> {
        Arc::new(StaticWorkflow { id: self.id.clone(), version: self.version, steps: self.steps.clone(), policy })
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
    fn policy(&self) -> Policy {
        self.policy.clone()
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
    trigger_ref: String,
    input: Option<Value>,
    store: Arc<dyn Store>,
    sandbox: Arc<dyn Sandbox>,
    kill: KillSwitch,
    policy: Policy,
    clock: Arc<dyn Clock>,
    creds: Option<Arc<dyn Credentials>>,
    audit: Auditor,
    park: Option<Park>,
    escalation: Option<Escalation>,
    step_key: String,
    n: u32,
    events: Vec<(EventKind, Value)>,
    spent_tokens: u64,
    spent_usd: u64,
    budget_tokens: u64,
    budget_usd: u64,
    over_budget: bool,
}

/// A gated action waiting for a human: the approval key and what is to be approved.
struct Park {
    key: String,
    action: Action,
}

/// Why the step must escalate at once, and the waiver keys a human can deliver to let it pass.
struct Escalation {
    reason: String,
    waives: Vec<String>,
}

impl StepCtx {
    /// The input the run was started with (`Engine::start_run_with_input`), if any (TRG-1).
    pub fn input(&self) -> Option<Value> {
        self.input.clone()
    }

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
    /// The engine sets workspace and egress from the run and policy (SBX-1), leases `call.secrets`
    /// within the policy's scopes (SBX-2), audits the call and redacts leased values from the result.
    pub fn tool(&mut self, call: &ToolCall, limits: &Limits) -> Result<ToolOutput> {
        let mut c = call.clone();
        c.workspace = workspace_name(&self.run_id);
        c.egress = self.policy.egress.clone();
        let run = ToolRun {
            sandbox: self.sandbox.clone(),
            kill: self.kill.clone(),
            creds: self.creds.clone(),
            clock: self.clock.clone(),
            audit: self.audit.clone(),
            scopes: self.policy.scopes.clone(),
            ttl_ms: self.policy.lease_ttl_ms,
            blocked: Arc::default(),
        };
        let blocked = run.blocked.clone();
        let l = *limits;
        let res = self.guarded(move |key| run.run(key, c, &l));
        // A refused secret (scope not granted, lease expired) is a policy breach: escalate, never retry.
        if let Some(reason) = blocked.lock().unwrap_or_else(|e| e.into_inner()).take() {
            self.escalation.get_or_insert(Escalation { reason, waives: vec![] });
        }
        let (v, fresh) = res?;
        if fresh {
            self.tool_call(json!({"program": call.program, "args": call.args}));
            self.tool_result(v.clone());
        }
        serde_json::from_value(v).map_err(|e| Error::Invalid(e.to_string()))
    }

    /// Side effect behind the approval gate (GOV). Shares the effect counter with `effect`/`tool`.
    /// Auto kinds run at once. Human kinds park until `Engine::approve` delivers a digest that
    /// matches this exact action; `f` then runs once through the ledger. A changed action needs a
    /// new approval. A denial escalates the run.
    pub fn action(&mut self, action: &Action, f: impl FnOnce(&str) -> Result<Value>) -> Result<Value> {
        let key = effect_key(&self.step_key, self.n);
        let digest = action.digest();
        let (name, detail) = (action.kind.name(), action.detail.clone());
        if action.kind == ActionKind::Deploy && from_chat(&self.trigger_ref) {
            return self.chat_deploy_blocked(&key, action, &digest);
        }
        if self.store.effect_result(&key)?.is_some() {
            return self.run_action(action, f);
        }
        if self.policy.gate(&action.kind) == Gate::Auto {
            self.audit.record(format!("{key}:auto"), &name, &action.target, &digest, Decision::Auto, None, detail)?;
            return self.run_action(action, f);
        }
        let akey = format!("approval:{}:{}:{}:{}", self.run_id, self.step_id, self.n, &digest[..16]);
        let Some(d) = self.store.delivery(&akey)? else {
            self.n += 1;
            self.park = Some(Park { key: akey.clone(), action: action.clone() });
            return Err(Error::AwaitingApproval(akey));
        };
        let by = d.get("by").and_then(Value::as_str).unwrap_or("").to_string();
        match d.get("approved").and_then(Value::as_bool) {
            Some(true) => {
                if d.get("digest").and_then(Value::as_str) != Some(digest.as_str()) {
                    return Err(Error::Forbidden("approval does not match this action".into()));
                }
                let by = Some(by.as_str()).filter(|b| !b.is_empty());
                self.audit.record(format!("{key}:approved"), &name, &action.target, &digest, Decision::Approved, by, detail)?;
                self.run_action(action, f)
            }
            Some(false) => {
                let reason = d.get("reason").and_then(Value::as_str).unwrap_or("denied").to_string();
                let detail = json!({"reason": reason, "action": detail});
                self.audit.record(format!("{key}:denied"), &name, &action.target, &digest, Decision::Denied, Some(&by), detail)?;
                self.escalation = Some(Escalation { reason: format!("denied: {reason}"), waives: vec![] });
                Err(Error::Denied(reason))
            }
            None => Err(Error::Forbidden("malformed approval delivery".into())),
        }
    }

    /// No chat message can start a deploy: audit, escalate, and no approval can override it.
    fn chat_deploy_blocked(&mut self, key: &str, action: &Action, digest: &str) -> Result<Value> {
        let reason = format!("deploy from chat trigger {}", self.trigger_ref);
        let detail = json!({"reason": reason, "action": action.detail});
        self.audit.record(format!("{key}:blocked"), &action.kind.name(), &action.target, digest, Decision::Blocked, None, detail)?;
        self.escalation = Some(Escalation { reason: reason.clone(), waives: vec![] });
        Err(Error::Forbidden(reason))
    }

    fn run_action(&mut self, action: &Action, f: impl FnOnce(&str) -> Result<Value>) -> Result<Value> {
        let (v, fresh) = self.guarded(f)?;
        if fresh {
            self.tool_result(json!({"action": action.kind.name(), "target": action.target, "result": v}));
        }
        Ok(v)
    }

    /// Wrap content from tickets, chat, email or the web (SBX-3). If the scan finds instruction-like
    /// text and no human has waived this exact content, audit it, escalate the run and fail.
    pub fn untrusted(&mut self, source: &str, content: &str) -> Result<Untrusted> {
        let u = Untrusted::new(source, content);
        if !self.policy.scan_untrusted {
            return Ok(u);
        }
        let findings = scan(&u.content);
        let digest = u.digest();
        let waive = format!("waive:{}:{digest}", self.run_id);
        if findings.is_empty() || self.store.delivery(&waive)?.is_some() {
            return Ok(u);
        }
        let rules = findings.iter().map(|f| f.rule.as_str()).collect::<Vec<_>>().join(", ");
        let detail = json!({"findings": findings, "waive": waive});
        let key = format!("{}:injection:{digest}", self.step_key);
        self.audit.record(key, "injection", source, &digest, Decision::Blocked, None, detail)?;
        let reason = format!("possible injection in {source}: {rules}");
        match &mut self.escalation {
            Some(e) => e.waives.push(waive),
            None => self.escalation = Some(Escalation { reason, waives: vec![waive] }),
        }
        Err(Error::Injection(rules))
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
    creds: Option<Arc<dyn Credentials>>,
}

impl Engine {
    pub fn new(store: Arc<dyn Store>, sandbox: Arc<dyn Sandbox>, clock: Arc<dyn Clock>, registry: Registry) -> Self {
        Engine { store, sandbox, clock, registry, kill: KillSwitch::new(), counter: AtomicU64::new(0), creds: None }
    }

    /// Credential issuer for `ToolCall::secrets` (SBX-2).
    pub fn with_credentials(mut self, creds: Arc<dyn Credentials>) -> Self {
        self.creds = Some(creds);
        self
    }

    pub fn kill_switch(&self) -> KillSwitch {
        self.kill.clone()
    }

    /// Create a run pinned to the latest registered version of `workflow_id` (DUR-6).
    pub fn start_run(&self, workflow_id: &str, trigger_ref: &str, budget_tokens: u64, budget_usd_micros: u64) -> Result<RunId> {
        self.start(workflow_id, trigger_ref, budget_tokens, budget_usd_micros, None)
    }

    /// Like `start_run`, with an input (the work item payload) stored durably before the run can
    /// tick; steps read it with `StepCtx::input` (TRG-1).
    pub fn start_run_with_input(
        &self,
        workflow_id: &str,
        trigger_ref: &str,
        budget_tokens: u64,
        budget_usd_micros: u64,
        input: Value,
    ) -> Result<RunId> {
        self.start(workflow_id, trigger_ref, budget_tokens, budget_usd_micros, Some(input))
    }

    fn start(&self, workflow_id: &str, trigger_ref: &str, budget_tokens: u64, budget_usd_micros: u64, input: Option<Value>) -> Result<RunId> {
        let wf = self.registry.latest(workflow_id).ok_or_else(|| Error::NotFound(format!("workflow {workflow_id}")))?;
        let now = self.clock.now();
        loop {
            let run_id = format!("run-{now}-{}-{}", std::process::id(), self.counter.fetch_add(1, Ordering::SeqCst));
            if let Some(v) = &input {
                self.store.deliver(&format!("input:{run_id}"), v)?; // durable before the run exists
            }
            let run = Run {
                run_id,
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
        let (defs, policy) = (wf.steps(), wf.policy());
        let mut steps: HashMap<String, Step> =
            self.store.get_steps(run_id)?.into_iter().map(|s| (s.step_id.clone(), s)).collect();
        let mut wake = run.wake.take();
        if self.budget_denied(&wake)? {
            run.state = RunState::Escalated;
            let evs = vec![state_event("", "escalated: budget denied", json!({}))];
            self.commit(&run, None, &mut seq, evs)?;
            return Ok(run.state);
        }
        let input = self.store.delivery(&format!("input:{run_id}"))?;
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
                trigger_ref: run.trigger_ref.clone(),
                input: input.clone(),
                store: self.store.clone(),
                sandbox: self.sandbox.clone(),
                kill: self.kill.clone(),
                policy: policy.clone(),
                clock: self.clock.clone(),
                creds: self.creds.clone(),
                audit: self.auditor(run_id, &def.id, def.agent.clone()),
                park: None,
                escalation: None,
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
                agent_id: def.agent.clone(),
                model_id: None,
                attempt,
                status: StepStatus::Completed,
                idempotency_key: step_key,
                output: None,
                error: None,
            };
            let mut done = false;
            let (park, esc) = (ctx.park.take(), ctx.escalation.take());
            let res = match res {
                StepResult::Fail(e) if park.is_none() && esc.is_some() => StepResult::Escalate(e),
                r => r,
            };
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
                StepResult::Fail(_) if park.is_some() => {
                    let p = park.expect("checked");
                    let a = &p.action;
                    let action = json!({"kind": a.kind, "target": a.target, "detail": a.detail, "digest": a.digest()});
                    step.status = StepStatus::Parked;
                    run.state = RunState::AwaitingApproval;
                    let w = Wake::Event(p.key);
                    evs.push(state_event(&def.id, "parked", json!({"wake": w, "action": action})));
                    run.wake = Some(w);
                    done = true;
                }
                StepResult::Escalate(e) => {
                    step.status = StepStatus::Failed;
                    step.error = Some(e.clone());
                    evs.push((def.id.clone(), EventKind::Error, json!({"error": e, "attempt": attempt})));
                    let (reason, waive) = esc.map_or((e, vec![]), |x| (x.reason, x.waives));
                    run.state = RunState::Escalated;
                    evs.push(state_event(&def.id, "escalated", json!({"reason": reason, "waive": waive})));
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

    /// A named human approves what the run is parked on. For an action park the delivery carries
    /// the digest of the parked action, so the approval covers exactly that action. Audited.
    pub fn approve(&self, run_id: &str, approver: &str) -> Result<()> {
        self.answer(run_id, approver, true, "")
    }

    /// A named human denies what the run is parked on. The parked step re-runs and gets `Denied`;
    /// a denied budget park escalates the run.
    pub fn deny(&self, run_id: &str, approver: &str, reason: &str) -> Result<()> {
        self.answer(run_id, approver, false, reason)
    }

    /// Deliver an external event (CI result, reply). Approvals, budget answers and waivers need a
    /// named human through `approve`, `deny` or `resume_escalated`, and run inputs are written only
    /// at start, so those keys are refused.
    pub fn deliver(&self, key: &str, payload: &Value) -> Result<()> {
        if ["approval:", "budget:", "waive:", "input:"].iter().any(|p| key.starts_with(p)) {
            return Err(Error::Forbidden(format!("{key} can only be answered by a named human")));
        }
        self.store.deliver(key, payload)
    }

    /// Tokens and USD micros spent by runs created at or after `ts`, in any state. The intake
    /// daily budget check: crossing it pauses intake, never running work.
    pub fn spent_since(&self, ts: Timestamp) -> Result<(u64, u64)> {
        let all = [
            RunState::Queued, RunState::Running, RunState::AwaitingApproval, RunState::WaitingOnEvent,
            RunState::RetryingStep, RunState::Escalated, RunState::Succeeded, RunState::Cancelled,
        ];
        let runs = self.store.list_runs(&all)?;
        Ok(runs.iter().filter(|r| r.created_at >= ts).fold((0, 0), |(t, u), r| (t + r.spent_tokens, u + r.spent_usd_micros)))
    }

    /// Human resumes an escalated run; the failed step gets a fresh retry budget. Injection
    /// waivers recorded since the last resume are delivered and audited under `by`.
    pub fn resume_escalated(&self, run_id: &str, by: &str) -> Result<()> {
        let mut run = self.load(run_id)?;
        if run.state != RunState::Escalated {
            return Err(Error::Invalid(format!("run {run_id} is not escalated")));
        }
        let by = named(by)?;
        let steps = self.store.get_steps(run_id)?;
        for (step_id, key) in self.pending_waivers(run_id)? {
            let agent = steps.iter().find(|s| s.step_id == step_id).and_then(|s| s.agent_id.clone());
            self.store.deliver(&key, &json!({"by": by}))?;
            let detail = json!({"waived": key});
            self.auditor(run_id, &step_id, agent).record(format!("{key}:waiver"), "waiver", &key, "", Decision::Approved, Some(by), detail)?;
        }
        let failed = steps.into_iter().find(|s| s.status == StepStatus::Failed);
        let failed = failed.map(|s| Step { attempt: 0, ..s });
        (run.state, run.wake) = (RunState::Running, None);
        let ev = state_event("", "resumed by human", json!({"by": by}));
        self.commit(&run, failed, &mut self.next_seq(run_id)?, vec![ev])
    }

    pub fn cancel(&self, run_id: &str) -> Result<()> {
        let mut run = self.load(run_id)?;
        if run.state.is_terminal() {
            return Err(Error::Invalid(format!("run {run_id} already finished")));
        }
        (run.state, run.wake) = (RunState::Cancelled, None);
        self.commit(&run, None, &mut self.next_seq(run_id)?, vec![state_event("", "cancelled", json!({}))])
    }

    fn answer(&self, run_id: &str, approver: &str, approved: bool, reason: &str) -> Result<()> {
        let by = named(approver)?;
        let run = self.load(run_id)?;
        let Some(Wake::Event(key)) = run.wake.clone().filter(|_| run.state == RunState::AwaitingApproval) else {
            return Err(Error::Invalid(format!("run {run_id} is not awaiting approval")));
        };
        let (step_id, digest) = self.parked_action(run_id)?;
        let mut payload = json!({"approved": approved, "by": by});
        if let Some(d) = &digest {
            payload["digest"] = json!(d);
        }
        if !approved {
            payload["reason"] = json!(reason);
        }
        self.store.deliver(&key, &payload)?;
        let agent = self.store.get_steps(run_id)?.into_iter().find(|s| s.step_id == step_id).and_then(|s| s.agent_id);
        let (suffix, decision) = if approved { ("approve", Decision::Approved) } else { ("deny", Decision::Denied) };
        let detail = json!({"reason": reason});
        self.auditor(run_id, &step_id, agent).record(
            format!("{key}:{suffix}"), "approval", &key, digest.as_deref().unwrap_or(""), decision, Some(by), detail,
        )?;
        let ev = (step_id, EventKind::Approval, json!({"key": key, "approved": approved, "by": by}));
        self.commit(&run, None, &mut self.next_seq(run_id)?, vec![ev])
    }

    /// Step id and action digest of the latest park (budget and plain approval parks have no digest).
    fn parked_action(&self, run_id: &str) -> Result<(String, Option<String>)> {
        let log = self.store.read_run(run_id)?;
        let parked = log.iter().rev().find(|e| e.kind == EventKind::State && e.payload["state"] == "parked");
        Ok(parked.map_or((String::new(), None), |e| {
            (e.step_id.clone(), e.payload["detail"]["action"]["digest"].as_str().map(String::from))
        }))
    }

    /// `(step, waive key)` for every injection escalation since the last human resume.
    fn pending_waivers(&self, run_id: &str) -> Result<Vec<(String, String)>> {
        let mut out = vec![];
        for e in self.store.read_run(run_id)?.iter().rev().filter(|e| e.kind == EventKind::State) {
            match e.payload["state"].as_str() {
                Some("resumed by human") => break,
                Some("escalated") => {
                    let keys = e.payload["detail"]["waive"].as_array().into_iter().flatten().filter_map(Value::as_str);
                    out.extend(keys.map(|k| (e.step_id.clone(), k.to_string())));
                }
                _ => {}
            }
        }
        Ok(out)
    }

    fn budget_denied(&self, wake: &Option<Wake>) -> Result<bool> {
        let Some(Wake::Event(k)) = wake else { return Ok(false) };
        Ok(k.starts_with("budget:") && self.store.delivery(k)?.is_some_and(|d| d["approved"] == false))
    }

    fn auditor(&self, run_id: &str, step_id: &str, agent_id: Option<String>) -> Auditor {
        Auditor { store: self.store.clone(), clock: self.clock.clone(), run_id: run_id.into(), step_id: step_id.into(), agent_id }
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

fn named(by: &str) -> Result<&str> {
    match by.trim() {
        "" => Err(Error::Invalid("approver must be named".into())),
        b => Ok(b),
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

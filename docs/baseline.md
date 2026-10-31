# Cost baseline (Milestone 2)

Recorded from `sf status` for the built-in `demo` workflow v1 (see `crates/cli/tests/durable.rs`, `cost_baseline_is_reported`):

| Metric | Value |
|---|---|
| spent_tokens | 1200 |
| spent_usd_micros | 3000 |
| steps | 4 (prepare, side-effect, slow, approve) |

These numbers are constants charged by the `prepare` step. They prove the plumbing: charges are checkpointed with the step, survive a crash, and are reported by `status`. They are **not** a real baseline. The real baseline needs a real workflow that calls a model; until then, no cost claim should cite these figures.

## Side effects: ledger plus receiver dedupe

The side effect and its ledger record are two writes. A kill between them re-runs the effect on resume, so the ledger alone is at-least-once. `StepCtx::effect` therefore hands the effect its idempotency key, and the external system must dedupe on it (an `Idempotency-Key` header, a unique column). The demo's file target does this. Both windows are tested:

- `crash_between_effect_and_checkpoint_does_not_duplicate`: the ledger row exists, so the effect is skipped.
- `crash_between_external_write_and_ledger_does_not_duplicate`: no ledger row, the effect re-runs, and the receiver drops it.
- `sigkill_mid_run_resumes_without_duplicate`: SIGKILL as soon as the write lands.

A real connector (GitHub, Slack) must pass the key through, or document that its action is naturally idempotent.


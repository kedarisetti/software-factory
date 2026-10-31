# Verification loop (M4: FBK-1 to FBK-3, ENT-1)

Feedback from three sources lands in one store, a daily sync turns it into knowledge-base updates and eval-set
candidates, a human promotes candidates into golden cases, and CI blocks any change that makes the golden set worse.

```
 human (sf feedback add) --\
 auto  (sf check)         ---> feedback table (sqlite) --daily--> sf feedback sync --> kb (counts, expiry, LRN-4)
 outcome (sf outcome)    --/    (append-only, FBK-2)                              \--> golden/candidates/*.json
                                                                                          | human sets expected outcome
                                                                                          v
                                          CI: sf-eval gate  <--  golden/cases + baseline.json  <-- promote
                                          (blocks a regression; seeded patches prove it can fail)
```

## Run each piece

All `sf` commands take `--db <path>`. Output is one JSON document on stdout.

| Step | Command |
|---|---|
| Human feedback (FBK-1) | `sf feedback add --target <t> --verdict <v> --as <name> [--note ..] [--id ..]` |
| Auto feedback | `sf check --name <check> --target <t> -- <cmd...>` (records pass/fail, exits with the command's code) |
| Outcome feedback | `sf outcome <run> survived\|reverted\|incident --as outcome-monitor` |
| List | `sf feedback list [--target <t>] [--from <seq>]` |
| Daily sync (FBK-3) | `sf --db sf.db feedback sync --kb kb --golden golden` |
| Lag check | `sf --db sf.db feedback status` (exit 2 when the oldest pending record is over 24 h) |
| Golden set | `target/debug/sf-eval run`, `sf-eval gate`, `sf-eval baseline` (see `golden/README.md`) |
| Gate self-test | `scripts/eval-gate-selftest.sh` (gate passes clean, fails on every seeded regression) |

Targets: `run:<run>`, `step:<run>/<step>`, `artifact:<path-or-url>[#span][@run]`, `entry:<kb id>`, `template:<section>`.

### Cron

```
17 3 * * *  sf --db /var/sf/sf.db feedback sync --kb /var/sf/kb --golden /srv/software-factory/golden >> /var/log/sf-sync.log 2>&1
*/30 * * * * sf --db /var/sf/sf.db feedback status || echo "sf feedback sync is late (exit $?)" | mail -s "sf feedback lag" oncall
```

The sync is the only writer of the `feedback-sync` cursor; run one at a time. Candidates are written into the checkout the
sync is pointed at, so a human commits them (or the cron job runs against a working copy that opens a PR).

## What the sync does

Starts at the `feedback-sync` cursor (the next seq to process). Each record is applied completely, then the cursor moves
past it. A crash re-applies at most one record, and every application tolerates that:

* **Entry targets** with helped/misled/wrong/stale call the kb `feedback` op as caller `factory`, so tallies and expiry follow
  the M1 rules. The note carries `sync:<feedback_id>`; before applying, the sync looks for that marker in `<kb>/.kb/calls.jsonl`,
  so a re-applied record does not double-count. An entry that no longer exists is counted as `entry_missing` and skipped.
* **Outcome reverted/incident on a run**: each Active learning whose `source_runs` contains that run and whose recorded outcome is
  not already failed/reverted is replayed through `learn` as a Reverted (reverted) or Failed (incident) learning from that run.
  That is the LRN-4 mechanism: the old learning is expired, and the new one carries the negative outcome and a `contradicts` link.
* **Candidates** for negative feedback (`verdict.is_negative()`) from human or outcome sources on a run, step or artifact:
  `golden/candidates/<feedback_id>.json`; template targets go to `golden/candidates/template/`. Auto failures produce a
  candidate only when the record says it is a golden-set failure: `detail.golden_set == true`, or the output tail contains the
  gate's `gate: FAIL` line. Other auto failures and all non-negative verdicts produce nothing (counted as `ignored`).
* Prints `{processed, applied: {entry_feedback, entry_missing, entry_already_applied, learnings_expired, candidates,
  template_candidates, ignored}, cursor_before, cursor_after}`.

### Candidate format (version 1)

```json
{
  "version": 1,
  "feedback": { "...the full feedback record..." },
  "run": { "run_id": "..", "workflow_id": "ship", "workflow_version": 1, "state": "Succeeded" },
  "steps": [ { "step_id": "..", "status": "..", "attempt": 1, "output": {}, "error": null } ],
  "event_log_excerpt": [ { "seq": 0, "step_id": "..", "kind": "State", "ts": 0, "payload": {} } ],
  "expected": { "TODO": "a human sets the expected outcome, then moves this into golden/cases/" }
}
```

`run`, `steps` and `event_log_excerpt` are null when the target has no run (a URL artifact, a template section). For a step
target, `steps` and the excerpt are limited to that step; otherwise all steps and the last 50 events. Rewriting a candidate is
atomic and byte-identical for the same record.

### Promoting a candidate

1. Read the candidate and decide what the right behaviour was (what the retrieval should have returned, what the scanner
   should have flagged, which gate should have parked the run).
2. Add the case to the matching suite under `golden/cases/` (see "Adding cases" in `golden/README.md`), re-baseline if the
   ladder changed (`sf-eval baseline`, review the diff, commit), and delete the candidate.
3. Check that the new case fails without the fix and passes with it.

## The CI gate

CI runs `sf-eval gate` on every push and pull request (`eval-gate`) and replays each `golden/seeded-regressions/*.patch`
to prove the gate fails (`eval-gate-selftest`). A metric worse than baseline by more than its tolerance exits 1.

## Honest limits

* No model-in-the-loop cases yet. The golden set is deterministic: retrieval over a fixed store, the injection scanner and the
  workflow gates. It cannot catch a regression in what an agent does with the results.
* Retrieval ladder sizes are 1x, 10x and 30x the 88-entry fixture (up to 2,640 entries), with synthetic distractors. That
  says little about a real knowledge base of tens of thousands of entries.
* Candidates need a human to set the expected outcome. Nothing becomes a case on its own, so the eval set grows only as fast
  as people triage; the one-day bound (FBK-3) covers the candidate appearing, not it being promoted.
* `tokens` and `tool_calls` metrics are trivial until an agent runs the questions (zero-agent runs report one search per question).
* The learning expiry replays a finding through `learn`. If a learning merged several runs and one of them reverted, the whole
  entry is expired (LRN-4 has no per-run split), and the replayed learning is credited to the reverted run only. The kb has no
  op to set an existing entry's outcome, so the "reverted" mark lives on the new linked entry.
* Entry-tally dedupe relies on `<kb>/.kb/calls.jsonl`, which is a log that may be deleted. If it is, a crash window of one
  record could double-count a helped/misled tally. Wrong/stale expiry and candidates are idempotent without it.
* The kb call log is written after the op, so a crash between the two can still double-count one tally. The window is a
  single record per crash.
* A record the kb rejects with an I/O error stops the sync (the cursor stays), so `status` goes over the limit and alerts;
  it does not skip silently.

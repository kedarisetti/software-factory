# Intake (TRG-1 to TRG-4)

Connectors turn Slack messages and GitHub issues into work items. `Intake::accept` (crate `sf-intake`)
decides what happens to each one; `sf intake ...` (crate `sf-cli`) is the server, poller and dispatcher.

## Flow

```
Slack events ---> sf intake serve ---+
GitHub webhook -> (verify, normalise) |--> Intake::accept --> work queue (SQLite, separate from the KB)
sf intake poll github ---------------+      permission -> near-dup -> rate limit -> priority
                                                                  |
sf intake dispatch  <-- Queued items, highest priority first <----+
   |  daily budget check (stop = "paused: daily budget")
   v
Engine::start_run_with_input(workflow, "{source}:{source_ref}", run budgets, item payload + item_id)
```

Every outcome is stored, none dropped: `queued`, `rejected` (permission table, with reason),
`rate_limited`, `backlog` (TRG-4), then `started` and `done` once dispatched. Exact duplicates
(Slack event id incl. `X-Slack-Retry-Num` retries, GitHub delivery or issue+action) are one item.
Near-duplicates (same actor, channel and normalised words within `near_dup_window_ms`) add no item.

## Commands

All take the global `--db <path>`; `--config` is `docs/intake.example.json` shaped.

- `sf intake serve --addr 127.0.0.1:8787 --config c.json` runs `POST /slack/events`, `POST /github/webhook`, `GET /healthz`.
- `sf intake poll github --repo org/repo [--label factory] [--from-file issues.json] --config c.json` reads open issues
  (`gh api`, one read-only GET, first 100) and keeps the cursor `github-poll:{repo}` (max `updated_at`, epoch ms).
  Issues need a `factory:<workflow>` label, like the webhook; others and pull requests are counted as `skipped`.
- `sf intake dispatch [--max N] --config c.json` starts Queued items in priority order. Before each start it sums spend of runs
  created since 00:00 UTC (`Engine::spent_since`); at or over `budget.daily_tokens` or `daily_usd_micros` it stops and reports
  `paused: daily budget`. Running work is untouched. Spend accrues as runs tick, so a burst can start several runs before
  the budget shows; per-run budgets bound each of them. Items whose workflow is not registered become `rejected`.
- `sf intake list [--status queued,rejected]` and `sf intake bottlenecks [--window-days 7 --min 3]` (TRG-4: clusters of
  similar items become one `backlog` item linking its evidence; re-running in the same window changes nothing).

Built-in workflows a trigger can name: `demo`, `triage` (reads the item and wraps it with `ctx.untrusted`), `ship` (deploy).

## Config

- `slack`: `signing_secret_env`, `bot_user_id`, `channel_defaults` (channel id -> workflow when the message names none).
  Message form: `@bot <workflow> <text>`.
- `github`: `webhook_secret_env`. Workflow comes from the `factory:<workflow>` label.
- `permissions`: `rules` (source, channel or `*`, optional `actors`, exact `workflows`) and `deploy_workflows`. Default deny.
  The table fails to load if a chat source allows a deploy workflow. `ship` contains a Deploy action, so it is listed as a
  deploy workflow and only GitHub (the ticket system, with its own review trail) may start it.
- `rate`: `window_ms`, `per_source`, `per_actor`. `priority`: per-source `base` plus label and keyword rules.
- `budget`: `run_tokens`, `run_usd_micros` per run; `daily_tokens`, `daily_usd_micros` across runs.

## Security

- Signatures: Slack `v0` HMAC-SHA256 over `v0:{ts}:{body}`; GitHub `sha256=` HMAC over the body; constant-time compare.
  Verification happens before parsing. Bad signature or stale timestamp is 401 and nothing is stored.
- Replay window: Slack timestamps more than 300 s from now are refused. Replays inside it dedupe on the event id.
- Secrets come from the env vars named in the config, are never in the file, never logged, never in responses. A missing one
  stops `serve` at start and names the variable only.
- Server limits: 1 MiB body cap (413), 16 KiB headers, no chunked bodies, 10 s read/write timeouts, one thread per
  connection. One JSON log line per request on stderr: time, method, path, status, short result, item id. No payload text.
- The item payload is untrusted (SBX-3). It reaches the run as `ctx.input()` with `payload.untrusted = true`. A workflow must
  pass any text from it through `ctx.untrusted(source, text)` before use; instruction-like text then escalates the run.
- Chat cannot deploy, enforced twice: the permission table rejects (and refuses to load) chat rules for deploy workflows, and
  the engine blocks any Deploy action in a run whose trigger ref starts with `slack:`, audits it and escalates. No approval
  overrides the second wall.

## Exposing the server

Not done here. Bind to 127.0.0.1 and put a tunnel or reverse proxy with TLS in front (for example `cloudflared tunnel` or
`tailscale funnel` to the port), then point Slack Event Subscriptions at `https://<host>/slack/events` and a GitHub
repository webhook (content type JSON, `issues` events) at `https://<host>/github/webhook`.

## Limits

- No Slack app was created. Slack is tested with signed fixtures through the real server; the URL-verification handshake
  is covered by connector tests only.
- GitHub polling was live-tested read-only against `kedarisetti/software-factory` (`scripts/m5-live-poll.sh`); the
  repo had no `factory` issues, so the live queue was empty. Webhooks are tested with signed fixtures.
- Polling reads the first 100 issues per call (`gh` 2.4 has no `--slurp`), and no pagination.
- Slack replies and GitHub comments are not sent; intake is inbound only.
- The server has no TLS, no per-IP throttle and no graceful shutdown.

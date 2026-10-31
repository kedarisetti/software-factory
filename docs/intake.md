# Intake (TRG-1 to TRG-4)

Sources turn Slack messages, GitHub issues and MCP tool results into work items. `Intake::accept` (crate `sf-intake`)
decides what happens to each one; `sf intake ...` (crate `sf-cli`) is the server, poller and dispatcher.

## Flow

```
Slack events ---> sf intake serve ---+
GitHub webhook -> (verify, normalise) |--> Intake::accept --> work queue (SQLite, separate from the KB)
sf intake poll (github, MCP) --------+      permission -> near-dup -> rate limit -> priority
                                                                  |
sf intake dispatch  <-- Queued items, highest priority first <----+
   |  daily budget check (stop = "paused: daily budget")
   v
Engine::start_run_with_input(workflow, "{source}:{ref}" or "chat:{source}:{ref}", run budgets, item payload + item_id)
```

Every outcome is stored, none dropped: `queued`, `rejected` (permission table, with reason),
`rate_limited`, `backlog` (TRG-4), then `started` and `done` once dispatched. Exact duplicates
(Slack event id incl. `X-Slack-Retry-Num` retries, GitHub delivery or issue+action) are one item.
Near-duplicates (same actor, channel and normalised words within `near_dup_window_ms`) add no item.

## Commands

All take the global `--db <path>`; `--config` is `docs/intake.example.json` shaped.

- `sf intake serve --addr 127.0.0.1:8787 --config c.json` runs the webhooks and `GET /healthz`. A Slack source named `slack`
  (or the only one) answers `POST /slack/events`; any Slack source answers `POST /slack/<name>/events` with its own secret.
  GitHub likewise: `POST /github/webhook`, `POST /github/<name>/webhook`.
- `sf intake poll [--source <name>] [--now] --config c.json` polls every enabled pollable source (GitHub sources with a `poll`
  block, and MCP sources) whose `interval_s` / `poll_interval_s` has elapsed; `--now` ignores the interval. Last-poll time is the
  store cursor `poll-at:{source}`. Without `--source` a refused or failed source is reported in the result and the sweep goes
  on; with `--source` a refusal or failure is an error (exit 1) with the reason.
- `sf intake poll github --repo org/repo [--source github] [--label factory] [--from-file issues.json] --config c.json` is the
  old one-repo shorthand. It reads open issues (`gh api`, one read-only GET, first 100) and keeps the cursor
  `github-poll:{repo}` (`github-poll:{source}:{repo}` for sources not named `github`; max `updated_at`, epoch ms).
  Issues need a `factory:<workflow>` label, like the webhook; others and pull requests are counted as `skipped`.
- `sf intake connectors list | approve <name> --as <human> | revoke <name> --as <human> --config c.json`: see Approval.
- `sf intake dispatch [--max N] --config c.json` starts Queued items in priority order. Before each start it sums spend of runs
  created since 00:00 UTC (`Engine::spent_since`); at or over `budget.daily_tokens` or `daily_usd_micros` it stops and reports
  `paused: daily budget`. Running work is untouched. Spend accrues as runs tick, so a burst can start several runs before
  the budget shows; per-run budgets bound each of them. Items whose workflow is not registered become `rejected`.
- `sf intake list [--status queued,rejected]` and `sf intake bottlenecks [--window-days 7 --min 3]` (TRG-4: clusters of
  similar items become one `backlog` item linking its evidence; re-running in the same window changes nothing).

Built-in workflows a trigger can name: `demo`, `triage` (reads the item and wraps it with `ctx.untrusted`), `ship` (deploy).

## Config

One `intake.json`. `"version": 2` lists `sources`; a file without `version` (v1, top-level `slack` and `github` sections)
still loads as two sources named `slack` and `github`. Version 2 refuses the v1 sections.

Every source has a unique `name` (1-64 chars of `A-Za-z0-9-_.`, unique ignoring case) and a `kind`. The name is what the
permission table (`rules[].source`), `rate.per_source`, `priority.base`, dedupe keys (`{name}:...`), the work item `source`
and the trigger ref use. In version 2 a rule or rate limit that names an unknown source fails the load (catches typos).
A source with no `priority.base` entry gets 10 (github) or 5.

- `kind: "slack"` (chat; never starts a deploy workflow): `signing_secret_env`, `bot_user_id`, `channel_defaults` (channel id ->
  workflow when the message names none). Message form: `@bot <workflow> <text>`. Add one source per workspace.
- `kind: "github"`: `webhook_secret_env` and/or `poll: {repos: [...], label: "factory", interval_s: 300}`. Workflow comes
  from the `factory:<workflow>` label. Add one source per account or repo set.
- `kind: "mcp"`: see below.
- `permissions`: `rules` (source name, channel or `*`, optional `actors`, exact `workflows`) and `deploy_workflows`. Default deny.
  Chat sources are the `slack`-kind ones and any with `chat: true`; the table fails to load if one allows a deploy workflow.
  `ship` contains a Deploy action, so it is listed as a deploy workflow and only non-chat sources may start it.
- `rate`: `window_ms`, `per_source`, `per_actor`. `priority`: per-source `base` plus label and keyword rules.
- `budget`: `run_tokens`, `run_usd_micros` per run; `daily_tokens`, `daily_usd_micros` across runs.

### MCP sources

```json
{ "name": "linear", "kind": "mcp",
  "command": ["npx", "-y", "mcp-remote", "https://mcp.linear.app/mcp"],
  "env": { "LINEAR_API_KEY": "${SF_LINEAR_API_KEY}" },
  "tool": "list_issues", "arguments": { "updatedAt": "{cursor}", "limit": 50 },
  "poll_interval_s": 300, "timeout_ms": 30000, "chat": false, "default_workflow": "triage",
  "map": { "items": "/issues", "id": "/identifier", "title": "/title", "text": "/description", "actor": "/creator/name",
           "channel": "/team/key", "labels": "/labels", "url": "/url", "updated_at": "/updatedAt" },
  "cursor": "max:updated_at" }
```

- `command`: argv, started without a shell. `env`: `NAME` -> `"${HOST_VAR}"`; values are references to the host environment
  and nothing else. The child gets a cleared environment with `PATH` plus exactly these.
- `tool`, `arguments`: the `tools/call`. Any `{cursor}` inside a string of `arguments` is replaced with the saved cursor
  (empty string on the first poll).
- `chat`: true for chat-like sources (a Slack MCP). Then the chat rules apply: no rule may allow a deploy workflow, and runs
  start under `chat:<name>:<ref>`, which the engine treats as chat.
- `map`: JSON pointers. `items` points at the array in the tool result (`structuredContent` if the server sends it, else the
  first text content parsed as JSON); the rest point into each item. `id` and `title` are required (items missing either are
  `skipped`); `text`, `actor` (default `unknown`), `channel`, `labels` (array of strings or objects with `name`), `url`
  (appended to the text), `updated_at` and `workflow` are optional. `default_workflow` is used when `workflow` is absent or empty;
  with neither, the item is stored as `rejected` ("no workflow requested"). Dedupe key: `{name}:{id}:{updated_at or ''}`, so an
  updated ticket is a new event (near-duplicate detection still drops identical content).
- `cursor`: a JSON pointer into the tool result for the next cursor value (for example `/pageInfo/endCursor`), or
  `max:updated_at` for the greatest mapped `updated_at`. Strings and numbers are kept as text. It is saved only after every
  item was accepted. Cursors are strings, which `Store::set_cursor` cannot hold, so they live in `<db>.intake-state.json`
  (key `mcp-cursor:{name}`, atomic rename). Delete the key to re-read from the start.
- `poll_interval_s` (default 300), `timeout_ms` (default 30000, per request: initialize, tools/list, tools/call).

Session per poll (newline-delimited JSON-RPC over stdio): `initialize` (protocol `2025-06-18`, client `sf-intake`),
`notifications/initialized`, `tools/list` (fails clearly if the tool is not offered; a server without it is tolerated),
`tools/call`, then stdin is closed and the process reaped. On timeout the whole process group is killed. `isError`
results, JSON-RPC errors, non-JSON text and missing servers are reported with the source name and nothing is queued.
Streamable HTTP servers are not supported directly: bridge them over stdio, as in the example (`npx -y mcp-remote <url>`).

### Adding an MCP source, step by step

1. Pick a server and the tool that lists your tickets. Run it once by hand (or with an MCP inspector) and note the JSON the
   tool returns.
2. Add a source to `sources` as above. Write the `map` pointers against that JSON. Reference secrets as `${VAR}` and export
   `VAR` in the environment where `sf intake poll` runs. Add a permission rule for the source name and the channel your
   `channel` pointer yields (for example the Linear team key), listing only workflows it may start.
3. `sf intake connectors list --config intake.json` shows the source as `enabled: false`, `approval: pending`, with its digest.
4. Review the command, env var names, tool, arguments and map, then
   `sf intake connectors approve linear --as <you> --config intake.json`.
5. `sf intake poll --source linear --now --config intake.json`, then `sf intake list --status queued`. Add the poll to cron.

## Approval (new connectors)

The spec requires human approval for a new tool, model or connector. An MCP source starts a local process, so it is
disabled until approved. The approval binds to a digest (sha256) of the command, env var names (the `NAME` and the `${VAR}`
name, never values), tool, arguments, map, cursor, `chat` and `default_workflow`. Any change to those disables the source
again until it is approved at the new digest; `poll_interval_s` and `timeout_ms` are not part of it.

Approve writes an audit record (`action new_capability`, `target intake:<name>`, `digest`, decision `approved`, `approver`);
revoke writes the same with decision `denied`. The current state is the latest approve/revoke for that name and the current
digest, read from the audit log (hash-chained, `sf audit-verify`). A poll of an unapproved source never starts the process:
it fails with a message naming the fix and audits a `blocked` record (once per digest until the state changes). Reverting a
config change to a digest that was approved before re-enables it unless it was revoked since. Slack and GitHub sources are
part of the factory and need no approval; `connectors list` shows them as `built-in`.

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
  the engine blocks any Deploy action in a run whose trigger ref starts with `chat:` (or `slack:`), audits it and escalates.
  Dispatch puts every chat source (kind `slack`, or MCP with `chat: true`) under `chat:<source>:<ref>`. No approval
  overrides the second wall.
- An MCP server is an approved local process. It sees only `PATH` and the env vars the config names. Secrets are never in the
  file: env values must be `${VAR}` references, and the load refuses env literals and anything in `command` or `arguments`
  that looks like a token (known prefixes such as `xoxb-` or `ghp_`, bearer headers, long mixed alphanumeric strings).
  Errors name the source, not the value.
- MCP output is untrusted. Items go through the same path as any event: permission table, rate limit, and to the run as
  `payload` with `untrusted = true` (SBX-3). Output is size-limited (8 MiB per line, 20 KB of text per item).
- The egress of the MCP process itself is not sandboxed yet. It can reach whatever the host can and use the env it was
  given, so approval of the exact command is the control, and why a changed config needs approval again.

## Exposing the server

Not done here. Bind to 127.0.0.1 and put a tunnel or reverse proxy with TLS in front (for example `cloudflared tunnel` or
`tailscale funnel` to the port), then point Slack Event Subscriptions at `https://<host>/slack/events` and a GitHub
repository webhook (content type JSON, `issues` events) at `https://<host>/github/webhook`.

## Limits

- No Slack app was created. Slack is tested with signed fixtures through the real server; the URL-verification handshake
  is covered by connector tests only.
- GitHub polling was live-tested read-only against `kedarisetti/software-factory` (`scripts/m5-live-poll.sh`); the
  repo had no `factory` issues, so the live queue was empty. Webhooks are tested with signed fixtures.
- Polling reads the first 100 issues per call (`gh` 2.4 has no `--slurp`), and no pagination. MCP pagination is whatever the
  tool and `cursor` give; there is no automatic follow-up page within one poll.
- MCP over streamable HTTP is not built in (use a stdio bridge). The state file has no lock, so run one poller per source.
- Slack replies and GitHub comments are not sent; intake is inbound only.
- The server has no TLS, no per-IP throttle and no graceful shutdown.

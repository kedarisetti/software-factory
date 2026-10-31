# Intake (TRG-1 to TRG-4)

Every trigger is a configured source. Nothing vendor-specific is built in: a source is a `webhook`, a `command` or an
`mcp` tool call, and its `events` pipeline turns the JSON it receives into events. Slack, GitHub, Linear and the like are
presets in `docs/presets/` that you paste into `sources`. `Intake::accept` (crate `sf-intake`) decides what happens to each
event; `sf intake ...` (crate `sf-cli`) serves, polls, dispatches and runs the loop.

## Commands

All take the global `--db <path>`; `--config` is `docs/intake.example.json` shaped.

- `sf intake serve --addr 127.0.0.1:8787 --config c.json`: `POST /hooks/<source-name>` for each webhook source, `GET /healthz`.
  Anything else is 404 (also an unknown name, or a source that is not a webhook). A challenge reply is `text/plain`.
- `sf intake poll [--source <name>] [--now] --config c.json`: polls every enabled `command` / `mcp` source whose
  `poll_interval_s` has elapsed (`--now` ignores it). Last poll time is the store cursor `poll-at:{source}`. In the sweep a
  refused or failing source is reported in the result and skipped (a failing one waits one interval); with `--source` it is an
  error. Naming a webhook source is an error (`webhook only`); the sweep skips webhooks silently.
- `sf intake connectors list | approve <name> --as <human> | revoke <name> --as <human> --config c.json`: see Approval.
- `sf intake dispatch [--max N] --config c.json`: starts Queued items in priority order. Before each start it sums spend of
  runs created since 00:00 UTC (`Engine::spent_since`); at or over `budget.daily_tokens` / `daily_usd_micros` it stops and
  reports `paused: daily budget`. Running work is untouched. Items whose workflow is not registered become `rejected`.
- `sf intake daemon --config c.json [--addr host:port] [--tick-s 5]`: one process for the whole loop. With `--addr` it serves
  the webhooks on a thread (and so needs the secrets). Every tick: poll due sources (errors are JSON lines on stderr, never
  fatal), dispatch queued items (daily budget respected), then `recover` and `run_due` until nothing progresses (what `sf run`
  does). It prints one JSON line per tick that did something: `{"tick": ts, "polled": n, "dispatched": n, "ticked": n}`.
  Runs waiting on approval or an event stay parked; approve them with `sf approve`.
- `sf intake list [--status queued,rejected]`, `sf intake bottlenecks [--window-days 7 --min 3]` (TRG-4: clusters of similar
  items become one `backlog` item linking its evidence; re-running in the same window changes nothing).

Built-in workflows a trigger can name: `demo`, `triage` (reads the item and wraps it with `ctx.untrusted`), `ship` (deploy).

## Config

One `intake.json`, `"version": 2` (version 1 and the old top-level `slack` / `github` sections or `kind: slack|github` are
refused; use the presets below).

```json
{ "version": 2, "sources": [ ... ], "permissions": {...}, "rate": {...}, "near_dup_window_ms": 600000,
  "priority": { "base": {"name": 10}, "rules": [...] }, "budget": {...} }
```

Every source has a unique `name` (1-64 chars of `A-Za-z0-9-_.`, unique ignoring case), a `kind` and an `events` pipeline. The name
is what `permissions.rules[].source`, `rate.per_source`, `priority.base`, dedupe keys (`{name}:...`), the work item `source` and
the trigger ref use. A rule or rate limit naming an unknown source fails the load.

- `permissions`: `rules` (source name, channel or `*`, optional `actors`, exact `workflows`) and `deploy_workflows`. Default deny.
  `ship` contains a Deploy action, so list it as a deploy workflow: a source with `chat: true` may not allow one (load fails).
- `rate`: `window_ms`, `per_source`, `per_actor`. `priority`: `base` per source name (missing = 0; there are no defaults) plus
  label and keyword rules. `budget`: `run_tokens`, `run_usd_micros` per run; `daily_tokens`, `daily_usd_micros` across runs.
- `chat: true` (any kind) marks a chat source: the chat rules apply and runs start under `chat:<source>:<ref>`, which the engine
  treats as chat. Otherwise the trigger ref is `<source>:<ref>`.

### `events` (the pipeline)

1. `items`: JSON pointer to the array of items (absent: the document is one item; `""` is a document that is itself an array).
2. `only`: keep an item only if one of these matches (absent: keep all). `ignore`: drop it if any matches.
3. `map`: build the event. 4. `workflow`: rules that pick the requested workflow. 5. `unmatched`.

References: `/json/pointer` into the item, `header:Name` (webhooks only), `source` (the source name).

Templates: a single reference (`"/issue/title"`) or text with holes (`"{/repository/full_name}#{/issue/number}"`). A missing
reference renders empty.

Conditions (`only` / `ignore` entries are objects of reference -> condition, all must hold): a string (equals), an array
(one of), or `{"exists": bool, "equals": s, "contains": s, "prefix": s}`. An empty match is refused.

`map`: `id` (required, becomes `source_ref`; items without one are `skipped`), `title` (default: first line of `text`; an item
with neither is `skipped`), `text`, `actor` (default `unknown`), `channel`, `labels` (pointer to an array of strings or `{name}`
objects), `url` (appended to the text), `updated_at`, `dedupe` (default `{id}:{updated_at}`; always prefixed `{source}:`).
Title is capped at 200 chars and text at 20 000.

`workflow` rules, first non-empty wins: `{"field": template}`, `{"label_prefix": {"prefix": "factory:", "from": "/label/name"}}`
(`from` is one string; default: the mapped labels), `{"command": {"mention": "<@BOT>"}}` (text starting with the mention, then
`<workflow> <rest>`; the text becomes `<rest>`), `{"by_channel": {"C1": "demo"}}`, `{"default": "triage"}`.

`unmatched`: `ignore` (default; the item is dropped and counted as `ignored`) or `reject` (stored as rejected, "no workflow requested").

### `webhook`

```json
{ "name": "gh-issues-hook", "kind": "webhook", "chat": false,
  "verify": { "secret_env": "SF_GH_WEBHOOK_SECRET", "header": "X-Hub-Signature-256", "prefix": "sha256=" },
  "challenge": { "when": { "/type": "url_verification" }, "reply": "/challenge" },
  "events": { ... } }
```

- `verify`: HMAC-SHA256, constant-time compare, before the body is parsed. `secret_env` is an env var NAME. `header` carries
  `prefix` + hex digest. `signed` is what is signed: text with `{header:Name}` holes and `{body}` (default `{body}`).
  `timestamp_header` (unix seconds) with `max_skew_s` (default 300) refuses stale or future requests. Bad signature or stale
  timestamp is 401, a missing header or bad body 400, nothing stored.
- `challenge` (optional): when `when` matches the body, answer `reply` (a template) as `text/plain` and store nothing.
- A body that maps to no events answers 200 `ignored` (senders do not retry). Several events are all accepted; the response lists
  them under `items` and repeats the first one's fields at the top level.
- A missing secret stops `serve` / `daemon --addr` at start and names the variable only. No approval is needed: the secret is the gate.

### `command`

```json
{ "name": "gh-issues", "kind": "command",
  "command": ["gh", "api", "--method", "GET", "repos/o/r/issues?state=open&sort=updated&direction=asc&since={cursor}"],
  "env": { "GH_TOKEN": "${SF_GH_TOKEN}" }, "poll_interval_s": 300, "timeout_ms": 30000, "chat": false,
  "cursor": "max:updated_at", "cursor_start": "1970-01-01T00:00:00Z", "events": { ... } }
```

- `command`: argv, run without a shell. `{cursor}` inside any element is replaced by the saved cursor, else `cursor_start`, else "".
- `env`: `NAME` -> `"${HOST_VAR}"`, references to the host environment and nothing else. The child gets a cleared environment with
  `PATH`, `HOME` and exactly these, no stdin, its own process group; on `timeout_ms` the group is killed.
- stdout must be JSON (8 MiB cap). A non-zero exit is an error with the first 500 chars of stderr (env values are masked).

### `mcp`

```json
{ "name": "linear", "kind": "mcp", "command": ["npx", "-y", "mcp-remote", "https://mcp.linear.app/mcp"],
  "env": { "LINEAR_API_KEY": "${SF_LINEAR_API_KEY}" },
  "tool": "list_issues", "arguments": { "updatedAt": "{cursor}", "limit": 50 },
  "poll_interval_s": 300, "timeout_ms": 30000, "chat": false,
  "cursor": "max:updated_at", "cursor_start": null, "events": { "items": "/issues", ... } }
```

Same `command` / `env` / cursor rules. `tool` + `arguments` are the `tools/call`; `{cursor}` is replaced in every string of
`arguments`. The result is `structuredContent` if the server sends it, else the first text content parsed as JSON.
Session: `initialize` (protocol `2025-06-18`, client `sf-intake`), `notifications/initialized`, `tools/list` (fails clearly if
the tool is not offered; a server without it is tolerated), `tools/call`, then stdin is closed and the process reaped; on timeout
the process group is killed (`timeout_ms` is per request). `isError` results, JSON-RPC errors and non-JSON text are reported
with the source name and nothing is queued. Streamable HTTP servers are bridged over stdio (`npx -y mcp-remote <url>`).

### Cursors

`cursor` is `max:updated_at` (greatest mapped `updated_at`; needs `map.updated_at`; never moves backwards) or a JSON pointer into
the result (e.g. `/pageInfo/endCursor`). Strings and numbers are kept as text. It is saved only after every event was accepted,
so a failure re-reads and never skips. Cursors are strings, which `Store::set_cursor` cannot hold, so they live in
`<db>.intake-state.json` (key `cursor:{name}`, atomic rename); delete the key to start over. Use one poller per source (no lock).

## Presets

One source object per file in `docs/presets/`, ready to paste into `sources`. Edit the ids, repo, env var names and secrets
names for your setup; add a `permissions` rule and `priority.base` entry for the source name.

| File | Kind | What it does |
|---|---|---|
| `slack-events.json` | webhook, `chat: true` | Slack Events API: `v0` signature over `v0:{ts}:{body}` with a 300 s window, URL-verification challenge, `message` and `app_mention` events only, drops bots, edits, its own user `U0FACTORY` and messages that also mention it. Text `<@U0FACTORY> <workflow> <text>`, else the channel's default workflow. Dedupe on `event_id` (retries collapse). |
| `github-issues-webhook.json` | webhook | `X-Hub-Signature-256` `sha256=`; `issues` events `opened`/`labeled`/`reopened`; workflow from the added label `factory:<wf>`, else any `factory:` label on the issue. Dedupe on `X-GitHub-Delivery`. |
| `github-issues-poll.json` | command | `gh api` GET of open issues, ascending by update, `since={cursor}`; pull requests ignored; workflow from the `factory:<wf>` label; dedupe on the issue number. Reads the first 100 issues per poll, no pagination. Needs `SF_GH_TOKEN`. |
| `linear-mcp.json` | mcp | `list_issues` through `mcp-remote`; every issue runs `triage`; cursor on `updatedAt`. Needs `SF_LINEAR_API_KEY`. |

A source for another tool is the same shape: point `items`/`map` pointers at its JSON.

### Adding a command or mcp source, step by step

1. Run the command (or tool) once by hand and note the JSON it returns.
2. Add a source with `events` pointers written against that JSON. Reference secrets as `${VAR}` and export `VAR` where the
   poller runs. Add a permission rule for the source name and the channel your `channel` template yields.
3. `sf intake connectors list --config intake.json` shows it as `enabled: false`, `approval: pending`, with its digest.
4. Review command, env var names, tool, arguments, events and cursor, then `sf intake connectors approve <name> --as <you> --config intake.json`.
5. `sf intake poll --source <name> --now --config intake.json`, then `sf intake list --status queued`. Run `sf intake daemon`
   (or cron the poll) from then on.

## How a trigger becomes a run

```
webhook POST /hooks/<name> --> verify (HMAC, replay window) --> challenge? --+
command / mcp poll (approved, cursor) ------------------------------------+--> pipeline: items, only/ignore, map, workflow
                                                                           v
                                    Intake::accept: workflow requested? -> permission table (default deny; chat cannot
                                    deploy) -> near-duplicate -> rate limit -> priority -> work_items (SQLite, not the KB)
                                           |  stored as Queued, Rejected (reason), RateLimited or Backlog (TRG-4);
                                           |  exact duplicate (same dedupe key) = the existing item
                                           v
              dispatch: Queued items, highest priority first, one run each; paused by the daily budget
                                           v
        Engine::start_run_with_input(workflow, trigger ref, run budgets, item)   trigger ref: "<source>:<ref>" or "chat:<source>:<ref>"
        the item (payload.untrusted = true) is delivered as input:<run>; the item becomes Started with its run_id
                                           v
        the engine drives the steps: `sf run` (or the daemon) calls recover + run_due until nothing progresses
```

Every outcome is stored, none dropped. Near-duplicates (same source, channel, actor and normalised words within
`near_dup_window_ms`) add no item. Priority is `priority.base[source]` plus the strongest matching boost plus the strongest
demotion, so stacked labels do not inflate it.

## Approval (new connectors)

The spec requires human approval for a new tool, model or connector. `command` and `mcp` sources start a local process, so each is
disabled until approved. The approval binds to a digest (sha256) of kind, command, env var names (the `NAME` and the `${VAR}` name,
never values), tool, arguments, `events`, cursor, `cursor_start` and `chat`. Any change disables the source again until it is approved
at the new digest; `poll_interval_s` and `timeout_ms` are not part of it.

Approve writes an audit record (`action new_capability`, `target intake:<name>`, `digest`, decision `approved`, `approver`); revoke
writes the same with decision `denied`. The current state is the latest approve/revoke for that name and digest, read from the
hash-chained audit log (`sf audit-verify`). A poll of an unapproved source never starts the process: it fails naming the fix and
audits a `blocked` record (once per digest until the state changes). Reverting a config change to a digest approved before
re-enables it unless it was revoked since. Webhook sources run nothing, need no approval, and show as `not required` in `connectors list`.

## Security

- Secrets come from the env vars named in the config, never the file, logs or responses. `secret_env` must be a NAME; `env`
  values must be `${VAR}`; the load refuses literals in `command`, `arguments`, `events`, `challenge` and `cursor_start` that
  look like tokens (known prefixes, bearer headers, long mixed alphanumeric strings). Errors name the source, not the value.
- Server limits: 1 MiB body cap (413), 16 KiB headers, no chunked bodies, 10 s read/write timeouts, one thread per connection.
  One JSON log line per request on stderr: time, method, path, status, short result, item id. No payload text.
- Everything a source returns is untrusted (SBX-3): it reaches the run as `ctx.input()` with `payload.untrusted = true`, and a
  workflow must pass any text from it through `ctx.untrusted(source, text)`; instruction-like text then escalates the run.
- Chat cannot deploy, enforced twice: the permission table rejects (and refuses to load) `chat: true` rules for deploy workflows,
  and the engine blocks any Deploy action in a run whose trigger ref starts with `chat:`, audits it and escalates.
- A `command` / `mcp` process sees only `PATH`, `HOME` (command) and the env vars the config names. Its network egress is not
  sandboxed yet, so approval of the exact command is the control.

## Exposing the server

Bind to 127.0.0.1 and put a tunnel or reverse proxy with TLS in front (for example `cloudflared tunnel` or `tailscale funnel`),
then point each sender at `https://<host>/hooks/<source-name>` (for the presets: the Events API request URL, and a repository
webhook with content type JSON and the `issues` event).

## Limits

- No chat app was created. The chat preset is tested with signed fixtures through the real server; the URL-verification
  handshake is covered by connector tests only.
- `gh api` polling was live-tested read-only (`scripts/m5-live-poll.sh`). It reads the first 100 issues per call, no pagination;
  MCP pagination is whatever the tool and `cursor` give.
- The state file has no lock. No replies are sent anywhere; intake is inbound only. The server has no TLS, no per-IP throttle and
  no graceful shutdown.

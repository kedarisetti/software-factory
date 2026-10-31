# Pi adapter for sf-kb (KIF-6)

`sf-kb.ts` registers `kb_search`, `kb_get`, `kb_list`, `kb_add`, `kb_learn` and `kb_feedback` in Pi. Each tool shells out to the `sf-kb` CLI with `--agent pi --session <pi session id>` and returns its JSON. It holds no logic.

## Wiki and flow

The store is a browsable wiki of plain Markdown: `<kind>/<topic>/<slug>.md`, with a generated `index.md` in every folder. Open the store folder in Obsidian, or read `index.md` on GitHub if it is committed. Agents use the same tree through the tools:

1. `kb_search` first, with the task's own wording unchanged; reformulate only on later calls. Hits are paths + titles + snippets.
2. `kb_get` only the narrowed candidates.
3. `kb_list` a folder (start at `""`) for "everything about X", or when search misses.
4. `kb_learn` at the end of the task; `kb_add` with kind `page` for a synthesis page linking its sources with `derived-from`.

## Install

1. Build the CLI: `cargo build --release -p sf-kb-cli` (binary: `target/release/sf-kb`) and put it on `PATH`, or set `SF_KB_BIN`.
2. Symlink the extension into Pi's auto-discovery folder:
   ```
   mkdir -p ~/.pi/agent/extensions
   ln -s "$PWD/adapters/pi/sf-kb.ts" ~/.pi/agent/extensions/sf-kb.ts
   ```
   For one project only, use `.pi/extensions/sf-kb.ts` instead. Run `/reload` in Pi, or try it with `pi -e adapters/pi/sf-kb.ts`.

## Environment

| Variable | Meaning | Default |
|---|---|---|
| `SF_KB_BIN` | Path to the `sf-kb` binary | `sf-kb` on `PATH` |
| `SF_KB_ROOT` | Knowledge store directory | `./.knowledge` |

Every call records agent `pi` and Pi's session ID (falls back to the process ID), so retrieval and learnings can be compared across agents (KIF-5).

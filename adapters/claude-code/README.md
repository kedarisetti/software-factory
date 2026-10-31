# Claude Code adapter for sf-kb (KIF-6)

Claude Code talks to the knowledge service through the MCP server `sf-kb mcp`. It exposes `kb_search`, `kb_get`, `kb_list`, `kb_add`, `kb_learn`, `kb_feedback` and `kb_check_anchors`.

## Wiki and flow

The store is a browsable wiki of plain Markdown: `<kind>/<topic>/<slug>.md`, with a generated `index.md` in every folder. Open the store folder in Obsidian, or read `index.md` on GitHub if it is committed. Agents use the same tree through the tools:

1. `kb_search` first, with the task's own wording unchanged; reformulate only on later calls. Hits are paths + titles + snippets.
2. `kb_get` only the narrowed candidates.
3. `kb_list` a folder (start at `""`) for "everything about X", or when search misses.
4. `kb_learn` at the end of the task; `kb_add` with kind `page` for a synthesis page linking its sources with `derived-from`.

The server also sends this flow as MCP `instructions` on `initialize`.

Build first: `cargo build --release -p sf-kb-cli`, then put `target/release/sf-kb` on `PATH` (or use its absolute path below).

## Register with the CLI

```
claude mcp add sf-kb --scope user -e SF_KB_ROOT=/abs/path/to/.knowledge -- sf-kb mcp --agent claude-code
```

## Or `.mcp.json` (project scope)

```json
{
  "mcpServers": {
    "sf-kb": {
      "command": "sf-kb",
      "args": ["mcp", "--agent", "claude-code"],
      "env": { "SF_KB_ROOT": "/abs/path/to/.knowledge" }
    }
  }
}
```

Optional env: `SF_KB_SESSION` (session ID recorded on every call; default `cli-<pid>`), `SF_KB_AGENT` (used if `--agent` is absent).

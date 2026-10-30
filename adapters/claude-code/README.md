# Claude Code adapter for sf-kb (KIF-6)

Claude Code talks to the knowledge service through the MCP server `sf-kb mcp`. It exposes `kb_search`, `kb_get`, `kb_add`, `kb_learn`, `kb_feedback` and `kb_check_anchors`.

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

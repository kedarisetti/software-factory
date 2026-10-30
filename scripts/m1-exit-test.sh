#!/usr/bin/env bash
# Milestone 1 exit test: Pi (via its extension) and Claude Code (via MCP) search and write
# learnings to one store, every call is logged with its agent, and the anchor check runs.
# Needs: pi with a tool-calling model (default gemma4-12b-qat:16k on Ollama), claude CLI.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
export SF_KB_BIN="$repo/target/release/sf-kb" SF_KB_ROOT="$work/kb"
cargo build -q --release -p sf-kb-cli --manifest-path "$repo/Cargo.toml"
finding="Pin the rust toolchain in rust-toolchain.toml so CI and local builds match"

cd "$work"
pi --model "${PI_MODEL:-gemma4-12b-qat:16k}" -e "$repo/adapters/pi/sf-kb.ts" -p \
  "Call kb_learn once with task_class \"fix-ci\", outcome \"succeeded\", worked: [\"$finding\"]. Then call kb_search with query \"rust toolchain\" and task_class \"fix-ci\"."

cat > mcp.json <<JSON
{"mcpServers":{"sf-kb":{"command":"$SF_KB_BIN","args":["mcp","--agent","claude-code"],"env":{"SF_KB_ROOT":"$SF_KB_ROOT"}}}}
JSON
claude -p "Call kb_search with query 'rust toolchain CI' and task_class 'fix-ci'. Then call kb_learn once with task_class 'fix-ci', outcome 'succeeded', worked: ['$finding']." \
  --model "${CLAUDE_MODEL:-haiku}" --strict-mcp-config --mcp-config mcp.json \
  --allowedTools "mcp__sf-kb__kb_search,mcp__sf-kb__kb_learn" </dev/null

"$SF_KB_BIN" check-anchors --agent nightly >/dev/null

log="$SF_KB_ROOT/.kb/calls.jsonl"
fail() { echo "FAIL: $1"; exit 1; }
for a in pi claude-code; do
  for op in search learn; do grep -q "\"agent\":\"$a\".*\"op\":\"$op\"" "$log" || fail "$a did not $op"; done
done
grep -q '"op":"check_anchors"' "$log" || fail "anchor check not logged"
grep -q '^count: 2$' "$SF_KB_ROOT"/entries/learning-*.md || fail "learnings from both agents did not merge"
echo "PASS: both agents share one store ($SF_KB_ROOT)"

# software-factory

A hands-off software factory: durable, sandboxed agents that turn tickets and threads into shipped changes, and get cheaper with every run.

Built in the open. Early design stage.

- Spec: [docs/spec.md](docs/spec.md)
- Layout: a Cargo workspace. `crates/kb` is standalone (KIF-1); orchestrator-side crates depend only on `core`.

```
crates/kb/                 # knowledge service core: entry files, six ops, anchor check (M1)
crates/kb-cli/             # sf-kb CLI (JSON) + MCP server (sf-kb mcp)
crates/core/               # orchestrator types + traits (Store, Sandbox, Clock) (M2)
crates/orchestrator/       # run lifecycle, checkpoints, timers
crates/store-sqlite/       # local durable log (WAL, synchronous=FULL)
crates/retrieval-tantivy/  # BM25 index over knowledge + learnings (a cache)
crates/sandbox-local/      # isolated tool execution
adapters/                  # thin Pi extension + Claude Code MCP config
scripts/m1-exit-test.sh    # M1 exit test: Pi and Claude Code share one store
agents/                    # Python/TS agent glue
evals/golden/              # golden task set
```

## Try the knowledge service

```
cargo build --release -p sf-kb-cli
export SF_KB_ROOT=$PWD/.knowledge
target/release/sf-kb --agent human add --title "Pin the toolchain" --body "rust-toolchain.toml" --tag ci
target/release/sf-kb search "toolchain"
```

Wire it into agents with [adapters/pi](adapters/pi/README.md) and [adapters/claude-code](adapters/claude-code/README.md).

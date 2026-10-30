# software-factory

A hands-off software factory: durable, sandboxed agents that turn tickets and threads into shipped changes, and get cheaper with every run.

Built in the open. Early design stage.

- Spec: [docs/spec.md](docs/spec.md)
- Layout: a Cargo workspace. `crates/core` holds shared types and plugin traits; each other crate implements one trait and depends only on `core`.

```
crates/core/               # types + traits (Store, Retriever, Sandbox, Clock)
crates/orchestrator/       # run lifecycle, checkpoints, timers
crates/store-sqlite/       # local durable log (WAL, synchronous=FULL)
crates/retrieval-tantivy/  # BM25 over knowledge + learnings
crates/sandbox-local/      # isolated tool execution
agents/                    # Python/TS agent glue
evals/golden/              # golden task set
```

/**
 * Pi adapter for the sf-kb knowledge service (KIF-6). No logic: each tool maps its
 * arguments onto the `sf-kb` CLI and returns the JSON the CLI prints.
 * Env: SF_KB_BIN (binary, default "sf-kb"), SF_KB_ROOT (store), forwarded to the CLI as-is.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { StringEnum } from "@earendil-works/pi-ai";
import { Type } from "typebox";

const BIN = process.env.SF_KB_BIN || "sf-kb";

export default function (pi: ExtensionAPI) {
  const call = (tool: string, label: string, description: string, parameters: any, argv: (p: any) => string[]) =>
    pi.registerTool({
      name: tool,
      label,
      description,
      parameters,
      async execute(_id, params: any, signal, _onUpdate, ctx) {
        let session = String(process.pid);
        try { session = ctx.sessionManager.getSessionId() || session; } catch {}
        const r = await pi.exec(BIN, ["--agent", "pi", "--session", session, ...argv(params)], { signal });
        const text = r.stdout.trim() || JSON.stringify({ ok: false, error: { code: "adapter", message: r.stderr.trim() || `sf-kb exited ${r.code}` } });
        return { content: [{ type: "text", text }], details: {} };
      },
    });

  call("kb_search", "KB search",
    "Search the shared knowledge base (conventions, glossary, learnings from earlier runs, synthesis pages). Call this FIRST at the start of a task, with the task's own wording unchanged; reformulate only on later calls. Returns paths (ids) + titles + snippets; then kb_get only the few that look relevant.",
    Type.Object({
      query: Type.String({ description: "Free-text query" }),
      task_class: Type.Optional(Type.String({ description: "Task class filter, e.g. fix-ci" })),
      top_k: Type.Optional(Type.Number({ description: "Max hits (default 5)" })),
    }),
    (p) => ["search", p.query, ...(p.task_class ? ["--task-class", p.task_class] : []), ...(p.top_k ? ["--top-k", String(p.top_k)] : [])]);

  call("kb_get", "KB get",
    "Read one knowledge entry in full by id. Read only the narrowed candidates from kb_search or kb_list; the id is its path, e.g. learnings/fix-ci/pin-the-rust-toolchain.",
    Type.Object({ id: Type.String({ description: "Entry id: its path without .md" }) }),
    (p) => ["get", p.id]);

  call("kb_list", "KB list",
    "Browse one folder of the knowledge wiki: sub-folders with entry counts, and entries with title and summary. Start at \"\" for the root index. Best for \"everything about X\" questions, or when kb_search misses.",
    Type.Object({ path: Type.Optional(Type.String({ description: "Folder, e.g. learnings/fix-ci; empty for the root" })) }),
    (p) => ["list", ...(p.path ? [p.path] : [])]);

  call("kb_add", "KB add",
    "Save a durable fact worth keeping (convention, glossary term, note). For a synthesis page over several entries use kind \"page\" and link each source with derived-from. Use kb_learn for end-of-task lessons.",
    Type.Object({
      title: Type.String(),
      body: Type.String({ description: "Markdown body" }),
      kind: Type.Optional(StringEnum(["note", "learning", "glossary", "convention", "page"] as const)),
      tags: Type.Optional(Type.Array(Type.String())),
      task_class: Type.Optional(Type.String()),
      source: Type.Optional(Type.String({ description: "URL, ticket, file" })),
      ttl_ms: Type.Optional(Type.Number({ description: "Expire after this many ms" })),
      links: Type.Optional(Type.Array(Type.Object({
        type: StringEnum(["depends-on", "supersedes", "contradicts", "derived-from"] as const),
        target: Type.String({ description: "Target entry id" }),
      }))),
    }),
    (p) => ["add", "--json", JSON.stringify(p)]);

  call("kb_learn", "KB learn",
    "Call at the END of a task (after search, get, list) to record what worked, what failed, dead ends and the shortest path, so later runs start ahead.",
    Type.Object({
      task_class: Type.String(),
      worked: Type.Optional(Type.Array(Type.String())),
      failed: Type.Optional(Type.Array(Type.String())),
      dead_ends: Type.Optional(Type.Array(Type.String())),
      shortest_path: Type.Optional(Type.String()),
      outcome: Type.Optional(StringEnum(["succeeded", "failed", "reverted"] as const)),
      steps: Type.Optional(Type.Array(Type.String())),
    }),
    (p) => ["learn", "--json", JSON.stringify(p)]);

  call("kb_feedback", "KB feedback",
    "Tell the knowledge base whether an entry you used helped, misled, was wrong or stale. Wrong/stale entries are retired.",
    Type.Object({
      id: Type.String(),
      verdict: StringEnum(["helped", "misled", "wrong", "stale"] as const),
      note: Type.Optional(Type.String()),
    }),
    (p) => ["feedback", p.id, p.verdict, ...(p.note ? ["--note", p.note] : [])]);
}

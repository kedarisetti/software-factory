//! The one place the seven operations (KIF-4) are dispatched from JSON args. The CLI and the MCP
//! server both call [`dispatch`], so they behave identically (KIF-3).

use serde::Deserialize;
use serde_json::{json, Value};
use sf_kb::{Caller, Knowledge, LearningRecord, NewEntry, Verdict};

/// `(code, message)`; codes are `sf_kb::Error::code()` plus `invalid` and `unknown_op`.
pub type OpError = (String, String);

pub const OPS: [&str; 7] = ["search", "get", "list", "add", "learn", "feedback", "check_anchors"];

const DEFAULT_TOP_K: usize = 5;

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    task_class: Option<String>,
    top_k: Option<usize>,
}

#[derive(Deserialize)]
struct GetArgs {
    id: String,
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
struct FeedbackArgs {
    id: String,
    verdict: Verdict,
    note: Option<String>,
}

fn args_of<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, OpError> {
    let args = if args.is_null() { json!({}) } else { args };
    serde_json::from_value(args).map_err(|e| ("invalid".into(), e.to_string()))
}

fn out<T: serde::Serialize>(r: sf_kb::Result<T>) -> Result<Value, OpError> {
    let v = r.map_err(|e| (e.code().to_string(), e.to_string()))?;
    Ok(serde_json::to_value(v).expect("results serialize"))
}

/// Run operation `op` (one of [`OPS`]) with JSON `args`.
pub fn dispatch(kb: &impl Knowledge, caller: &Caller, op: &str, args: Value) -> Result<Value, OpError> {
    match op {
        "search" => {
            let a: SearchArgs = args_of(args)?;
            out(kb.search(caller, &a.query, a.task_class.as_deref(), a.top_k.unwrap_or(DEFAULT_TOP_K)))
        }
        "get" => out(kb.get(caller, &args_of::<GetArgs>(args)?.id)),
        "list" => out(kb.list(caller, &args_of::<ListArgs>(args)?.path)),
        "add" => out(kb.add(caller, args_of::<NewEntry>(args)?)),
        "learn" => out(kb.learn(caller, args_of::<LearningRecord>(args)?)),
        "feedback" => {
            let a: FeedbackArgs = args_of(args)?;
            out(kb.feedback(caller, &a.id, a.verdict, a.note.as_deref()))
        }
        "check_anchors" => out(kb.check_anchors(caller)),
        _ => Err(("unknown_op".into(), format!("unknown operation: {op}"))),
    }
}

/// The RET-7 flow in four lines; the MCP `initialize` result carries it as `instructions`.
pub const FLOW: &str = "1. kb_search FIRST, with the task's own wording unchanged; reformulate only on later calls. Hits are paths + titles + snippets.\n\
2. kb_get only the narrowed candidates.\n\
3. kb_list a folder (start at \"\" for the root index) for \"everything about X\" or when search misses.\n\
4. kb_learn at the end of the task; kb_add kind \"page\" for a synthesis page linking its sources with derived-from.";

fn tool(op: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": format!("kb_{op}"),
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
    })
}

/// MCP tool name to operation, e.g. `kb_search` to `search`.
pub fn op_for_tool(name: &str) -> Option<&'static str> {
    OPS.into_iter().find(|op| name.strip_prefix("kb_") == Some(op))
}

/// MCP `tools/list` payload; schemas mirror the args [`dispatch`] accepts.
pub fn tools() -> Vec<Value> {
    let s = |d: &str| json!({ "type": "string", "description": d });
    let strs = |d: &str| json!({ "type": "array", "items": { "type": "string" }, "description": d });
    vec![
        tool(
            "search",
            "Search the shared knowledge base (conventions, glossary, learnings from earlier runs, synthesis pages). \
             Call this FIRST at the start of a task, with the task's own wording unchanged; reformulate only on later calls. \
             Returns ranked hits: path (id), title and snippet. Then kb_get only the few that look relevant.",
            json!({
                "query": s("Free-text query"),
                "task_class": s("Only entries of this task class (plus class-less ones), e.g. fix-ci"),
                "top_k": { "type": "integer", "minimum": 1, "description": "Max hits, default 5" },
            }),
            &["query"],
        ),
        tool(
            "get",
            "Read one knowledge entry in full by id. Read only the narrowed candidates from kb_search or kb_list; the id is its path, e.g. learnings/fix-ci/pin-the-rust-toolchain.",
            json!({ "id": s("Entry id: its path without .md") }),
            &["id"],
        ),
        tool(
            "list",
            "Browse one folder of the knowledge wiki: sub-folders with entry counts, and entries with title and summary. \
             Start at \"\" for the root index. Best for \"everything about X\" questions, or when kb_search misses.",
            json!({ "path": s("Folder relative to the root, e.g. learnings/fix-ci; empty or omitted for the root") }),
            &[],
        ),
        tool(
            "add",
            "Save a durable fact worth keeping: a convention, glossary term or note. For a synthesis page over several entries use kind \"page\" \
             and link each source with type derived-from. For lessons from a finished task use kb_learn instead.",
            json!({
                "title": s("Short, searchable title"),
                "body": s("Markdown body"),
                "kind": { "type": "string", "enum": ["note", "learning", "glossary", "convention", "page"] },
                "tags": strs("Tags"),
                "task_class": s("Task class this applies to; omit for all"),
                "source": s("Where it came from: URL, ticket, file"),
                "ttl_ms": { "type": "integer", "description": "Expire after this many milliseconds" },
                "links": { "type": "array", "items": { "type": "object", "properties": {
                    "type": { "type": "string", "enum": ["depends-on", "supersedes", "contradicts", "derived-from"] },
                    "target": s("Target entry id") }, "required": ["type", "target"] } },
                "anchors": { "type": "array", "items": { "type": "object", "properties": {
                    "repo": s("Repo path or name"), "path": s("File path"),
                    "lines": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2 },
                    "sha": s("Commit sha") }, "required": ["repo", "path", "lines", "sha"] } },
            }),
            &["title", "body"],
        ),
        tool(
            "learn",
            "Call at the END of a task: record what worked, what failed, dead ends and the shortest path, so later runs start ahead. \
             Repeated findings merge and gain weight; a failed or reverted outcome expires learnings it contradicts.",
            json!({
                "task_class": s("Kind of task, e.g. fix-ci"),
                "worked": strs("Things that worked"),
                "failed": strs("Things that failed"),
                "dead_ends": strs("Approaches not worth retrying"),
                "shortest_path": s("The shortest sequence of steps that would have solved it"),
                "outcome": { "type": "string", "enum": ["succeeded", "failed", "reverted"] },
                "steps": strs("Step IDs or pointers into the run"),
            }),
            &["task_class"],
        ),
        tool(
            "feedback",
            "Say whether an entry you used helped, misled, was wrong or stale. Wrong or stale entries are retired, so use it whenever an entry proved right or wrong.",
            json!({
                "id": s("Entry id"),
                "verdict": { "type": "string", "enum": ["helped", "misled", "wrong", "stale"] },
                "note": s("Why"),
            }),
            &["id", "verdict"],
        ),
        tool(
            "check_anchors",
            "Verify that code anchors in entries still resolve; broken ones are flagged and repaired ones restored. Takes no arguments.",
            json!({}),
            &[],
        ),
    ]
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use sf_kb::*;
    use std::sync::Mutex;

    /// Records `(op, detail)` per call; `get("missing")` is NotFound.
    #[derive(Default)]
    pub struct Fake(pub Mutex<Vec<(String, String)>>);

    fn entry(title: &str, id: &str, c: &Caller) -> Entry {
        Entry {
            meta: Meta {
                id: id.into(), kind: EntryKind::Note, title: title.into(), tags: vec![], task_class: None,
                links: vec![], anchors: vec![], source: "t".into(), author: c.clone(), created_at: 1, updated_at: 1,
                expires_at: None, status: EntryStatus::Active, outcome: None, count: 1, source_runs: vec![], key: None,
                helped: 0, misled: 0,
            },
            body: "b".into(),
        }
    }

    impl Fake {
        fn log(&self, op: &str, d: String) {
            self.0.lock().unwrap().push((op.into(), d));
        }
    }

    impl Knowledge for Fake {
        fn search(&self, c: &Caller, q: &str, tc: Option<&str>, k: usize) -> Result<Vec<Hit>> {
            self.log("search", format!("{}|{q}|{tc:?}|{k}", c.agent));
            Ok(vec![Hit { id: "notes/general/a".into(), title: "A".into(), score: 1.0, kind: EntryKind::Note, snippet: "s".into() }])
        }
        fn get(&self, c: &Caller, id: &str) -> Result<Entry> {
            self.log("get", id.into());
            if id == "missing" { Err(Error::NotFound(id.into())) } else { Ok(entry("T", id, c)) }
        }
        fn list(&self, _: &Caller, path: &str) -> Result<Listing> {
            self.log("list", path.into());
            Ok(Listing { path: path.into(), dirs: vec![DirItem { path: "notes".into(), entries: 2 }], entries: vec![] })
        }
        fn add(&self, c: &Caller, e: NewEntry) -> Result<Entry> {
            self.log("add", format!("{}|{:?}|{:?}|{:?}", e.title, e.kind, e.tags, e.ttl_ms));
            Ok(entry(&e.title, "new", c))
        }
        fn learn(&self, _: &Caller, r: LearningRecord) -> Result<Learned> {
            self.log("learn", format!("{}|{:?}|{:?}", r.task_class, r.worked, r.outcome));
            Ok(Learned { created: vec!["l1".into()], merged: vec![], expired: vec![] })
        }
        fn feedback(&self, c: &Caller, id: &str, v: Verdict, n: Option<&str>) -> Result<Entry> {
            self.log("feedback", format!("{id}|{v:?}|{n:?}"));
            Ok(entry("T", id, c))
        }
        fn check_anchors(&self, _: &Caller) -> Result<AnchorReport> {
            self.log("check_anchors", String::new());
            Ok(AnchorReport { checked: 3, broken: vec![], repaired: vec![], dangling_links: vec![], unreadable: vec![], stale_pages: vec![] })
        }
    }

    pub fn caller() -> Caller {
        Caller { agent: "pi".into(), session: "s".into() }
    }

    fn last(f: &Fake) -> String {
        f.0.lock().unwrap().last().unwrap().1.clone()
    }

    #[test]
    fn dispatch_every_op() {
        let (f, c) = (Fake::default(), caller());
        let r = dispatch(&f, &c, "search", json!({"query": "q", "task_class": "x"})).unwrap();
        assert_eq!(r[0]["id"], "notes/general/a");
        assert_eq!(last(&f), "pi|q|Some(\"x\")|5");
        dispatch(&f, &c, "search", json!({"query": "q", "top_k": 2})).unwrap();
        assert_eq!(last(&f), "pi|q|None|2");
        assert_eq!(dispatch(&f, &c, "get", json!({"id": "notes/general/e1"})).unwrap()["meta"]["id"], "notes/general/e1");
        assert_eq!(dispatch(&f, &c, "list", json!({"path": ""})).unwrap()["dirs"][0]["entries"], 2);
        assert_eq!(last(&f), "");
        dispatch(&f, &c, "list", json!({"path": "learnings/fix-ci"})).unwrap();
        assert_eq!(last(&f), "learnings/fix-ci");
        dispatch(&f, &c, "list", Value::Null).unwrap();
        assert_eq!(last(&f), "");
        let r = dispatch(&f, &c, "add", json!({"title": "T", "body": "B", "kind": "glossary", "tags": ["t"], "ttl_ms": 5})).unwrap();
        assert_eq!(r["meta"]["title"], "T");
        assert_eq!(last(&f), "T|Some(Glossary)|[\"t\"]|Some(5)");
        let r = dispatch(&f, &c, "learn", json!({"task_class": "fix", "worked": ["w"], "outcome": "failed"})).unwrap();
        assert_eq!(r["created"][0], "l1");
        assert_eq!(last(&f), "fix|[\"w\"]|Some(Failed)");
        dispatch(&f, &c, "feedback", json!({"id": "e1", "verdict": "stale", "note": "n"})).unwrap();
        assert_eq!(last(&f), "e1|Stale|Some(\"n\")");
        assert_eq!(dispatch(&f, &c, "check_anchors", Value::Null).unwrap()["checked"], 3);
        assert_eq!(f.0.lock().unwrap().len(), 10, "7 ops plus 3 extra searches/lists");
    }

    #[test]
    fn dispatch_errors_carry_codes() {
        let (f, c) = (Fake::default(), caller());
        assert_eq!(dispatch(&f, &c, "get", json!({"id": "missing"})).unwrap_err().0, "not_found");
        assert_eq!(dispatch(&f, &c, "get", json!({})).unwrap_err().0, "invalid");
        assert_eq!(dispatch(&f, &c, "feedback", json!({"id": "a", "verdict": "meh"})).unwrap_err().0, "invalid");
        assert_eq!(dispatch(&f, &c, "nope", json!({})).unwrap_err().0, "unknown_op");
    }

    #[test]
    fn tools_match_ops() {
        let t = tools();
        assert_eq!(t.len(), 7, "one tool per op");
        assert_eq!(t.len(), OPS.len());
        for (tool, op) in t.iter().zip(OPS) {
            assert_eq!(op_for_tool(tool["name"].as_str().unwrap()), Some(op));
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
        assert_eq!(op_for_tool("kb_nope"), None);
    }
}

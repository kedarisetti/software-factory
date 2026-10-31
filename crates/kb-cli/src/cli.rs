//! Hand-rolled argument parsing for `sf-kb` (no clap). Every subcommand becomes an op name plus
//! the same JSON args the MCP tools take (KIF-3).

use std::io;
use std::path::PathBuf;

use serde_json::{json, Map, Value};

pub struct Globals {
    pub root: PathBuf,
    /// `None` lets the MCP server fall back to `mcp:<client name>`; the CLI uses `human`.
    pub agent: Option<String>,
    pub session: String,
    pub contract: u32,
}

#[derive(Debug, PartialEq)]
pub enum Cmd {
    Op(&'static str, Value),
    Reindex,
    Version,
    Mcp,
}

pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;
pub type Stdin<'a> = &'a mut dyn FnMut() -> io::Result<String>;

const GLOBAL: [&str; 4] = ["root", "agent", "session", "contract"];

struct Flags(Vec<(String, String)>);

impl Flags {
    fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }
    fn all(&self, k: &str) -> Vec<&str> {
        self.0.iter().filter(|(n, _)| n == k).map(|(_, v)| v.as_str()).collect()
    }
    fn num<T: std::str::FromStr>(&self, k: &str) -> Result<Option<T>, String> {
        self.get(k).map(|v| v.parse().map_err(|_| format!("--{k} expects a number, got {v:?}"))).transpose()
    }
}

/// Split `args` into positionals and `--flag value` / `--flag=value` pairs.
fn split(args: &[String]) -> Result<(Vec<String>, Flags), String> {
    let (mut pos, mut flags, mut it) = (vec![], vec![], args.iter());
    while let Some(a) = it.next() {
        match a.strip_prefix("--") {
            Some(f) => match f.split_once('=') {
                Some((k, v)) => flags.push((k.to_string(), v.to_string())),
                None => flags.push((f.to_string(), it.next().ok_or(format!("--{f} needs a value"))?.clone())),
            },
            None => pos.push(a.clone()),
        }
    }
    Ok((pos, Flags(flags)))
}

fn json_arg(s: &str, what: &str) -> Result<Value, String> {
    serde_json::from_str(s).map_err(|e| format!("{what}: invalid JSON: {e}"))
}

/// Parse `args` (without the program name). `Err` is a usage error (exit code 2).
pub fn parse(args: &[String], env: Env, stdin: Stdin) -> Result<(Globals, Cmd), String> {
    let (pos, flags) = split(args)?;
    let sub = pos.first().ok_or("missing subcommand (search, get, list, add, learn, feedback, check-anchors, reindex, version, mcp)")?;
    let (allowed, arity): (&[&str], _) = match sub.as_str() {
        "search" => (&["task-class", "top-k"], 1..=usize::MAX),
        "get" => (&[], 1..=1),
        "list" => (&[], 0..=1),
        "add" => (&["title", "body", "body-file", "tag", "task-class", "kind", "source", "ttl-ms", "json"], 0..=0),
        "learn" => (&["json"], 0..=0),
        "feedback" => (&["note"], 2..=2),
        "check-anchors" | "reindex" | "version" | "mcp" => (&[], 0..=0),
        other => return Err(format!("unknown subcommand: {other}")),
    };
    if let Some((k, _)) = flags.0.iter().find(|(k, _)| !GLOBAL.contains(&k.as_str()) && !allowed.contains(&k.as_str())) {
        return Err(format!("unknown option --{k} for {sub}"));
    }
    let rest = &pos[1..];
    if !arity.contains(&rest.len()) {
        return Err(format!("wrong number of arguments for {sub}"));
    }
    let g = Globals {
        root: flags.get("root").map(str::to_string).or_else(|| env("SF_KB_ROOT")).unwrap_or_else(|| ".knowledge".into()).into(),
        agent: flags.get("agent").map(str::to_string).or_else(|| env("SF_KB_AGENT")),
        session: flags.get("session").map(str::to_string).or_else(|| env("SF_KB_SESSION")).unwrap_or_else(|| format!("cli-{}", std::process::id())),
        contract: flags.num("contract")?.unwrap_or(sf_kb::CONTRACT_VERSION),
    };
    let mut read_stdin = || stdin().map_err(|e| format!("reading stdin: {e}"));
    let cmd = match sub.as_str() {
        "search" => {
            let mut a = json!({ "query": rest.join(" ") });
            if let Some(c) = flags.get("task-class") { a["task_class"] = c.into() }
            if let Some(k) = flags.num::<usize>("top-k")? { a["top_k"] = k.into() }
            Cmd::Op("search", a)
        }
        "get" => Cmd::Op("get", json!({ "id": rest[0] })),
        "list" => Cmd::Op("list", json!({ "path": rest.first().map_or("", |p| p.trim_matches('/')) })),
        "add" => {
            let a = if let Some(j) = flags.get("json") {
                json_arg(j, "--json")?
            } else {
                let body = match (flags.get("body"), flags.get("body-file")) {
                    (Some(b), _) => b.to_string(),
                    (None, Some(f)) => std::fs::read_to_string(f).map_err(|e| format!("--body-file {f}: {e}"))?,
                    (None, None) => read_stdin()?,
                };
                let mut m = Map::new();
                m.insert("title".into(), flags.get("title").ok_or("add needs --title (or --json)")?.into());
                m.insert("body".into(), body.into());
                m.insert("tags".into(), flags.all("tag").into());
                for (flag, key) in [("task-class", "task_class"), ("kind", "kind"), ("source", "source")] {
                    if let Some(v) = flags.get(flag) { m.insert(key.into(), v.into()); }
                }
                if let Some(t) = flags.num::<u64>("ttl-ms")? { m.insert("ttl_ms".into(), t.into()); }
                Value::Object(m)
            };
            Cmd::Op("add", a)
        }
        "learn" => {
            let text = match flags.get("json") { Some(j) => j.to_string(), None => read_stdin()? };
            Cmd::Op("learn", json_arg(&text, "learning record")?)
        }
        "feedback" => {
            let mut a = json!({ "id": rest[0], "verdict": rest[1] });
            if let Some(n) = flags.get("note") { a["note"] = n.into() }
            Cmd::Op("feedback", a)
        }
        "check-anchors" => Cmd::Op("check_anchors", json!({})),
        "reindex" => Cmd::Reindex,
        "version" => Cmd::Version,
        _ => Cmd::Mcp,
    };
    Ok((g, cmd))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<(Globals, Cmd), String> {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse(&v, &|k| (k == "SF_KB_AGENT").then(|| "envagent".into()), &mut || Ok("from stdin".into()))
    }

    #[test]
    fn globals_and_defaults() {
        let (g, c) = p(&["--root", "/r", "--session=s9", "--contract", "1", "version"]).unwrap();
        assert_eq!((g.root.to_str(), g.session.as_str(), g.contract, g.agent.as_deref()), (Some("/r"), "s9", 1, Some("envagent")));
        assert_eq!(c, Cmd::Version);
        let (g, _) = p(&["version", "--agent", "pi"]).unwrap();
        assert_eq!((g.root.to_str(), g.agent.as_deref()), (Some(".knowledge"), Some("pi")));
        assert!(g.session.starts_with("cli-"));
    }

    #[test]
    fn search_and_get() {
        let (_, c) = p(&["search", "flaky", "ci", "--task-class", "fix", "--top-k", "3"]).unwrap();
        assert_eq!(c, Cmd::Op("search", json!({"query": "flaky ci", "task_class": "fix", "top_k": 3})));
        let (_, c) = p(&["get", "notes/general/e1"]).unwrap();
        assert_eq!(c, Cmd::Op("get", json!({"id": "notes/general/e1"})));
        assert_eq!(p(&["list"]).unwrap().1, Cmd::Op("list", json!({"path": ""})));
        assert_eq!(p(&["list", "learnings/fix-ci/"]).unwrap().1, Cmd::Op("list", json!({"path": "learnings/fix-ci"})));
    }

    #[test]
    fn add_variants() {
        let (_, c) = p(&["add", "--title", "T", "--body", "B", "--tag", "a", "--tag", "b", "--kind", "note", "--ttl-ms", "9"]).unwrap();
        assert_eq!(c, Cmd::Op("add", json!({"title": "T", "body": "B", "tags": ["a", "b"], "kind": "note", "ttl_ms": 9})));
        let (_, c) = p(&["add", "--title", "T"]).unwrap();
        assert_eq!(c, Cmd::Op("add", json!({"title": "T", "body": "from stdin", "tags": []})));
        let (_, c) = p(&["add", "--json", r#"{"title":"J","body":"x"}"#]).unwrap();
        assert_eq!(c, Cmd::Op("add", json!({"title": "J", "body": "x"})));
    }

    #[test]
    fn learn_feedback_misc() {
        assert!(p(&["learn"]).is_err(), "stdin is not JSON");
        let (_, c) = p(&["learn", "--json", r#"{"task_class":"x"}"#]).unwrap();
        assert_eq!(c, Cmd::Op("learn", json!({"task_class": "x"})));
        let (_, c) = p(&["feedback", "e1", "helped", "--note", "n"]).unwrap();
        assert_eq!(c, Cmd::Op("feedback", json!({"id": "e1", "verdict": "helped", "note": "n"})));
        assert_eq!(p(&["check-anchors"]).unwrap().1, Cmd::Op("check_anchors", json!({})));
        assert_eq!(p(&["reindex"]).unwrap().1, Cmd::Reindex);
        assert_eq!(p(&["mcp"]).unwrap().1, Cmd::Mcp);
    }

    #[test]
    fn usage_errors() {
        for bad in [&[][..], &["nope"], &["get"], &["get", "a", "b"], &["list", "a", "b"], &["feedback", "a"], &["search", "q", "--top-k", "x"],
                    &["search", "q", "--bogus", "1"], &["search", "q", "--top-k"], &["add", "--body", "b"], &["learn", "--json", "{"]] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
    }
}

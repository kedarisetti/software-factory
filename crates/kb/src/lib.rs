//! Knowledge service core (KIF-1). Runs without the orchestrator; any agent harness can use it.
//!
//! Source of truth is plain files (KIF-2) laid out as a wiki (WIKI-1): `<root>/<kind>/<topic>/<slug>.md`,
//! YAML frontmatter + Markdown body + a generated links footer (WIKI-4). An entry's ID is that path
//! without `.md`, e.g. `learnings/fix-ci/pin-the-rust-toolchain`.
//!
//! Generated from the entries, never hand-edited: `index.md` at the root and in every folder (WIKI-2),
//! `log.md` (WIKI-3). Written once, then human-owned: `AGENTS.md` (WIKI-6).
//! Everything under `<root>/.kb/` is cache or log and can be deleted:
//! `.kb/index/` (BM25, see [`Index`]), `.kb/lock` (writer lock), `.kb/calls.jsonl` (call log, KIF-5).
//!
//! The seven operations (KIF-4) are the [`Knowledge`] trait. [`Kb`] is the one implementation;
//! the CLI, the MCP server and Rust callers all go through it (KIF-3).

use serde::{Deserialize, Serialize};

mod kb;
pub use kb::Kb;

/// Contract version (KIF-8). Bump on any breaking change to the types or operations below.
/// Clients send the version they expect; a mismatch is `Error::Version`, never a silent best effort.
pub const CONTRACT_VERSION: u32 = 2;

/// Unix time in milliseconds.
pub type Timestamp = u64;

/// Entry IDs are the entry's path relative to the root, without `.md` (WIKI-1):
/// 2 or 3 `/`-separated segments of `[a-z0-9-]`, at most 160 chars, e.g. `notes/retrieval/writer-lock`.
pub type EntryId = String;

/// True if `id` is a well-formed entry ID. Front doors and the store reject anything else
/// (this is also the path-traversal guard).
pub fn valid_id(id: &str) -> bool {
    let segs: Vec<&str> = id.split('/').collect();
    id.len() <= 160
        && (2..=3).contains(&segs.len())
        && segs.iter().all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
}

/// Who is calling (KIF-5). Recorded on every operation and in every entry they write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caller {
    /// Agent harness, for example `pi`, `claude-code`, `codex`, `factory`, `human`.
    pub agent: String,
    /// Run ID (factory) or session ID (interactive harness).
    pub session: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    Note,
    /// Written by `learn` (LRN-2).
    Learning,
    Glossary,
    Convention,
    /// Synthesis page summarising a topic across entries (WIKI-5).
    Page,
}

impl EntryKind {
    /// Top-level folder for this kind (WIKI-1).
    pub fn dir(self) -> &'static str {
        match self {
            EntryKind::Note => "notes",
            EntryKind::Learning => "learnings",
            EntryKind::Glossary => "glossary",
            EntryKind::Convention => "conventions",
            EntryKind::Page => "pages",
        }
    }
}

/// Typed link (KNO-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkType {
    DependsOn,
    Supersedes,
    Contradicts,
    DerivedFrom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    #[serde(rename = "type")]
    pub kind: LinkType,
    pub target: EntryId,
}

/// Code reference pinned to a commit (KNO-2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// Path to a local git checkout, or a name resolved against `$SF_KB_REPOS/<name>`.
    pub repo: String,
    pub path: String,
    /// Inclusive, 1-based line range.
    pub lines: (u32, u32),
    pub sha: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryStatus {
    Active,
    /// TTL passed, or a contradicting outcome expired it (KNO-4, LRN-4).
    Expired,
    /// At least one anchor no longer resolves (KNO-2).
    BrokenAnchor,
    /// Synthesis page whose `derived-from` source expired, broke or was flagged (WIKI-8).
    Stale,
}

/// Run outcome a learning came from (LRN-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Succeeded,
    Failed,
    Reverted,
}

/// YAML frontmatter (KNO-1, KNO-3, KNO-4, KIF-2). Optional fields are omitted from the file when empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub id: EntryId,
    pub kind: EntryKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Retrieval filter (RET-3). Empty means "any task class".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_class: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anchors: Vec<Anchor>,
    /// Where the knowledge came from: URL, ticket, file, or `run:<id>`.
    pub source: String,
    /// The `Caller` that wrote it (KNO-1).
    pub author: Caller,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    pub status: EntryStatus,
    /// Learnings only: outcome of the run it came from (LRN-4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    /// Learnings only: how many runs reported the same finding (LRN-3). 1 for other kinds.
    #[serde(default = "one")]
    pub count: u32,
    /// Learnings only: every run or session that reported it (LRN-3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_runs: Vec<String>,
    /// Learnings only: normalized finding key used to merge repeats (LRN-3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Feedback tallies, updated by `feedback`.
    #[serde(default)]
    pub helped: u32,
    #[serde(default)]
    pub misled: u32,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub meta: Meta,
    /// Markdown body.
    pub body: String,
}

/// Input to `add`. The service fills id, author, timestamps, status and counters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewEntry {
    #[serde(default)]
    pub kind: Option<EntryKind>,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub task_class: Option<String>,
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub anchors: Vec<Anchor>,
    #[serde(default)]
    pub source: Option<String>,
    /// Time to live in milliseconds from now (KNO-4).
    #[serde(default)]
    pub ttl_ms: Option<u64>,
}

/// End-of-run learning record (LRN-1). Each non-empty item becomes, or merges into, one learning entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LearningRecord {
    pub task_class: String,
    #[serde(default)]
    pub worked: Vec<String>,
    #[serde(default)]
    pub failed: Vec<String>,
    #[serde(default)]
    pub dead_ends: Vec<String>,
    #[serde(default)]
    pub shortest_path: Option<String>,
    pub outcome: Option<Outcome>,
    /// Step IDs or other pointers into the run (LRN-1).
    #[serde(default)]
    pub steps: Vec<String>,
}

/// What `learn` did with each finding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Learned {
    pub created: Vec<EntryId>,
    /// Existing entries whose count went up (LRN-3).
    pub merged: Vec<EntryId>,
    /// Existing learnings this outcome contradicted and expired (LRN-4).
    pub expired: Vec<EntryId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Helped,
    Misled,
    Wrong,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub id: EntryId,
    pub title: String,
    pub score: f32,
    pub kind: EntryKind,
    /// First ~240 chars of the body.
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnchorProblem {
    pub id: EntryId,
    pub anchor: Anchor,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DanglingLink {
    pub from: EntryId,
    pub to: EntryId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnchorReport {
    pub checked: usize,
    pub broken: Vec<AnchorProblem>,
    /// Entries that were BrokenAnchor and now resolve again.
    pub repaired: Vec<EntryId>,
    /// Links whose target does not exist (WIKI-7).
    #[serde(default)]
    pub dangling_links: Vec<DanglingLink>,
    /// Entry files that do not parse, skipped everywhere else (WIKI-9).
    #[serde(default)]
    pub unreadable: Vec<Unreadable>,
    /// Pages marked `Stale` by this check (WIKI-8).
    #[serde(default)]
    pub stale_pages: Vec<EntryId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unreadable {
    /// Path relative to the root.
    pub path: String,
    pub reason: String,
}

/// One folder of the wiki, as returned by `list` (WIKI-2, RET-7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    /// The folder listed, relative to the root ("" for the root).
    pub path: String,
    pub dirs: Vec<DirItem>,
    pub entries: Vec<ListItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirItem {
    /// Path relative to the root, e.g. `learnings/fix-ci`.
    pub path: String,
    /// Entries anywhere below it.
    pub entries: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListItem {
    pub id: EntryId,
    pub title: String,
    pub kind: EntryKind,
    pub status: EntryStatus,
    /// First sentence or line of the body, at most ~120 chars.
    pub summary: String,
    pub count: u32,
    pub updated_at: Timestamp,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse {path}: {msg}")]
    Parse { path: String, msg: String },
    #[error("not found: {0}")]
    NotFound(EntryId),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("index: {0}")]
    Index(String),
    #[error("contract version mismatch: client expects {expected}, service is {actual}")]
    Version { expected: u32, actual: u32 },
}

impl Error {
    /// Stable machine-readable code for the CLI and MCP error payloads.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Io(_) => "io",
            Error::Parse { .. } => "parse",
            Error::NotFound(_) => "not_found",
            Error::Invalid(_) => "invalid",
            Error::Index(_) => "index",
            Error::Version { .. } => "version",
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The seven operations (KIF-4). Every call takes the `Caller` and is logged (KIF-5).
/// Retrieval flow (RET-7): `search` first with the task's own wording, then `get` or `list`
/// on the narrowed candidates.
pub trait Knowledge {
    /// BM25 search (RET-1). Only `Active`, unexpired entries are returned (RET-2);
    /// `task_class` keeps entries of that class plus class-less ones (RET-3).
    /// Logged with the query, returned IDs and scores (RET-4).
    fn search(&self, caller: &Caller, query: &str, task_class: Option<&str>, top_k: usize) -> Result<Vec<Hit>>;
    fn get(&self, caller: &Caller, id: &str) -> Result<Entry>;
    /// Browse one folder (`""` for the root): subfolders with entry counts, and the entries directly
    /// in it, including inactive ones (their status is shown). `NotFound` if the folder does not exist.
    fn list(&self, caller: &Caller, path: &str) -> Result<Listing>;
    fn add(&self, caller: &Caller, entry: NewEntry) -> Result<Entry>;
    /// LRN-1 to LRN-4.
    fn learn(&self, caller: &Caller, record: LearningRecord) -> Result<Learned>;
    /// FBK-2 for knowledge entries: bumps tallies; `Wrong` or `Stale` expires the entry.
    fn feedback(&self, caller: &Caller, id: &str, verdict: Verdict, note: Option<&str>) -> Result<Entry>;
    /// KNO-2: verify every anchor; mark broken ones `BrokenAnchor`, restore repaired ones to `Active`.
    /// Also reports dangling links and unreadable files (WIKI-7, WIKI-9), marks pages stale (WIKI-8),
    /// and rebuilds the index and wiki files (RET-5).
    fn check_anchors(&self, caller: &Caller) -> Result<AnchorReport>;
}

/// Search index over entries: a cache, always rebuildable from the files (KIF-2, RET-5).
/// Implemented by `sf-retrieval-tantivy`. Field weights: title and tags boosted over body (RET-1).
pub trait Index: Send + Sync {
    /// Insert or replace one entry. Must be searchable after it returns.
    fn upsert(&self, entry: &Entry) -> Result<()>;
    fn remove(&self, id: &str) -> Result<()>;
    /// Drop everything and index `entries` from scratch.
    fn rebuild(&self, entries: &mut dyn Iterator<Item = Entry>) -> Result<()>;
    /// Ranked `(id, score)`, already restricted to `Active` entries whose task class matches
    /// (or is unset). Expiry by time is the caller's job, so ask for a few extra.
    fn search(&self, query: &str, task_class: Option<&str>, top_k: usize) -> Result<Vec<(EntryId, f32)>>;
}

/// Start of the generated links footer (WIKI-4). Everything from here to the end of file is regenerated.
pub const LINKS_MARKER: &str = "<!-- kb:links (generated from frontmatter; edits here are overwritten) -->";

/// Parse an entry file: `---\n<yaml>\n---\n<body>[<links footer>]`. The footer is dropped;
/// links live in the frontmatter.
pub fn parse_entry(text: &str) -> std::result::Result<Entry, String> {
    let rest = text.strip_prefix("---\n").ok_or("missing opening ---")?;
    let end = rest.find("\n---\n").or_else(|| rest.strip_suffix("\n---").map(|r| r.len())).ok_or("missing closing ---")?;
    let meta: Meta = serde_yaml::from_str(&rest[..end]).map_err(|e| e.to_string())?;
    let mut body = rest[end..].strip_prefix("\n---").unwrap_or("").trim_start_matches('\n');
    if let Some(i) = body.find(LINKS_MARKER) {
        body = body[..i].strip_suffix("\n\n").unwrap_or(&body[..i]);
    }
    Ok(Entry { meta, body: body.to_string() })
}

/// Render an entry file, with a `[[wikilink]]` footer when it has links (WIKI-4).
/// `parse_entry(&render_entry(e)) == e` for any body that does not contain `LINKS_MARKER`.
pub fn render_entry(entry: &Entry) -> String {
    let yaml = serde_yaml::to_string(&entry.meta).expect("Meta always serializes");
    let mut out = format!("---\n{yaml}---\n\n{}", entry.body);
    if !entry.meta.links.is_empty() {
        out.push_str("\n\n");
        out.push_str(LINKS_MARKER);
        out.push_str("\n## Links\n\n");
        for l in &entry.meta.links {
            let kind = serde_yaml::to_string(&l.kind).expect("LinkType serializes");
            out.push_str(&format!("- {}: [[{}]]\n", kind.trim(), l.target));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Entry {
        Entry {
            meta: Meta {
                id: "learnings/fix-ci/retry-flaky-ci-once".into(),
                kind: EntryKind::Learning,
                title: "Re-run flaky CI once before escalating".into(),
                tags: vec!["ci".into()],
                task_class: Some("fix-ci".into()),
                links: vec![Link { kind: LinkType::DerivedFrom, target: "notes/ci/flaky-jobs".into() }],
                anchors: vec![Anchor { repo: "factory".into(), path: "a.rs".into(), lines: (1, 3), sha: "abc".into() }],
                source: "run:r1".into(),
                author: Caller { agent: "pi".into(), session: "s1".into() },
                created_at: 1,
                updated_at: 2,
                expires_at: None,
                status: EntryStatus::Active,
                outcome: Some(Outcome::Succeeded),
                count: 2,
                source_runs: vec!["r1".into(), "r2".into()],
                key: Some("k".into()),
                helped: 0,
                misled: 0,
            },
            body: "Line one.\n\n---\n\nA horizontal rule must not end the frontmatter.\n".into(),
        }
    }

    #[test]
    fn entry_file_round_trips() {
        let e = sample();
        let text = render_entry(&e);
        assert!(text.starts_with("---\nid: learnings/fix-ci/retry-flaky-ci-once\n"));
        assert!(text.ends_with("- derived-from: [[notes/ci/flaky-jobs]]\n"), "{text}");
        assert_eq!(parse_entry(&text).unwrap(), e);
    }

    #[test]
    fn ids_are_paths() {
        assert!(valid_id("learnings/fix-ci/pin-the-toolchain"));
        assert!(valid_id("glossary/idempotency-key"));
        for bad in ["notes", "notes/../x", "/notes/a", "notes/a/", "Notes/a", "a/b/c/d", "notes/a.md"] {
            assert!(!valid_id(bad), "{bad}");
        }
    }
}

/// Naive in-memory `Index` (term overlap, no BM25) for tests and for running without a cache.
pub mod scan {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use crate::*;

    #[derive(Default)]
    pub struct ScanIndex(Mutex<HashMap<EntryId, Entry>>);

    impl Index for ScanIndex {
        fn upsert(&self, entry: &Entry) -> Result<()> {
            self.0.lock().unwrap().insert(entry.meta.id.clone(), entry.clone());
            Ok(())
        }
        fn remove(&self, id: &str) -> Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }
        fn rebuild(&self, entries: &mut dyn Iterator<Item = Entry>) -> Result<()> {
            let mut map = self.0.lock().unwrap();
            map.clear();
            map.extend(entries.map(|e| (e.meta.id.clone(), e)));
            Ok(())
        }
        fn search(&self, query: &str, task_class: Option<&str>, top_k: usize) -> Result<Vec<(EntryId, f32)>> {
            let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
            let map = self.0.lock().unwrap();
            let mut hits: Vec<(EntryId, f32)> = map
                .values()
                .filter(|e| e.meta.status == EntryStatus::Active)
                .filter(|e| match (task_class, &e.meta.task_class) {
                    (Some(want), Some(have)) => want == have,
                    _ => true,
                })
                .filter_map(|e| {
                    let text = format!("{} {} {}", e.meta.title, e.meta.tags.join(" "), e.body).to_lowercase();
                    let score = terms.iter().filter(|t| text.contains(t.as_str())).count() as f32;
                    (score > 0.0).then(|| (e.meta.id.clone(), score))
                })
                .collect();
            hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            hits.truncate(top_k);
            Ok(hits)
        }
    }
}

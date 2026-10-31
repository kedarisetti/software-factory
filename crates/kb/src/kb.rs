//! `Kb`: the file-backed implementation of `Knowledge` (KIF-1 to KIF-7) laid out as a wiki (WIKI-1 to WIKI-7).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::*;

#[path = "wiki.rs"]
mod wiki;

/// Clock in unix milliseconds; injectable so TTL tests need no sleeping.
pub type Now = fn() -> Timestamp;

pub struct Kb {
    root: PathBuf,
    index: Box<dyn Index>,
    now: Now,
}

const KINDS: [EntryKind; 5] = [EntryKind::Note, EntryKind::Learning, EntryKind::Glossary, EntryKind::Convention, EntryKind::Page];

fn system_now() -> Timestamp {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// LRN-3: hash of category + task class + normalized text.
fn finding_key(cat: &str, task_class: &str, norm: &str) -> String {
    let h = Sha256::digest(format!("{cat}|{}|{norm}", task_class.to_lowercase()).as_bytes());
    h[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn kind_str(k: EntryKind) -> &'static str {
    match k {
        EntryKind::Note => "note",
        EntryKind::Learning => "learning",
        EntryKind::Glossary => "glossary",
        EntryKind::Convention => "convention",
        EntryKind::Page => "page",
    }
}

/// Failed and reverted runs are one side of a contradiction (LRN-4); succeeded (or unknown) the other.
fn negative(o: Option<Outcome>) -> bool {
    matches!(o, Some(Outcome::Failed | Outcome::Reverted))
}

/// Parse a file; the path is the truth for the ID (WIKI-1).
fn parse_at(text: &str, path: &Path, id: &str) -> Result<Entry> {
    let mut e = parse_entry(text).map_err(|msg| Error::Parse { path: path.display().to_string(), msg })?;
    e.meta.id = id.to_string();
    Ok(e)
}

/// Entry files below `dir` (a kind folder, at most one topic level down), as `(id, path)`.
fn walk(dir: &Path, rel: &str, depth: usize, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for ent in rd {
        let path = ent?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let child = format!("{rel}/{name}");
        if name.starts_with('.') {
            continue;
        } else if path.is_dir() {
            if depth < 2 {
                walk(&path, &child, depth + 1, out)?;
            }
        } else if let Some(stem) = child.strip_suffix(".md").filter(|s| !s.ends_with("/index") && valid_id(s)) {
            out.push((stem.to_string(), path));
        }
    }
    Ok(())
}

impl Kb {
    /// Open (creating if needed) the store at `root`: migrates a v1 `entries/` folder, writes `AGENTS.md`
    /// if missing (WIKI-6) and a root `index.md` if missing. Does not rebuild the index unless it migrated;
    /// call `rebuild_index` when the cache is new or suspect.
    pub fn open(root: impl Into<PathBuf>, index: Box<dyn Index>) -> Result<Kb> {
        let root = root.into();
        fs::create_dir_all(root.join(".kb").join("index"))?;
        let kb = Kb { root, index, now: system_now };
        let _lock = kb.lock()?;
        kb.migrate_v1()?;
        let agents = kb.root.join("AGENTS.md");
        if !agents.exists() {
            wiki::write_atomic(&agents, wiki::AGENTS_MD.as_bytes())?;
        }
        if !kb.root.join("index.md").exists() {
            kb.regen()?;
        }
        Ok(kb)
    }

    /// Replace the clock (tests).
    pub fn with_now(mut self, now: Now) -> Kb {
        self.now = now;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Re-index every entry file (RET-5 nightly rebuild, or a fresh cache) and regenerate the `index.md` files.
    /// A file that does not parse is an `Error::Parse` naming it.
    pub fn rebuild_index(&self) -> Result<usize> {
        let _lock = self.lock()?;
        let entries = self.load_all()?;
        self.regen_with(&entries)?;
        let n = entries.len();
        self.index.rebuild(&mut entries.into_iter())?;
        Ok(n)
    }

    /// KIF-8 handshake for front doors.
    pub fn check_version(expected: u32) -> Result<()> {
        if expected == CONTRACT_VERSION {
            Ok(())
        } else {
            Err(Error::Version { expected, actual: CONTRACT_VERSION })
        }
    }

    fn entry_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.md"))
    }

    /// Exclusive cross-process writer lock (KIF-7); released when the file is dropped.
    fn lock(&self) -> Result<File> {
        let f = OpenOptions::new().create(true).truncate(false).write(true).open(self.root.join(".kb").join("lock"))?;
        f.lock()?;
        Ok(f)
    }

    fn load(&self, id: &str) -> Result<Entry> {
        if !valid_id(id) {
            return Err(Error::Invalid(format!("bad entry id {id:?}")));
        }
        let path = self.entry_path(id);
        match fs::read_to_string(&path) {
            Ok(_) if id.ends_with("/index") => Err(Error::NotFound(id.to_string())),
            Ok(text) => parse_at(&text, &path, id),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::NotFound(id.to_string())),
            Err(e) => Err(e.into()),
        }
    }

    /// Every entry, skipping `index.md`, `log.md`, `AGENTS.md` and temp files (they are not under a kind folder or are named so).
    fn load_all(&self) -> Result<Vec<Entry>> {
        let mut files = vec![];
        for k in KINDS {
            walk(&self.root.join(k.dir()), k.dir(), 1, &mut files)?;
        }
        files.sort();
        files.iter().map(|(id, path)| parse_at(&fs::read_to_string(path)?, path, id)).collect()
    }

    /// Atomic write (temp file, fsync, rename) then index upsert. Caller holds the lock (KIF-7).
    fn store(&self, entry: &Entry) -> Result<()> {
        let path = self.entry_path(&entry.meta.id);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        wiki::write_atomic(&path, render_entry(entry).as_bytes())?;
        self.index.upsert(entry)
    }

    /// WIKI-2: rewrite every `index.md` whose text changed.
    fn regen_with(&self, entries: &[Entry]) -> Result<()> {
        for (dir, text) in wiki::render_indexes(entries) {
            let path = self.root.join(&dir).join("index.md");
            if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
                wiki::write_atomic(&path, text.as_bytes())?;
            }
        }
        Ok(())
    }

    fn regen(&self) -> Result<()> {
        self.regen_with(&self.load_all()?)
    }

    /// WIKI-3: append one line to `log.md`. Caller holds the lock.
    fn change(&self, caller: &Caller, op: &str, id: &str, detail: &str) -> Result<()> {
        let path = self.root.join("log.md");
        let fresh = !path.exists();
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        let line = wiki::log_line((self.now)(), &caller.agent, op, id, detail);
        f.write_all(format!("{}{line}", if fresh { wiki::LOG_HEADER } else { "" }).as_bytes())?;
        Ok(())
    }

    /// First free ID for `<dir>/<slug>`; a taken one gets `-2`, `-3`, ... (WIKI-1, never overwrite).
    fn unique_id(&self, dir: &str, slug: &str, taken: &HashSet<String>) -> String {
        let base = format!("{dir}/{slug}");
        (1..)
            .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
            .find(|id| !taken.contains(id) && !self.entry_path(id).exists())
            .expect("unbounded range")
    }

    /// Entries in one folder (not recursive), used to find a learning to merge into.
    fn load_dir(&self, dir: &str) -> Result<Vec<Entry>> {
        let mut files = vec![];
        walk(&self.root.join(dir), dir, 2, &mut files)?;
        files.iter().map(|(id, path)| parse_at(&fs::read_to_string(path)?, path, id)).collect()
    }

    /// Move a v1 `<root>/entries/<id>.md` store into the wiki layout, rewriting links to the new IDs.
    fn migrate_v1(&self) -> Result<()> {
        let old_dir = self.root.join("entries");
        if !old_dir.is_dir() {
            return Ok(());
        }
        let mut old = vec![];
        for ent in fs::read_dir(&old_dir)? {
            let path = ent?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || !name.ends_with(".md") {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            old.push((path.clone(), parse_entry(&text).map_err(|msg| Error::Parse { path: path.display().to_string(), msg })?));
        }
        old.sort_by(|a, b| (a.1.meta.created_at, &a.1.meta.id).cmp(&(b.1.meta.created_at, &b.1.meta.id)));
        let (mut taken, mut map) = (HashSet::new(), HashMap::new());
        for (_, e) in &old {
            let m = &e.meta;
            let id = self.unique_id(&wiki::dir_of(m.kind, m.task_class.as_deref(), &m.tags), &wiki::entry_slug(&m.title), &taken);
            taken.insert(id.clone());
            map.insert(m.id.clone(), id);
        }
        let who = Caller { agent: "migration".into(), session: "v1".into() };
        for (path, mut e) in old {
            let old_id = std::mem::take(&mut e.meta.id);
            e.meta.id = map[&old_id].clone();
            for l in &mut e.meta.links {
                if let Some(n) = map.get(&l.target) {
                    l.target = n.clone();
                }
            }
            if e.meta.kind == EntryKind::Learning && e.meta.key.is_none() {
                let cat = e.meta.tags.first().cloned().unwrap_or_default();
                e.meta.key = Some(finding_key(&cat, e.meta.task_class.as_deref().unwrap_or(""), &normalize(&e.meta.title)));
            }
            self.store(&e)?;
            self.change(&who, "migrate", &e.meta.id, &format!("from entries/{old_id}"))?;
            fs::remove_file(path)?;
            self.index.remove(&old_id)?;
        }
        let _ = fs::remove_dir(&old_dir);
        self.regen()
    }

    /// Append one JSON line to the call log (KIF-5, RET-4). Logging never fails the operation.
    fn log<T>(&self, caller: &Caller, op: &str, args: Value, r: &Result<T>, summary: impl Fn(&T) -> Value) {
        let (ok, result, error) = match r {
            Ok(v) => (true, summary(v), Value::Null),
            Err(e) => (false, Value::Null, json!(e.code())),
        };
        let line = json!({"ts": (self.now)(), "agent": caller.agent, "session": caller.session, "op": op,
            "args": args, "result": result, "ok": ok, "error": error});
        let path = self.root.join(".kb").join("calls.jsonl");
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(format!("{line}\n").as_bytes());
        }
    }

    fn do_search(&self, query: &str, task_class: Option<&str>, top_k: usize) -> Result<Vec<(Hit, f32)>> {
        if top_k == 0 {
            return Err(Error::Invalid("top_k must be at least 1".into()));
        }
        let now = (self.now)();
        let mut hits = vec![];
        for (id, score) in self.index.search(query, task_class, top_k * 2 + 5)? {
            let e = match self.load(&id) {
                Ok(e) => e,
                Err(Error::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            let m = &e.meta;
            let class_ok = match (task_class, &m.task_class) {
                (Some(want), Some(have)) => want == have,
                _ => true,
            };
            if m.status != EntryStatus::Active || m.expires_at.is_some_and(|t| t <= now) || !class_ok {
                continue;
            }
            let snippet: String = e.body.chars().take(240).collect();
            hits.push((Hit { id, title: m.title.clone(), score, kind: m.kind, snippet }, score));
            if hits.len() == top_k {
                break;
            }
        }
        Ok(hits)
    }

    /// KIF-4 `list` (RET-7): subfolders with recursive counts, and the entries directly in `path`.
    fn do_list(&self, path: &str) -> Result<Listing> {
        let path = path.trim_matches('/');
        let segs: Vec<&str> = if path.is_empty() { vec![] } else { path.split('/').collect() };
        let ok_seg = |s: &&str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if segs.len() > 2 || !segs.iter().all(ok_seg) {
            return Err(Error::Invalid(format!("bad folder path {path:?}")));
        }
        if segs.first().is_some_and(|d| !KINDS.iter().any(|k| k.dir() == *d) || !self.root.join(path).is_dir()) {
            return Err(Error::NotFound(path.to_string()));
        }
        let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
        let mut dirs: HashMap<String, usize> = HashMap::new();
        let mut items = vec![];
        for e in self.load_all()? {
            let Some(rest) = e.meta.id.strip_prefix(&prefix) else { continue };
            match rest.split_once('/') {
                Some((d, _)) => *dirs.entry(format!("{prefix}{d}")).or_default() += 1,
                None => items.push(ListItem {
                    id: e.meta.id.clone(),
                    title: e.meta.title.clone(),
                    kind: e.meta.kind,
                    status: e.meta.status,
                    summary: wiki::summary(&e.body),
                    count: e.meta.count,
                    updated_at: e.meta.updated_at,
                }),
            }
        }
        items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
        let mut dirs: Vec<DirItem> = dirs.into_iter().map(|(path, entries)| DirItem { path, entries }).collect();
        dirs.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Listing { path: path.to_string(), dirs, entries: items })
    }

    fn do_add(&self, caller: &Caller, new: NewEntry) -> Result<Entry> {
        let title = new.title.trim().to_string();
        if title.is_empty() {
            return Err(Error::Invalid("title must not be empty".into()));
        }
        let now = (self.now)();
        let kind = new.kind.unwrap_or(EntryKind::Note);
        let _lock = self.lock()?;
        let dir = wiki::dir_of(kind, new.task_class.as_deref(), &new.tags);
        let meta = Meta {
            id: self.unique_id(&dir, &wiki::entry_slug(&title), &HashSet::new()),
            kind,
            title,
            tags: new.tags,
            task_class: new.task_class,
            links: new.links,
            anchors: new.anchors,
            source: new.source.unwrap_or_else(|| format!("agent:{}/{}", caller.agent, caller.session)),
            author: caller.clone(),
            created_at: now,
            updated_at: now,
            expires_at: new.ttl_ms.map(|t| now.saturating_add(t)),
            status: EntryStatus::Active,
            outcome: None,
            count: 1,
            source_runs: vec![],
            key: None,
            helped: 0,
            misled: 0,
        };
        let entry = Entry { meta, body: new.body };
        self.store(&entry)?;
        self.change(caller, "add", &entry.meta.id, kind_str(kind))?;
        self.regen()?;
        Ok(entry)
    }

    fn do_learn(&self, caller: &Caller, rec: LearningRecord) -> Result<Learned> {
        let now = (self.now)();
        let tc = rec.task_class.trim().to_string();
        let neg = negative(rec.outcome);
        let mut items: Vec<(&str, &str)> = vec![];
        for (cat, list) in [("worked", &rec.worked), ("failed", &rec.failed), ("dead-end", &rec.dead_ends)] {
            items.extend(list.iter().map(|s| (cat, s.as_str())));
        }
        items.extend(rec.shortest_path.as_deref().map(|s| ("shortest-path", s)));
        let steps = if rec.steps.is_empty() { String::new() } else { format!("\n\nSteps: {}\n", rec.steps.join(", ")) };
        let mut out = Learned { created: vec![], merged: vec![], expired: vec![] };
        let dir = wiki::dir_of(EntryKind::Learning, Some(&tc), &[]);
        let _lock = self.lock()?;
        for (cat, text) in items {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let key = finding_key(cat, &tc, &normalize(text));
            // LRN-3/4: find the Active learning with this key in the topic folder; same side merges,
            // the other side is contradicted: expired and linked from the new entry.
            let (mut existing, mut contradicted) = (None, None);
            for mut old in self.load_dir(&dir)? {
                if old.meta.key.as_deref() != Some(&key) || old.meta.status != EntryStatus::Active {
                    continue;
                }
                if negative(old.meta.outcome) == neg {
                    existing = Some(old);
                } else {
                    old.meta.status = EntryStatus::Expired;
                    old.meta.updated_at = now;
                    self.store(&old)?;
                    self.change(caller, "expire", &old.meta.id, "contradicted by a later outcome")?;
                    out.expired.push(old.meta.id.clone());
                    contradicted = Some(Link { kind: LinkType::Contradicts, target: old.meta.id.clone() });
                }
            }
            let merging = existing.is_some();
            let mut e = match existing {
                Some(mut e) => {
                    e.meta.count += 1;
                    e.meta.updated_at = now;
                    if !e.meta.source_runs.contains(&caller.session) {
                        e.meta.source_runs.push(caller.session.clone());
                    }
                    e
                }
                None => {
                    let title: String = text.chars().take(200).collect();
                    Entry {
                        meta: Meta {
                            id: self.unique_id(&dir, &wiki::entry_slug(&title), &HashSet::new()),
                            kind: EntryKind::Learning,
                            title,
                            tags: vec![cat.to_string(), tc.clone()],
                            task_class: Some(tc.clone()),
                            links: vec![],
                            anchors: vec![],
                            source: format!("run:{}", caller.session),
                            author: caller.clone(),
                            created_at: now,
                            updated_at: now,
                            expires_at: None,
                            status: EntryStatus::Active,
                            outcome: rec.outcome,
                            count: 1,
                            source_runs: vec![caller.session.clone()],
                            key: Some(key),
                            helped: 0,
                            misled: 0,
                        },
                        body: format!("{text}\n{steps}"),
                    }
                }
            };
            if let Some(l) = contradicted.filter(|l| !e.meta.links.contains(l)) {
                e.meta.links.push(l);
            }
            self.store(&e)?;
            let id = e.meta.id.clone();
            self.change(caller, "learn", &id, &if merging { format!("merged, x{}", e.meta.count) } else { "created".into() })?;
            if merging { out.merged.push(id) } else { out.created.push(id) }
        }
        self.regen()?;
        Ok(out)
    }

    fn do_feedback(&self, caller: &Caller, id: &str, verdict: Verdict) -> Result<Entry> {
        if !valid_id(id) {
            return Err(Error::Invalid(format!("bad entry id {id:?}")));
        }
        let _lock = self.lock()?;
        let mut e = self.load(id)?;
        match verdict {
            Verdict::Helped => e.meta.helped += 1,
            Verdict::Misled => e.meta.misled += 1,
            Verdict::Wrong | Verdict::Stale => {
                if verdict == Verdict::Wrong {
                    e.meta.misled += 1;
                }
                e.meta.status = EntryStatus::Expired;
            }
        }
        e.meta.updated_at = (self.now)();
        self.store(&e)?;
        self.change(caller, "feedback", id, &format!("{verdict:?}").to_lowercase())?;
        self.regen()?;
        Ok(e)
    }

    fn do_check_anchors(&self, caller: &Caller) -> Result<AnchorReport> {
        let _lock = self.lock()?;
        let now = (self.now)();
        let mut report = AnchorReport { checked: 0, broken: vec![], repaired: vec![], dangling_links: vec![] };
        let entries = self.load_all()?;
        let ids: HashSet<&str> = entries.iter().map(|e| e.meta.id.as_str()).collect();
        for e in &entries {
            for l in e.meta.links.iter().filter(|l| !ids.contains(l.target.as_str())) {
                report.dangling_links.push(DanglingLink { from: e.meta.id.clone(), to: l.target.clone() });
            }
        }
        let mut changed = false;
        for mut e in entries.clone() {
            if e.meta.anchors.is_empty() {
                continue;
            }
            let mut any_broken = false;
            for a in &e.meta.anchors {
                report.checked += 1;
                if let Err(reason) = anchor_resolves(a) {
                    any_broken = true;
                    report.broken.push(AnchorProblem { id: e.meta.id.clone(), anchor: a.clone(), reason });
                }
            }
            let (new_status, op) = match (e.meta.status, any_broken) {
                (EntryStatus::Expired, _) => continue,
                (EntryStatus::Active, true) => (EntryStatus::BrokenAnchor, "broken-anchor"),
                (EntryStatus::BrokenAnchor, false) => {
                    report.repaired.push(e.meta.id.clone());
                    (EntryStatus::Active, "repaired")
                }
                _ => continue,
            };
            e.meta.status = new_status;
            e.meta.updated_at = now;
            self.store(&e)?;
            self.change(caller, op, &e.meta.id, "check_anchors")?;
            changed = true;
        }
        if changed {
            self.regen()?;
        }
        Ok(report)
    }
}

/// KNO-2: does `<sha>:<path>` exist in the repo and have at least `lines.1` lines?
fn anchor_resolves(a: &Anchor) -> std::result::Result<(), String> {
    if a.sha.starts_with('-') || a.path.starts_with('-') || a.sha.is_empty() {
        return Err("invalid sha or path".into());
    }
    let p = Path::new(&a.repo);
    let repo = if p.is_absolute() || p.exists() {
        p.to_path_buf()
    } else {
        match std::env::var_os("SF_KB_REPOS") {
            Some(base) => Path::new(&base).join(&a.repo),
            None => return Err(format!("repo {:?} not found and SF_KB_REPOS unset", a.repo)),
        }
    };
    let spec = format!("{}:{}", a.sha, a.path);
    let git = |args: &[&str]| Command::new("git").arg("-C").arg(&repo).args(args).arg(&spec).output();
    if !git(&["cat-file", "-e"]).map_err(|e| format!("git: {e}"))?.status.success() {
        return Err(format!("{spec} not found"));
    }
    let shown = git(&["show"]).map_err(|e| format!("git: {e}"))?;
    if !shown.status.success() {
        return Err(format!("git show {spec} failed"));
    }
    let n = String::from_utf8_lossy(&shown.stdout).lines().count();
    if n < a.lines.1 as usize {
        return Err(format!("{spec} has {n} lines, anchor needs {}", a.lines.1));
    }
    Ok(())
}

impl Knowledge for Kb {
    fn search(&self, caller: &Caller, query: &str, task_class: Option<&str>, top_k: usize) -> Result<Vec<Hit>> {
        let r = self.do_search(query, task_class, top_k);
        let args = json!({"query": query, "task_class": task_class, "top_k": top_k});
        self.log(caller, "search", args, &r, |h| h.iter().map(|(h, s)| json!({"id": h.id, "score": s})).collect());
        r.map(|v| v.into_iter().map(|(h, _)| h).collect())
    }
    fn get(&self, caller: &Caller, id: &str) -> Result<Entry> {
        let r = self.load(id);
        self.log(caller, "get", json!({"id": id}), &r, |e| json!({"id": e.meta.id}));
        r
    }
    fn list(&self, caller: &Caller, path: &str) -> Result<Listing> {
        let r = self.do_list(path);
        self.log(caller, "list", json!({"path": path}), &r, |l| json!({"dirs": l.dirs.len(), "entries": l.entries.len()}));
        r
    }
    fn add(&self, caller: &Caller, entry: NewEntry) -> Result<Entry> {
        let args = json!({"title": entry.title, "kind": entry.kind, "tags": entry.tags, "task_class": entry.task_class});
        let r = self.do_add(caller, entry);
        self.log(caller, "add", args, &r, |e| json!({"id": e.meta.id}));
        r
    }
    fn learn(&self, caller: &Caller, record: LearningRecord) -> Result<Learned> {
        let args = json!({"task_class": record.task_class, "outcome": record.outcome,
            "items": record.worked.len() + record.failed.len() + record.dead_ends.len() + record.shortest_path.iter().count()});
        let r = self.do_learn(caller, record);
        self.log(caller, "learn", args, &r, |l| json!(l));
        r
    }
    fn feedback(&self, caller: &Caller, id: &str, verdict: Verdict, note: Option<&str>) -> Result<Entry> {
        let r = self.do_feedback(caller, id, verdict);
        self.log(caller, "feedback", json!({"id": id, "verdict": verdict, "note": note}), &r, |e| {
            json!({"id": e.meta.id, "status": e.meta.status, "helped": e.meta.helped, "misled": e.meta.misled})
        });
        r
    }
    fn check_anchors(&self, caller: &Caller) -> Result<AnchorReport> {
        let r = self.do_check_anchors(caller);
        self.log(caller, "check_anchors", json!({}), &r, |a| {
            json!({"checked": a.checked, "broken": a.broken.iter().map(|b| &b.id).collect::<HashSet<_>>(), "repaired": a.repaired})
        });
        r
    }
}


#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::scan::ScanIndex;

    fn who(agent: &str, session: &str) -> Caller {
        Caller { agent: agent.into(), session: session.into() }
    }

    fn open(root: &Path) -> Kb {
        Kb::open(root, Box::new(ScanIndex::default())).unwrap()
    }

    fn note(title: &str, body: &str) -> NewEntry {
        NewEntry { title: title.into(), body: body.into(), ..Default::default() }
    }

    #[test]
    fn add_get_round_trip_and_invalid_id() {
        let dir = tempfile::tempdir().unwrap();
        let kb = open(dir.path());
        let c = who("pi", "s1");
        let e = kb.add(&c, NewEntry { ttl_ms: Some(10), ..note("Use cargo nextest", "Faster.") }).unwrap();
        assert_eq!(e.meta.id, "notes/general/use-cargo-nextest");
        assert_eq!((e.meta.author.clone(), e.meta.source.as_str(), e.meta.count), (c.clone(), "agent:pi/s1", 1));
        assert_eq!(e.meta.expires_at, Some(e.meta.created_at + 10));
        assert_eq!(kb.get(&c, &e.meta.id).unwrap(), e);
        for bad in ["../x", "A", "", "nope", "notes/a.md", &"a".repeat(161)] {
            assert!(matches!(kb.get(&c, bad), Err(Error::Invalid(_))), "{bad}");
        }
        assert!(matches!(kb.get(&c, "notes/general/nope"), Err(Error::NotFound(_))));
        assert!(matches!(kb.get(&c, "notes/general/index"), Err(Error::NotFound(_))));
        assert!(matches!(kb.add(&c, note("  ", "x")), Err(Error::Invalid(_))));
    }

    #[test]
    fn search_filters_expired_broken_and_class() {
        let dir = tempfile::tempdir().unwrap();
        let c = who("pi", "s1");
        let kb = open(dir.path()).with_now(|| 1_000);
        let ttl = kb.add(&c, NewEntry { ttl_ms: Some(500), ..note("zebra ttl", "zebra") }).unwrap();
        let keep = kb.add(&c, note("zebra keep", "zebra")).unwrap();
        let other = kb.add(&c, NewEntry { task_class: Some("other".into()), ..note("zebra other", "zebra") }).unwrap();
        let mine = kb.add(&c, NewEntry { task_class: Some("fix-ci".into()), ..note("zebra mine", "zebra") }).unwrap();
        let ids = |h: Vec<Hit>| h.into_iter().map(|h| h.id).collect::<HashSet<_>>();
        assert_eq!(ids(kb.search(&c, "zebra", Some("fix-ci"), 10).unwrap()), [keep.meta.id.clone(), mine.meta.id.clone(), ttl.meta.id.clone()].into());
        // Later clock: the TTL entry expires. Fresh Kb (empty index) needs a rebuild.
        let later = open(dir.path()).with_now(|| 2_000);
        assert_eq!(later.rebuild_index().unwrap(), 4);
        assert_eq!(ids(later.search(&c, "zebra", Some("fix-ci"), 10).unwrap()), [keep.meta.id.clone(), mine.meta.id.clone()].into());
        assert_eq!(later.search(&c, "zebra", None, 1).unwrap().len(), 1);
        assert!(matches!(later.search(&c, "zebra", None, 0), Err(Error::Invalid(_))));
        // BrokenAnchor entries are dropped even if the index still lists them.
        let mut broken = later.get(&c, &keep.meta.id).unwrap();
        broken.meta.status = EntryStatus::BrokenAnchor;
        fs::write(later.entry_path(&keep.meta.id), render_entry(&broken)).unwrap();
        assert_eq!(ids(later.search(&c, "zebra", Some("fix-ci"), 10).unwrap()), [mine.meta.id].into());
        let _ = other;
    }

    fn rec(outcome: Outcome, worked: &[&str]) -> LearningRecord {
        LearningRecord {
            task_class: "fix-ci".into(),
            worked: worked.iter().map(|s| s.to_string()).collect(),
            outcome: Some(outcome),
            ..Default::default()
        }
    }

    #[test]
    fn learn_merges_duplicates_across_callers() {
        let dir = tempfile::tempdir().unwrap();
        let kb = open(dir.path());
        let a = kb.learn(&who("pi", "r1"), rec(Outcome::Succeeded, &["Re-run  flaky CI once"])).unwrap();
        let b = kb.learn(&who("claude-code", "r2"), rec(Outcome::Succeeded, &["re-run flaky ci once"])).unwrap();
        assert_eq!((a.created.len(), b.created.len(), b.merged.clone()), (1, 0, a.created.clone()));
        let e = kb.get(&who("pi", "r1"), &a.created[0]).unwrap();
        assert_eq!((e.meta.count, e.meta.source_runs.clone()), (2, vec!["r1".to_string(), "r2".to_string()]));
        assert_eq!(a.created[0], "learnings/fix-ci/re-run-flaky-ci-once");
        assert!(e.meta.key.is_some());
        assert_eq!(e.meta.task_class.as_deref(), Some("fix-ci"));
        assert_eq!(e.meta.source, "run:r1");
        assert!(e.meta.updated_at >= e.meta.created_at);
    }

    #[test]
    fn contradiction_expires_old_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let kb = open(dir.path());
        let c = who("pi", "r1");
        let old = kb.learn(&c, rec(Outcome::Succeeded, &["bump the timeout"])).unwrap().created[0].clone();
        let l = kb.learn(&who("pi", "r2"), rec(Outcome::Reverted, &["bump the timeout"])).unwrap();
        assert_eq!((l.expired.clone(), l.merged.len(), l.created.len()), (vec![old.clone()], 0, 1));
        assert_eq!(kb.get(&c, &old).unwrap().meta.status, EntryStatus::Expired);
        let new = kb.get(&c, &l.created[0]).unwrap();
        assert_eq!(new.meta.outcome, Some(Outcome::Reverted));
        assert_eq!(new.meta.links, vec![Link { kind: LinkType::Contradicts, target: old }]);
        // Same-side repeat merges into the new entry, not the expired one.
        assert_eq!(kb.learn(&c, rec(Outcome::Failed, &["bump the timeout"])).unwrap().merged, l.created);
    }

    #[test]
    fn feedback_tallies_and_stale_expires() {
        let dir = tempfile::tempdir().unwrap();
        let kb = open(dir.path());
        let c = who("pi", "s1");
        let id = kb.add(&c, note("Thing", "body")).unwrap().meta.id;
        assert_eq!(kb.feedback(&c, &id, Verdict::Helped, None).unwrap().meta.helped, 1);
        assert_eq!(kb.feedback(&c, &id, Verdict::Misled, Some("hm")).unwrap().meta.misled, 1);
        assert_eq!(kb.feedback(&c, &id, Verdict::Stale, None).unwrap().meta.status, EntryStatus::Expired);
        assert!(kb.search(&c, "thing", None, 5).unwrap().is_empty());
    }

    #[test]
    fn calls_log_has_one_line_per_op() {
        let dir = tempfile::tempdir().unwrap();
        let kb = open(dir.path());
        let c = who("claude-code", "s9");
        let id = kb.add(&c, note("Logged", "body text")).unwrap().meta.id;
        kb.search(&c, "logged", None, 3).unwrap();
        kb.get(&c, &id).unwrap();
        let _ = kb.get(&c, "../bad");
        kb.learn(&c, rec(Outcome::Succeeded, &["x"])).unwrap();
        kb.feedback(&c, &id, Verdict::Helped, None).unwrap();
        kb.check_anchors(&c).unwrap();
        let text = fs::read_to_string(dir.path().join(".kb/calls.jsonl")).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let ops: Vec<_> = lines.iter().map(|l| l["op"].as_str().unwrap()).collect();
        assert_eq!(ops, ["add", "search", "get", "get", "learn", "feedback", "check_anchors"]);
        assert!(lines.iter().all(|l| l["agent"] == "claude-code" && l["session"] == "s9"));
        assert_eq!(lines[1]["result"][0]["id"], json!(id));
        assert_eq!((lines[3]["ok"].clone(), lines[3]["error"].clone()), (json!(false), json!("invalid")));
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-C"])
            .arg(dir)
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn check_anchors_flags_and_repairs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        fs::write(repo.path().join("a.rs"), "1\n2\n3\n").unwrap();
        git(repo.path(), &["add", "a.rs"]);
        git(repo.path(), &["commit", "-q", "-m", "init"]);
        let sha = Command::new("git").arg("-C").arg(repo.path()).args(["rev-parse", "HEAD"]).output().unwrap();
        let sha = String::from_utf8(sha.stdout).unwrap().trim().to_string();
        let anchor = |path: &str, end: u32| Anchor { repo: repo.path().display().to_string(), path: path.into(), lines: (1, end), sha: sha.clone() };

        let kb = open(dir.path());
        let c = who("factory", "n1");
        let ok = kb.add(&c, NewEntry { anchors: vec![anchor("a.rs", 3)], ..note("ok anchor", "ok") }).unwrap().meta.id;
        let bad = kb.add(&c, NewEntry { anchors: vec![anchor("missing.rs", 1)], ..note("bad anchor", "bad") }).unwrap().meta.id;
        let short = kb.add(&c, NewEntry { anchors: vec![anchor("a.rs", 9)], ..note("short anchor", "s") }).unwrap().meta.id;
        let r = kb.check_anchors(&c).unwrap();
        assert_eq!((r.checked, r.broken.len(), r.repaired.len()), (3, 2, 0));
        let status = |id: &str| kb.get(&c, id).unwrap().meta.status;
        assert_eq!((status(&ok), status(&bad), status(&short)), (EntryStatus::Active, EntryStatus::BrokenAnchor, EntryStatus::BrokenAnchor));
        assert!(kb.search(&c, "anchor", None, 10).unwrap().iter().all(|h| h.id == ok));

        // Repair: add the missing file in a new commit and re-anchor via the file (sha pinned to the new commit).
        fs::write(repo.path().join("missing.rs"), "x\n").unwrap();
        git(repo.path(), &["add", "missing.rs"]);
        git(repo.path(), &["commit", "-q", "-m", "more"]);
        let sha2 = Command::new("git").arg("-C").arg(repo.path()).args(["rev-parse", "HEAD"]).output().unwrap();
        let sha2 = String::from_utf8(sha2.stdout).unwrap().trim().to_string();
        let mut e = kb.get(&c, &bad).unwrap();
        e.meta.anchors[0].sha = sha2;
        fs::write(kb.entry_path(&bad), render_entry(&e)).unwrap();
        let r = kb.check_anchors(&c).unwrap();
        assert_eq!((r.broken.len(), r.repaired.clone()), (1, vec![bad.clone()]));
        assert_eq!(status(&bad), EntryStatus::Active);
    }

    #[test]
    fn concurrent_writers_two_instances() {
        let dir = tempfile::tempdir().unwrap();
        let kbs = [Arc::new(open(dir.path())), Arc::new(open(dir.path()))];
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let kb = kbs[t % 2].clone();
                std::thread::spawn(move || {
                    for i in 0..10 {
                        kb.add(&who("pi", &format!("t{t}")), note(&format!("entry {t}-{i}"), "body")).unwrap();
                    }
                })
            })
            .collect();
        handles.into_iter().for_each(|h| h.join().unwrap());
        let fresh = open(dir.path());
        assert_eq!(fresh.rebuild_index().unwrap(), 80);
        assert_eq!(fs::read_to_string(dir.path().join("log.md")).unwrap().lines().filter(|l| l.starts_with("- ")).count(), 80);
        let tmp = fs::read_dir(dir.path().join("notes/general")).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".tmp-")).count();
        assert_eq!(tmp, 0);
    }

    const T0: Timestamp = 1_000_000_000_000;

    fn read(dir: &tempfile::TempDir, rel: &str) -> String {
        fs::read_to_string(dir.path().join(rel)).unwrap()
    }

    #[test]
    fn paths_are_descriptive_and_collisions_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let kb = open(dir.path());
        let c = who("pi", "r1");
        let l = kb.learn(&c, rec(Outcome::Succeeded, &["Pin the Rust toolchain"])).unwrap();
        assert_eq!(l.created, ["learnings/fix-ci/pin-the-rust-toolchain"]);
        assert!(dir.path().join("learnings/fix-ci/pin-the-rust-toolchain.md").is_file());
        let tagged = kb.add(&c, NewEntry { tags: vec!["Rust CI".into()], ..note("Cache", "b") }).unwrap();
        assert_eq!(tagged.meta.id, "notes/rust-ci/cache");
        let long = kb.add(&c, note(&"word ".repeat(30), "b")).unwrap();
        assert!(long.meta.id.len() <= "notes/general/".len() + 60 && long.meta.id.ends_with("-word"), "{}", long.meta.id);
        let g = kb.add(&c, NewEntry { kind: Some(EntryKind::Glossary), ..note("Idempotency key", "b") }).unwrap();
        assert_eq!(g.meta.id, "glossary/idempotency-key");
        let (a, b, d) = (kb.add(&c, note("Same", "one")).unwrap(), kb.add(&c, note("Same", "two")).unwrap(), kb.add(&c, note("Same", "three")).unwrap());
        assert_eq!((a.meta.id.as_str(), b.meta.id.as_str(), d.meta.id.as_str()), ("notes/general/same", "notes/general/same-2", "notes/general/same-3"));
        assert_eq!(kb.get(&c, &a.meta.id).unwrap().body, "one");
    }

    #[test]
    fn contradiction_collision_gets_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let kb = open(dir.path());
        let c = who("pi", "r1");
        let old = kb.learn(&c, rec(Outcome::Succeeded, &["bump the timeout"])).unwrap().created[0].clone();
        let new = kb.learn(&c, rec(Outcome::Failed, &["bump the timeout"])).unwrap().created[0].clone();
        assert_eq!((old.as_str(), new.as_str()), ("learnings/fix-ci/bump-the-timeout", "learnings/fix-ci/bump-the-timeout-2"));
    }

    #[test]
    fn index_md_is_generated_per_folder_and_tracks_status() {
        let dir = tempfile::tempdir().unwrap();
        let kb = open(dir.path()).with_now(|| T0);
        let c = who("pi", "r1");
        let id = kb.learn(&c, rec(Outcome::Succeeded, &["Pin the toolchain. Then rebuild."])).unwrap().created[0].clone();
        kb.learn(&who("pi", "r2"), rec(Outcome::Succeeded, &["Pin the toolchain. Then rebuild."])).unwrap();
        kb.add(&c, note("Writer lock", "Held for writes.")).unwrap();
        let root = read(&dir, "index.md");
        assert!(root.starts_with("# Knowledge base\n"), "{root}");
        for want in ["## Folders", "- [learnings/](learnings/index.md): 1 entry", "- [notes/](notes/index.md): 1 entry", "## Recently changed",
            "(learnings/fix-ci/pin-the-toolchain-then-rebuild.md)", "AGENTS.md", "log.md"] {
            assert!(root.contains(want), "{want} in {root}");
        }
        assert!(read(&dir, "learnings/index.md").contains("- [fix-ci/](fix-ci/index.md): 1 entry"));
        let folder = read(&dir, "learnings/fix-ci/index.md");
        assert!(folder.starts_with("# learnings/fix-ci\n") && folder.contains("## Entries"), "{folder}");
        assert!(folder.contains("(pin-the-toolchain-then-rebuild.md) · ×2"), "summary repeating the title is dropped: {folder}");
        kb.feedback(&c, &id, Verdict::Stale, None).unwrap();
        assert!(read(&dir, "learnings/fix-ci/index.md").contains("· expired"));
        assert!(read(&dir, "index.md").contains("· expired"));
    }

    #[test]
    fn log_md_has_one_line_per_mutation_and_none_per_read() {
        let dir = tempfile::tempdir().unwrap();
        let kb = open(dir.path()).with_now(|| T0);
        let c = who("pi", "r1");
        let id = kb.add(&c, note("Logged", "body")).unwrap().meta.id;
        kb.feedback(&c, &id, Verdict::Helped, None).unwrap();
        kb.search(&c, "logged", None, 3).unwrap();
        kb.get(&c, &id).unwrap();
        kb.list(&c, "").unwrap();
        let log = read(&dir, "log.md");
        let lines: Vec<&str> = log.lines().filter(|l| l.starts_with("- ")).collect();
        assert_eq!(lines, [
            "- 2001-09-09T01:46:40Z **pi** add [[notes/general/logged]] (note)",
            "- 2001-09-09T01:46:40Z **pi** feedback [[notes/general/logged]] (helped)",
        ]);
        let l = kb.learn(&c, rec(Outcome::Succeeded, &["x"])).unwrap();
        kb.learn(&c, rec(Outcome::Succeeded, &["x"])).unwrap();
        assert_eq!(l.created.len(), 1);
        assert_eq!(read(&dir, "log.md").lines().filter(|l| l.starts_with("- ")).count(), 4);
    }

    #[test]
    fn agents_md_is_created_once() {
        let dir = tempfile::tempdir().unwrap();
        let kb = open(dir.path());
        let text = read(&dir, "AGENTS.md");
        assert!(text.lines().count() < 40 && text.contains("search") && text.contains("derived-from"));
        assert!(dir.path().join("index.md").is_file());
        drop(kb);
        fs::write(dir.path().join("AGENTS.md"), "mine").unwrap();
        let kb = open(dir.path());
        kb.add(&who("pi", "s"), note("x", "y")).unwrap();
        assert_eq!(read(&dir, "AGENTS.md"), "mine");
    }

    #[test]
    fn list_root_folder_and_bad_paths() {
        let dir = tempfile::tempdir().unwrap();
        let kb = open(dir.path());
        let c = who("pi", "r1");
        kb.learn(&c, rec(Outcome::Succeeded, &["First finding. More text.", "Second finding"])).unwrap();
        kb.add(&c, note("Lock", &"é".repeat(300))).unwrap();
        let root = kb.list(&c, "").unwrap();
        let dirs: Vec<_> = root.dirs.iter().map(|d| (d.path.as_str(), d.entries)).collect();
        assert_eq!((root.path.as_str(), dirs, root.entries.len()), ("", vec![("learnings", 2), ("notes", 1)], 0));
        let f = kb.list(&c, "learnings/fix-ci").unwrap();
        assert!(f.dirs.is_empty());
        assert_eq!(f.entries.iter().map(|i| i.summary.as_str()).collect::<std::collections::BTreeSet<_>>(), ["First finding.", "Second finding"].into());
        assert_eq!(kb.list(&c, "learnings").unwrap().dirs, [DirItem { path: "learnings/fix-ci".into(), entries: 2 }]);
        let n = &kb.list(&c, "notes/general").unwrap().entries[0];
        assert_eq!((n.summary.chars().count(), n.id.as_str()), (120, "notes/general/lock"));
        for bad in ["../x", "a/b/c", "Notes", "notes//x"] {
            assert!(matches!(kb.list(&c, bad), Err(Error::Invalid(_))), "{bad}");
        }
        for missing in ["nope", "notes/none", ".kb"] {
            assert!(matches!(kb.list(&c, missing), Err(Error::NotFound(_) | Error::Invalid(_))), "{missing}");
        }
        assert!(matches!(kb.list(&c, "notes/none"), Err(Error::NotFound(_))));
        let last: Value = serde_json::from_str(read(&dir, ".kb/calls.jsonl").lines().last().unwrap()).unwrap();
        assert_eq!(last["op"], "list");
    }

    #[test]
    fn dangling_links_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let kb = open(dir.path());
        let c = who("pi", "r1");
        let src = kb.add(&c, note("Source", "s")).unwrap().meta.id;
        let link = |t: &str| Link { kind: LinkType::DerivedFrom, target: t.into() };
        let page = NewEntry { kind: Some(EntryKind::Page), links: vec![link(&src), link("notes/general/gone")], ..note("Summary", "p") };
        let p = kb.add(&c, page).unwrap().meta.id;
        let r = kb.check_anchors(&c).unwrap();
        assert_eq!(r.dangling_links, [DanglingLink { from: p, to: "notes/general/gone".into() }]);
    }

    #[test]
    fn page_with_derived_from_links_renders_footer_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let kb = open(dir.path());
        let c = who("pi", "r1");
        let src = kb.add(&c, note("Source", "s")).unwrap().meta.id;
        let links = vec![Link { kind: LinkType::DerivedFrom, target: src.clone() }];
        let p = kb.add(&c, NewEntry { kind: Some(EntryKind::Page), links, ..note("Retrieval overview", "Summary body.") }).unwrap();
        assert_eq!(p.meta.id, "pages/general/retrieval-overview");
        let text = read(&dir, "pages/general/retrieval-overview.md");
        assert!(text.contains(LINKS_MARKER) && text.ends_with("- derived-from: [[notes/general/source]]\n"), "{text}");
        assert_eq!(kb.get(&c, &p.meta.id).unwrap(), p);
        assert_eq!(parse_entry(&text).unwrap(), p);
    }

    #[test]
    fn v1_store_is_migrated() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("entries")).unwrap();
        let c = who("pi", "s1");
        let e = |id: &str, kind, title: &str, tc: Option<&str>, links: Vec<Link>, created: u64| Entry {
            meta: Meta {
                id: id.into(), kind, title: title.into(), tags: vec![], task_class: tc.map(String::from), links, anchors: vec![],
                source: "x".into(), author: c.clone(), created_at: created, updated_at: created, expires_at: None,
                status: EntryStatus::Active, outcome: None, count: 1, source_runs: vec![], key: None, helped: 0, misled: 0,
            },
            body: "Old body.".into(),
        };
        let a = e("note-lock-1234abcd", EntryKind::Note, "Writer lock", None, vec![], 1);
        let b = e("learning-pin-deadbeef", EntryKind::Learning, "Pin it", Some("fix-ci"), vec![Link { kind: LinkType::DependsOn, target: a.meta.id.clone() }], 2);
        for x in [&a, &b] {
            fs::write(dir.path().join(format!("entries/{}.md", x.meta.id)), render_entry(x)).unwrap();
        }
        let kb = open(dir.path());
        assert!(!dir.path().join("entries").exists());
        let moved = kb.get(&c, "learnings/fix-ci/pin-it").unwrap();
        assert_eq!(moved.meta.links[0].target, "notes/general/writer-lock");
        assert!(moved.meta.key.is_some());
        assert_eq!(kb.get(&c, "notes/general/writer-lock").unwrap().body, "Old body.");
        assert!(read(&dir, "index.md").contains("learnings/") && read(&dir, "log.md").contains("migrate [[notes/general/writer-lock]]"));
        assert!(kb.check_anchors(&c).unwrap().dangling_links.is_empty());
        assert_eq!(kb.rebuild_index().unwrap(), 2);
    }
}

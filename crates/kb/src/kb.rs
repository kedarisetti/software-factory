//! `Kb`: the file-backed implementation of `Knowledge` (KIF-1 to KIF-7).

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::*;

/// Clock in unix milliseconds; injectable so TTL tests need no sleeping.
pub type Now = fn() -> Timestamp;

pub struct Kb {
    root: PathBuf,
    index: Box<dyn Index>,
    now: Now,
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn system_now() -> Timestamp {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn hash8(s: &str) -> String {
    Sha256::digest(s.as_bytes())[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Lowercase alnum slug, words joined by `-`, at most `max` chars.
fn slug(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(max);
    out.trim_matches('-').to_string()
}

fn kind_str(k: EntryKind) -> &'static str {
    match k {
        EntryKind::Note => "note",
        EntryKind::Learning => "learning",
        EntryKind::Glossary => "glossary",
        EntryKind::Convention => "convention",
    }
}

fn join_id(kind: EntryKind, slug: &str, hash: &str) -> String {
    if slug.is_empty() { format!("{}-{hash}", kind_str(kind)) } else { format!("{}-{slug}-{hash}", kind_str(kind)) }
}

/// Path-traversal guard: IDs are `[a-z0-9-]{1,80}`.
fn valid_id(id: &str) -> bool {
    (1..=80).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Failed and reverted runs are one side of a contradiction (LRN-4); succeeded (or unknown) the other.
fn negative(o: Option<Outcome>) -> bool {
    matches!(o, Some(Outcome::Failed | Outcome::Reverted))
}

impl Kb {
    /// Open (creating if needed) the store at `root`. Does not rebuild the index;
    /// call `rebuild_index` when the cache is new or suspect.
    pub fn open(root: impl Into<PathBuf>, index: Box<dyn Index>) -> Result<Kb> {
        let root = root.into();
        fs::create_dir_all(root.join("entries"))?;
        fs::create_dir_all(root.join(".kb").join("index"))?;
        Ok(Kb { root, index, now: system_now })
    }

    /// Replace the clock (tests).
    pub fn with_now(mut self, now: Now) -> Kb {
        self.now = now;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Re-index every entry file (RET-5 nightly rebuild, or a fresh cache).
    /// A file that does not parse is an `Error::Parse` naming it.
    pub fn rebuild_index(&self) -> Result<usize> {
        let _lock = self.lock()?;
        let entries = self.load_all()?;
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
        self.root.join("entries").join(format!("{id}.md"))
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
            Ok(text) => parse_entry(&text).map_err(|msg| Error::Parse { path: path.display().to_string(), msg }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::NotFound(id.to_string())),
            Err(e) => Err(e.into()),
        }
    }

    fn load_all(&self) -> Result<Vec<Entry>> {
        let mut out = vec![];
        for ent in fs::read_dir(self.root.join("entries"))? {
            let path = ent?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || name.contains(".tmp-") || !name.ends_with(".md") {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            out.push(parse_entry(&text).map_err(|msg| Error::Parse { path: path.display().to_string(), msg })?);
        }
        Ok(out)
    }

    /// Atomic write (temp file, fsync, rename) then index upsert. Caller holds the lock (KIF-7).
    fn store(&self, entry: &Entry) -> Result<()> {
        let id = &entry.meta.id;
        let dir = self.root.join("entries");
        let tmp = dir.join(format!(".{id}.tmp-{}-{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
        let written = (|| {
            let mut f = File::create(&tmp)?;
            f.write_all(render_entry(entry).as_bytes())?;
            f.sync_all()?;
            fs::rename(&tmp, self.entry_path(id))
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        self.index.upsert(entry)
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

    fn do_add(&self, caller: &Caller, new: NewEntry) -> Result<Entry> {
        let title = new.title.trim().to_string();
        if title.is_empty() {
            return Err(Error::Invalid("title must not be empty".into()));
        }
        let now = (self.now)();
        let kind = new.kind.unwrap_or(EntryKind::Note);
        let h = hash8(&format!("{title}{}{now}{}", new.body, caller.agent));
        let meta = Meta {
            id: join_id(kind, &slug(&title, 40), &h),
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
            helped: 0,
            misled: 0,
        };
        let entry = Entry { meta, body: new.body };
        let _lock = self.lock()?;
        self.store(&entry)?;
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
        let _lock = self.lock()?;
        for (cat, text) in items {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let norm = text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
            let key = format!("{norm}|{cat}|{}", tc.to_lowercase());
            let id_for = |neg: bool| join_id(EntryKind::Learning, &slug(&norm, 40), &hash8(&format!("{key}|{}", if neg { "neg" } else { "pos" })));
            let (id, opp_id) = (id_for(neg), id_for(!neg));
            // LRN-4: a contradicting outcome expires the old learning and is linked from the new one.
            let mut contradicted = None;
            if let Ok(mut old) = self.load(&opp_id) {
                if old.meta.status == EntryStatus::Active {
                    old.meta.status = EntryStatus::Expired;
                    old.meta.updated_at = now;
                    self.store(&old)?;
                    out.expired.push(opp_id.clone());
                    contradicted = Some(Link { kind: LinkType::Contradicts, target: opp_id.clone() });
                }
            }
            let existing = self.load(&id).ok().filter(|e| e.meta.status == EntryStatus::Active);
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
                None => Entry {
                    meta: Meta {
                        id: id.clone(),
                        kind: EntryKind::Learning,
                        title: text.chars().take(200).collect(),
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
                        helped: 0,
                        misled: 0,
                    },
                    body: format!("{text}\n{steps}"),
                },
            };
            if let Some(l) = contradicted.filter(|l| !e.meta.links.contains(l)) {
                e.meta.links.push(l);
            }
            self.store(&e)?;
            if merging { out.merged.push(id) } else { out.created.push(id) }
        }
        Ok(out)
    }

    fn do_feedback(&self, id: &str, verdict: Verdict) -> Result<Entry> {
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
        Ok(e)
    }

    fn do_check_anchors(&self) -> Result<AnchorReport> {
        let _lock = self.lock()?;
        let now = (self.now)();
        let mut report = AnchorReport { checked: 0, broken: vec![], repaired: vec![] };
        for mut e in self.load_all()? {
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
            let new_status = match (e.meta.status, any_broken) {
                (EntryStatus::Expired, _) => continue,
                (EntryStatus::Active, true) => EntryStatus::BrokenAnchor,
                (EntryStatus::BrokenAnchor, false) => {
                    report.repaired.push(e.meta.id.clone());
                    EntryStatus::Active
                }
                _ => continue,
            };
            e.meta.status = new_status;
            e.meta.updated_at = now;
            self.store(&e)?;
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
        let r = self.do_feedback(id, verdict);
        self.log(caller, "feedback", json!({"id": id, "verdict": verdict, "note": note}), &r, |e| {
            json!({"id": e.meta.id, "status": e.meta.status, "helped": e.meta.helped, "misled": e.meta.misled})
        });
        r
    }
    fn check_anchors(&self, caller: &Caller) -> Result<AnchorReport> {
        let r = self.do_check_anchors();
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
        assert!(valid_id(&e.meta.id) && e.meta.id.starts_with("note-use-cargo-nextest-"));
        assert_eq!((e.meta.author.clone(), e.meta.source.as_str(), e.meta.count), (c.clone(), "agent:pi/s1", 1));
        assert_eq!(e.meta.expires_at, Some(e.meta.created_at + 10));
        assert_eq!(kb.get(&c, &e.meta.id).unwrap(), e);
        for bad in ["../x", "A", "", "a/b", &"a".repeat(81)] {
            assert!(matches!(kb.get(&c, bad), Err(Error::Invalid(_))), "{bad}");
        }
        assert!(matches!(kb.get(&c, "nope"), Err(Error::NotFound(_))));
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
        let tmp = fs::read_dir(dir.path().join("entries")).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".tmp-")).count();
        assert_eq!(tmp, 0);
    }
}

//! BM25 index over knowledge entries (RET-1, RET-5). Implements `sf_kb::Index`; a cache rebuilt from the entry files (KIF-2).
//!
//! Many processes may share one index dir. The kb layer's file lock serializes writers (KIF-7);
//! tantivy's writer lock is taken per write and released after commit, and readers reload before
//! each search so they see other processes' commits.

use std::path::Path;
use std::sync::Mutex;

use sf_kb::{Entry, EntryId, EntryStatus, Error, Index, Result};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value, STORED, STRING, TEXT};
use tantivy::{doc, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

/// RET-1 field weights: tags highest, then title, anchor paths, link targets, body.
pub const BOOST_TAGS: f32 = 3.0;
pub const BOOST_TITLE: f32 = 2.0;
pub const BOOST_ANCHORS: f32 = 1.5;
pub const BOOST_LINKS: f32 = 1.0;
pub const BOOST_BODY: f32 = 1.0;
/// Writer heap budget in bytes.
const WRITER_HEAP: usize = 30_000_000;

fn ix<E: std::fmt::Display>(e: E) -> Error {
    Error::Index(e.to_string())
}

#[derive(Clone, Copy)]
struct Fields {
    id: Field,
    title: Field,
    tags: Field,
    anchors: Field,
    links: Field,
    body: Field,
    task_class: Field,
    active: Field,
}

fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let f = Fields {
        id: b.add_text_field("id", STRING | STORED),
        title: b.add_text_field("title", TEXT),
        tags: b.add_text_field("tags", TEXT),
        anchors: b.add_text_field("anchors", TEXT),
        links: b.add_text_field("links", TEXT),
        body: b.add_text_field("body", TEXT),
        task_class: b.add_text_field("task_class", STRING),
        active: b.add_text_field("active", STRING),
    };
    (b.build(), f)
}

/// Tantivy-backed BM25 index (RET-1).
pub struct TantivyIndex {
    index: tantivy::Index,
    reader: IndexReader,
    /// Serializes writes within this process; tantivy's writer is created per write.
    writer: Mutex<()>,
    f: Fields,
}

impl TantivyIndex {
    /// Open or create an on-disk index. An incompatible or unreadable index is wiped and recreated (KIF-2: it is a cache).
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let (schema, f) = build_schema();
        std::fs::create_dir_all(dir)?;
        let existing = tantivy::Index::open_in_dir(dir).ok().filter(|i| i.schema() == schema);
        let index = match existing {
            Some(i) => i,
            None => {
                std::fs::remove_dir_all(dir)?;
                std::fs::create_dir_all(dir)?;
                tantivy::Index::create_in_dir(dir, schema).map_err(ix)?
            }
        };
        Self::new(index, f)
    }

    /// Purely in-memory index, for tests.
    pub fn in_memory() -> Result<Self> {
        let (schema, f) = build_schema();
        Self::new(tantivy::Index::create_in_ram(schema), f)
    }

    fn new(index: tantivy::Index, f: Fields) -> Result<Self> {
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into().map_err(ix)?;
        Ok(Self { index, reader, writer: Mutex::new(()), f })
    }

    /// Run `op` on a fresh writer, commit, release tantivy's writer lock, and reload so changes are
    /// searchable on return. The lock is held only for the write, so a long-lived process (the MCP
    /// server) never blocks writers in other processes (KIF-7).
    fn write(&self, op: impl FnOnce(&mut IndexWriter) -> Result<()>) -> Result<()> {
        let _guard = self.writer.lock().map_err(ix)?;
        let mut w: IndexWriter = self.index.writer(WRITER_HEAP).map_err(|e| {
            Error::Index(format!("cannot take the index writer lock (another process is writing?): {e}"))
        })?;
        let res = op(&mut w).and_then(|()| w.commit().map(drop).map_err(ix));
        if res.is_err() {
            let _ = w.rollback();
        }
        w.wait_merging_threads().map_err(ix)?;
        res?;
        self.reader.reload().map_err(ix)
    }

    fn document(&self, e: &Entry) -> TantivyDocument {
        let m = &e.meta;
        let join = |it: Vec<&str>| it.join(" ");
        doc!(
            self.f.id => m.id.as_str(),
            self.f.title => m.title.as_str(),
            self.f.tags => m.tags.join(" "),
            self.f.anchors => join(m.anchors.iter().map(|a| a.path.as_str()).collect()),
            self.f.links => join(m.links.iter().map(|l| l.target.as_str()).collect()),
            self.f.body => e.body.as_str(),
            self.f.task_class => m.task_class.as_deref().unwrap_or(""),
            self.f.active => if m.status == EntryStatus::Active { "1" } else { "0" },
        )
    }

    fn term_query(&self, field: Field, text: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(Term::from_field_text(field, text), IndexRecordOption::Basic))
    }
}

impl Index for TantivyIndex {
    /// Delete-then-add by ID, so repeated upserts never duplicate.
    fn upsert(&self, entry: &Entry) -> Result<()> {
        let doc = self.document(entry);
        self.write(|w| {
            w.delete_term(Term::from_field_text(self.f.id, &entry.meta.id));
            w.add_document(doc).map(drop).map_err(ix)
        })
    }

    fn remove(&self, id: &str) -> Result<()> {
        self.write(|w| {
            w.delete_term(Term::from_field_text(self.f.id, id));
            Ok(())
        })
    }

    fn rebuild(&self, entries: &mut dyn Iterator<Item = Entry>) -> Result<()> {
        let docs: Vec<_> = entries.map(|e| self.document(&e)).collect();
        self.write(|w| {
            w.delete_all_documents().map_err(ix)?;
            docs.into_iter().try_for_each(|d| w.add_document(d).map(drop).map_err(ix))
        })
    }

    /// RET-1 weighted BM25; RET-2 and RET-3 filters are query clauses, so `top_k` counts only eligible entries.
    fn search(&self, query: &str, task_class: Option<&str>, top_k: usize) -> Result<Vec<(EntryId, f32)>> {
        if top_k == 0 || query.trim().is_empty() {
            return Ok(vec![]);
        }
        let f = self.f;
        let mut parser = QueryParser::for_index(&self.index, vec![f.title, f.tags, f.body, f.anchors, f.links]);
        for (field, boost) in [
            (f.tags, BOOST_TAGS),
            (f.title, BOOST_TITLE),
            (f.anchors, BOOST_ANCHORS),
            (f.links, BOOST_LINKS),
            (f.body, BOOST_BODY),
        ] {
            parser.set_field_boost(field, boost);
        }
        // Lenient: malformed syntax degrades to a best-effort query instead of an error.
        let (text_query, _errors) = parser.parse_query_lenient(query);
        let mut clauses = vec![(Occur::Must, text_query), (Occur::Must, self.term_query(f.active, "1"))];
        if let Some(c) = task_class {
            let any = BooleanQuery::new(vec![
                (Occur::Should, self.term_query(f.task_class, c)),
                (Occur::Should, self.term_query(f.task_class, "")),
            ]);
            clauses.push((Occur::Must, Box::new(any)));
        }
        // Pick up commits from other processes (Pi CLI, MCP server) before reading.
        self.reader.reload().map_err(ix)?;
        let searcher = self.reader.searcher();
        let top = searcher.search(&BooleanQuery::new(clauses), &TopDocs::with_limit(top_k)).map_err(ix)?;
        top.into_iter()
            .map(|(score, addr)| {
                let d: TantivyDocument = searcher.doc(addr).map_err(ix)?;
                let id = d.get_first(f.id).and_then(|v| v.as_str()).ok_or_else(|| ix("stored id missing"))?;
                Ok((id.to_string(), score))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sf_kb::{Anchor, Caller, EntryKind, Meta};

    fn entry(id: &str, title: &str, tags: &[&str], body: &str) -> Entry {
        Entry {
            meta: Meta {
                id: id.into(),
                kind: EntryKind::Note,
                title: title.into(),
                tags: tags.iter().map(|t| t.to_string()).collect(),
                task_class: None,
                links: vec![],
                anchors: vec![],
                source: "test".into(),
                author: Caller { agent: "t".into(), session: "s".into() },
                created_at: 0,
                updated_at: 0,
                expires_at: None,
                status: EntryStatus::Active,
                outcome: None,
                count: 1,
                source_runs: vec![],
                helped: 0,
                misled: 0,
            },
            body: body.into(),
        }
    }

    fn ids(r: Vec<(EntryId, f32)>) -> Vec<String> {
        r.into_iter().map(|(i, _)| i).collect()
    }

    #[test]
    fn upsert_then_search() {
        let ix = TantivyIndex::in_memory().unwrap();
        ix.upsert(&entry("a", "Cache", &[], "invalidate the cache nightly")).unwrap();
        assert_eq!(ids(ix.search("cache", None, 5).unwrap()), ["a"]);
        assert!(ix.search("nothing", None, 5).unwrap().is_empty());
    }

    #[test]
    fn tag_outranks_body() {
        let ix = TantivyIndex::in_memory().unwrap();
        ix.upsert(&entry("body", "x", &[], "kubernetes")).unwrap();
        ix.upsert(&entry("tag", "y", &["kubernetes"], "other words")).unwrap();
        assert_eq!(ids(ix.search("kubernetes", None, 5).unwrap()), ["tag", "body"]);
    }

    #[test]
    fn anchors_and_links_are_indexed() {
        let ix = TantivyIndex::in_memory().unwrap();
        let mut e = entry("a", "t", &[], "b");
        e.meta.anchors.push(Anchor { repo: "r".into(), path: "widget".into(), lines: (1, 2), sha: "s".into() });
        e.meta.links.push(sf_kb::Link { kind: sf_kb::LinkType::DependsOn, target: "gadget".into() });
        ix.upsert(&e).unwrap();
        assert_eq!(ids(ix.search("widget", None, 5).unwrap()), ["a"]);
        assert_eq!(ids(ix.search("gadget", None, 5).unwrap()), ["a"]);
    }

    #[test]
    fn task_class_filter() {
        let ix = TantivyIndex::in_memory().unwrap();
        let mut a = entry("a", "t", &[], "word");
        a.meta.task_class = Some("fix".into());
        let mut b = entry("b", "t", &[], "word");
        b.meta.task_class = Some("review".into());
        let c = entry("c", "t", &[], "word");
        [&a, &b, &c].iter().for_each(|e| ix.upsert(e).unwrap());
        let mut got = ids(ix.search("word", Some("fix"), 10).unwrap());
        got.sort();
        assert_eq!(got, ["a", "c"]);
        assert_eq!(ix.search("word", None, 10).unwrap().len(), 3);
    }

    #[test]
    fn inactive_never_returned() {
        let ix = TantivyIndex::in_memory().unwrap();
        for (id, st) in [("e", EntryStatus::Expired), ("b", EntryStatus::BrokenAnchor), ("ok", EntryStatus::Active)] {
            let mut e = entry(id, "t", &[], "word");
            e.meta.status = st;
            ix.upsert(&e).unwrap();
        }
        assert_eq!(ids(ix.search("word", None, 1000).unwrap()), ["ok"]);
    }

    #[test]
    fn upsert_twice_no_duplicate() {
        let ix = TantivyIndex::in_memory().unwrap();
        ix.upsert(&entry("a", "t", &[], "old")).unwrap();
        ix.upsert(&entry("a", "t", &[], "new new")).unwrap();
        assert_eq!(ids(ix.search("new OR old", None, 10).unwrap()), ["a"]);
        assert!(ix.search("old", None, 10).unwrap().is_empty());
    }

    #[test]
    fn remove_entry() {
        let ix = TantivyIndex::in_memory().unwrap();
        ix.upsert(&entry("a", "t", &[], "word")).unwrap();
        ix.remove("a").unwrap();
        ix.remove("missing").unwrap();
        assert!(ix.search("word", None, 10).unwrap().is_empty());
    }

    #[test]
    fn rebuild_replaces_everything() {
        let ix = TantivyIndex::in_memory().unwrap();
        ix.upsert(&entry("old", "t", &[], "word")).unwrap();
        let mut it = vec![entry("n1", "t", &[], "word"), entry("n2", "t", &[], "word")].into_iter();
        ix.rebuild(&mut it).unwrap();
        let mut got = ids(ix.search("word", None, 10).unwrap());
        got.sort();
        assert_eq!(got, ["n1", "n2"]);
    }

    #[test]
    fn on_disk_reopen_persists() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("idx");
        {
            let ix = TantivyIndex::open(&p).unwrap();
            ix.upsert(&entry("a", "t", &[], "persisted")).unwrap();
        }
        let ix = TantivyIndex::open(&p).unwrap();
        assert_eq!(ids(ix.search("persisted", None, 5).unwrap()), ["a"]);
        ix.upsert(&entry("b", "t", &[], "persisted")).unwrap();
        assert_eq!(ix.search("persisted", None, 5).unwrap().len(), 2);
    }

    #[test]
    fn incompatible_schema_is_wiped() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = Schema::builder();
        b.add_text_field("other", TEXT);
        tantivy::Index::create_in_dir(dir.path(), b.build()).unwrap();
        let ix = TantivyIndex::open(dir.path()).unwrap();
        ix.upsert(&entry("a", "t", &[], "word")).unwrap();
        assert_eq!(ix.search("word", None, 5).unwrap().len(), 1);
    }

    #[test]
    fn two_instances_share_writes() {
        // Models the MCP server and a CLI call on one store: neither keeps the writer lock,
        // and each sees the other's commits (KIF-7).
        let dir = tempfile::tempdir().unwrap();
        let a = TantivyIndex::open(dir.path()).unwrap();
        let b = TantivyIndex::open(dir.path()).unwrap();
        a.upsert(&entry("a", "t", &[], "word")).unwrap();
        b.upsert(&entry("b", "t", &[], "word")).unwrap();
        for ix in [&a, &b] {
            let mut ids: Vec<_> = ix.search("word", None, 10).unwrap().into_iter().map(|h| h.0).collect();
            ids.sort();
            assert_eq!(ids, ["a", "b"]);
        }
    }

    #[test]
    fn garbage_queries_do_not_error() {
        let ix = TantivyIndex::in_memory().unwrap();
        ix.upsert(&entry("a", "t", &[], "bar")).unwrap();
        for q in ["foo:(bar AND", "\"unclosed", "(((", "a:b:c", "AND OR", "", "  ", "\\", "[1 TO"] {
            ix.search(q, None, 5).unwrap();
        }
    }

    #[test]
    fn top_k_honoured_with_filters() {
        let ix = TantivyIndex::in_memory().unwrap();
        for i in 0..20 {
            let mut e = entry(&format!("x{i}"), "t", &["word"], "word word word");
            e.meta.status = EntryStatus::Expired;
            ix.upsert(&e).unwrap();
        }
        for i in 0..3 {
            ix.upsert(&entry(&format!("a{i}"), "t", &[], "word")).unwrap();
        }
        let got = ids(ix.search("word", None, 3).unwrap());
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|i| i.starts_with('a')));
    }
}

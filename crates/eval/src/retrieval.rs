//! Retrieval suite (RET-6): recall@5, recall@10, MRR and searches per question over a nested ladder of store sizes.
//! The fixture store is rung 1x; larger rungs add seeded distractors (see `ladder`). Questions and relevant
//! entries stay fixed across rungs, as in [1].

use std::path::{Path, PathBuf};

use serde::Deserialize;
use sf_kb::{Caller, Entry, Kb, Knowledge};
use sf_retrieval_tantivy::TantivyIndex;

use crate::ladder::{Distractors, Vocab};
use crate::metrics::{mean, recall_at_k, reciprocal_rank, round6, Metrics};
use crate::{ctx, read_jsonl, Result};

/// Store sizes as multiples of the fixture. 30x is about 2.6k entries.
pub const LADDER: [usize; 3] = [1, 10, 30];
pub const SEED: u64 = 0x5EED_601D;
const K: usize = 10;

#[derive(Debug, Clone, Deserialize)]
pub struct Question {
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub task_class: Option<String>,
    /// Entry IDs a human marked relevant.
    pub relevant: Vec<String>,
}

pub fn load_questions(golden: &Path) -> Result<Vec<Question>> {
    let path = golden.join("cases/retrieval/questions.jsonl");
    let qs: Vec<Question> = read_jsonl(&path)?
        .into_iter()
        .map(|v| ctx(serde_json::from_value(v), path.display()))
        .collect::<Result<_>>()?;
    if qs.iter().any(|q| q.relevant.is_empty()) {
        return Err("a question has no relevant entries".into());
    }
    Ok(qs)
}

/// Entry files under a fixture store, as parsed entries. Hidden folders (`.kb`) are skipped.
pub fn load_fixture(dir: &Path) -> Result<Vec<Entry>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for ent in ctx(std::fs::read_dir(dir), dir.display())? {
            let p = ctx(ent, dir.display())?.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') {
                continue;
            } else if p.is_dir() {
                walk(&p, out)?;
            } else if name.ends_with(".md") && !["index.md", "AGENTS.md", "log.md"].contains(&name) {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut files = vec![];
    walk(dir, &mut files)?;
    files.sort();
    files
        .iter()
        .map(|p| {
            let text = ctx(std::fs::read_to_string(p), p.display())?;
            sf_kb::parse_entry(&text).map_err(|e| format!("{}: {e}", p.display()))
        })
        .collect()
}

/// Write `entries` as files under `root`, at their ID paths.
fn write_entries(root: &Path, entries: impl Iterator<Item = Entry>) -> Result<()> {
    for e in entries {
        let path = root.join(format!("{}.md", e.meta.id));
        ctx(std::fs::create_dir_all(path.parent().expect("id has a folder")), path.display())?;
        ctx(std::fs::write(&path, sf_kb::render_entry(&e)), path.display())?;
    }
    Ok(())
}

/// Build the rung-`mult` store in `root`: fixture files plus `(mult-1) * fixture` distractors, then one bulk rebuild.
pub fn build_store(root: &Path, fixture: &[Entry], gen: &Distractors, mult: usize) -> Result<Kb> {
    write_entries(root, fixture.iter().cloned())?;
    write_entries(root, (0..fixture.len() * (mult - 1)).map(|i| gen.entry(i)))?;
    let index = ctx(TantivyIndex::in_memory(), "index")?;
    let kb = ctx(Kb::open(root, Box::new(index)), "open kb")?;
    ctx(kb.rebuild_index(), "rebuild")?;
    Ok(kb)
}

/// Metrics for one rung, keyed `retrieval.<mult>x.<name>`.
pub fn score(kb: &Kb, questions: &[Question], mult: usize) -> Result<Metrics> {
    let caller = Caller { agent: "golden".into(), session: "retrieval".into() };
    let (mut r5, mut r10, mut rr) = (vec![], vec![], vec![]);
    let mut searches = 0usize;
    for q in questions {
        searches += 1; // plain BM25: the question text, once
        let ranked: Vec<String> = ctx(kb.search(&caller, &q.question, q.task_class.as_deref(), K), &q.id)?.into_iter().map(|h| h.id).collect();
        r5.push(recall_at_k(&ranked, &q.relevant, 5));
        r10.push(recall_at_k(&ranked, &q.relevant, 10));
        rr.push(reciprocal_rank(&ranked, &q.relevant, K));
    }
    let p = format!("retrieval.{mult}x");
    Ok([
        (format!("{p}.recall@5"), mean(&r5)),
        (format!("{p}.recall@10"), mean(&r10)),
        (format!("{p}.mrr"), mean(&rr)),
        (format!("{p}.searches_per_question"), searches as f64 / questions.len() as f64),
    ]
    .into_iter()
    .map(|(k, v)| (k, round6(v)))
    .collect())
}

pub fn run(golden: &Path) -> Result<Metrics> {
    let fixture = load_fixture(&golden.join("fixtures/kb"))?;
    let questions = load_questions(golden)?;
    let ids: std::collections::HashSet<_> = fixture.iter().map(|e| e.meta.id.as_str()).collect();
    if let Some(bad) = questions.iter().flat_map(|q| &q.relevant).find(|r| !ids.contains(r.as_str())) {
        return Err(format!("relevant entry {bad} is not in the fixture"));
    }
    let gen = Distractors::new(SEED, Vocab::from_entries(&fixture));
    let mut out = Metrics::new();
    for mult in LADDER {
        let dir = ctx(tempfile::tempdir(), "tempdir")?;
        let kb = build_store(dir.path(), &fixture, &gen, mult)?;
        out.extend(score(&kb, &questions, mult)?);
    }
    Ok(out)
}

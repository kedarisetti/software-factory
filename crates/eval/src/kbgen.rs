//! Generator for the committed retrieval fixture `golden/fixtures/kb` (RET-6): reads `golden/fixtures/kb-seed.jsonl`
//! and drives the real `Kb::add` with a fixed clock, so the Markdown files are exactly what the service writes.
//! Rerun `sf-eval gen-kb` after editing the seed and commit the result.

use std::path::Path;

use sf_kb::{Caller, Kb, Knowledge, NewEntry};
use sf_retrieval_tantivy::TantivyIndex;

use crate::{ctx, read_jsonl, Result};

/// 2026-01-01T00:00:00Z in milliseconds.
const FIXED_NOW: u64 = 1_767_225_600_000;

fn fixed_now() -> u64 {
    FIXED_NOW
}

/// Rebuild `golden/fixtures/kb` from the seed. Returns the number of entries written.
pub fn generate(golden: &Path) -> Result<usize> {
    let seed = read_jsonl(&golden.join("fixtures/kb-seed.jsonl"))?;
    let root = golden.join("fixtures/kb");
    if root.exists() {
        ctx(std::fs::remove_dir_all(&root), root.display())?;
    }
    let index = ctx(TantivyIndex::in_memory(), "index")?;
    let kb = ctx(Kb::open(&root, Box::new(index)), "open kb")?.with_now(fixed_now);
    let caller = Caller { agent: "golden".into(), session: "seed".into() };
    for row in &seed {
        let new: NewEntry = ctx(serde_json::from_value(row.clone()), row["title"].to_string())?;
        ctx(kb.add(&caller, new), row["title"].to_string())?;
    }
    // The call log and index cache are runtime state, not fixture.
    ctx(std::fs::remove_dir_all(root.join(".kb")), ".kb")?;
    Ok(seed.len())
}

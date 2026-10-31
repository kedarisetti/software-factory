//! Checks on the committed golden set itself: the fixture matches its generator, questions point at real entries,
//! and the suites meet the ENT-1 / RET-6 size requirements.

use std::path::{Path, PathBuf};

use sf_eval::retrieval::{load_fixture, load_questions};

fn golden() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../golden")
}

fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.push((p.strip_prefix(root).unwrap().display().to_string(), std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = vec![];
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn fixture_is_what_the_generator_writes() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("fixtures")).unwrap();
    std::fs::copy(golden().join("fixtures/kb-seed.jsonl"), tmp.path().join("fixtures/kb-seed.jsonl")).unwrap();
    sf_eval::kbgen::generate(tmp.path()).unwrap();
    assert!(
        tree(&tmp.path().join("fixtures/kb")) == tree(&golden().join("fixtures/kb")),
        "golden/fixtures/kb is stale: run `sf-eval gen-kb` and commit"
    );
}

#[test]
fn questions_cover_task_classes_and_point_at_real_entries() {
    let fixture = load_fixture(&golden().join("fixtures/kb")).unwrap();
    assert!(fixture.len() >= 80, "{}", fixture.len());
    let qs = load_questions(&golden()).unwrap();
    assert!(qs.len() >= 30, "{}", qs.len());
    let ids: std::collections::HashSet<_> = fixture.iter().map(|e| e.meta.id.as_str()).collect();
    for q in &qs {
        assert!(q.relevant.iter().all(|r| ids.contains(r.as_str())), "{}: unknown relevant entry", q.id);
        assert!(q.task_class.is_some(), "{}", q.id);
    }
    let classes: std::collections::HashSet<_> = qs.iter().filter_map(|q| q.task_class.as_deref()).collect();
    assert!(classes.len() >= 5, "{classes:?}");
    let mut seen: Vec<_> = qs.iter().map(|q| q.id.as_str()).collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), qs.len(), "duplicate question ids");
}

#[test]
fn ladder_rung_is_built_with_one_bulk_rebuild_and_searches() {
    use sf_eval::ladder::{Distractors, Vocab};
    let fixture = load_fixture(&golden().join("fixtures/kb")).unwrap();
    let gen = Distractors::new(sf_eval::retrieval::SEED, Vocab::from_entries(&fixture));
    let dir = tempfile::tempdir().unwrap();
    let kb = sf_eval::retrieval::build_store(dir.path(), &fixture, &gen, 3).unwrap();
    assert_eq!(kb.rebuild_index().unwrap(), fixture.len() * 3);
    let m = sf_eval::retrieval::score(&kb, &load_questions(&golden()).unwrap(), 3).unwrap();
    assert_eq!(m["retrieval.3x.searches_per_question"], 1.0);
    assert!(m["retrieval.3x.recall@10"] > 0.5, "{m:?}");
}

#[test]
fn baseline_covers_every_metric_a_run_produces() {
    let b = sf_eval::gate::Baseline::load(&golden().join("baseline.json")).unwrap();
    for rung in sf_eval::retrieval::LADDER {
        for m in ["recall@5", "recall@10", "mrr", "searches_per_question"] {
            assert!(b.metrics.contains_key(&format!("retrieval.{rung}x.{m}")), "{rung}x {m}");
        }
    }
    for m in ["scanner.detection_rate", "scanner.false_positive_rate", "workflow.pass_rate"] {
        assert!(b.metrics.contains_key(m), "{m}");
    }
}

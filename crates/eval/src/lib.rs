//! Golden-set runner and regression gate (ENT-1). Deterministic suites, no model calls:
//! retrieval ladder (RET-6), injection scanner (SBX-3), workflow gates (APR-*, DUR-3).

pub mod gate;
pub mod kbgen;
pub mod ladder;
pub mod metrics;
pub mod retrieval;
pub mod scanner;
pub mod workflow;

use std::path::Path;

pub type Result<T> = std::result::Result<T, String>;

/// Stringify any error with context.
pub fn ctx<T, E: std::fmt::Display>(r: std::result::Result<T, E>, what: impl std::fmt::Display) -> Result<T> {
    r.map_err(|e| format!("{what}: {e}"))
}

/// Read a `.jsonl` file into values; blank lines are skipped.
pub fn read_jsonl(path: &Path) -> Result<Vec<serde_json::Value>> {
    let text = ctx(std::fs::read_to_string(path), path.display())?;
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| ctx(serde_json::from_str(l), format!("{}:{}", path.display(), i + 1)))
        .collect()
}

/// Everything one `run` produces: gated metrics plus wall-clock per suite (informational, never gated).
#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    pub metrics: metrics::Metrics,
    pub timings_ms: std::collections::BTreeMap<String, u64>,
}

/// Run every suite under `golden`. `repo` is the repository root (scanner corpora and the `sf` binary live relative to it).
pub fn run_all(golden: &Path) -> Result<Report> {
    let mut report = Report::default();
    type Suite = fn(&Path) -> Result<metrics::Metrics>;
    let suites: [(&str, Suite); 3] =
        [("retrieval", retrieval::run), ("scanner", scanner::run), ("workflow", workflow::run)];
    for (name, f) in suites {
        let t = std::time::Instant::now();
        report.metrics.extend(f(golden)?);
        report.timings_ms.insert(name.into(), t.elapsed().as_millis() as u64);
    }
    Ok(report)
}

/// Repository root: the parent of the golden directory.
pub fn repo_root(golden: &Path) -> std::path::PathBuf {
    let g = golden.canonicalize().unwrap_or_else(|_| golden.to_path_buf());
    g.parent().map(Path::to_path_buf).unwrap_or_else(|| ".".into())
}

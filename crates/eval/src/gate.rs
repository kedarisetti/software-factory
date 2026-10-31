//! Baseline file and regression comparison (ENT-1): per metric a direction and an absolute tolerance.
//! A metric in the baseline but absent from the current run is a regression. Improvements never
//! update the baseline; a human runs `sf-eval baseline` and commits the result.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::metrics::Metrics;
use crate::{ctx, Result};

const EPS: f64 = 1e-9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// A larger value is better (recall).
    Higher,
    /// A smaller value is better (false positives, searches per question).
    Lower,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gated {
    pub value: f64,
    pub direction: Direction,
    /// Largest worsening, in the metric's own units, that still passes.
    pub tolerance: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub version: u32,
    pub metrics: BTreeMap<String, Gated>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Improved,
    Regressed,
    /// In the baseline, absent from the run. Counts as a regression.
    Missing,
    /// In the run, absent from the baseline. Not gated until a human adds it.
    New,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub name: String,
    pub baseline: Option<f64>,
    pub current: Option<f64>,
    pub tolerance: f64,
    pub direction: Option<Direction>,
    pub status: Status,
}

impl Row {
    /// Signed change, current minus baseline.
    pub fn delta(&self) -> Option<f64> {
        Some(self.current? - self.baseline?)
    }
}

/// Direction and tolerance for a metric the baseline has not seen yet.
pub fn default_policy(name: &str) -> (Direction, f64) {
    if name.ends_with("searches_per_question") {
        (Direction::Lower, 0.0)
    } else if name.starts_with("retrieval.") {
        // BM25 is deterministic but segment layout can reorder ties; allow two points.
        (Direction::Higher, 0.02)
    } else if name.ends_with("false_positive_rate") {
        (Direction::Lower, 0.0)
    } else {
        (Direction::Higher, 0.0)
    }
}

impl Baseline {
    /// Baseline from a run. Directions and tolerances already in `previous` are kept (a human tuned them).
    pub fn from_metrics(current: &Metrics, previous: Option<&Baseline>) -> Baseline {
        let metrics = current
            .iter()
            .map(|(name, &value)| {
                let (direction, tolerance) = match previous.and_then(|p| p.metrics.get(name)) {
                    Some(g) => (g.direction, g.tolerance),
                    None => default_policy(name),
                };
                (name.clone(), Gated { value, direction, tolerance })
            })
            .collect();
        Baseline { version: 1, metrics }
    }

    pub fn load(path: &Path) -> Result<Baseline> {
        let text = ctx(std::fs::read_to_string(path), path.display())?;
        ctx(serde_json::from_str(&text), path.display())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut text = ctx(serde_json::to_string_pretty(self), "baseline")?;
        text.push('\n');
        ctx(std::fs::write(path, text), path.display())
    }
}

/// Compare a run to the baseline: one row per baseline metric, plus rows for metrics only the run has.
pub fn compare(baseline: &Baseline, current: &Metrics) -> Vec<Row> {
    let mut rows: Vec<Row> = baseline
        .metrics
        .iter()
        .map(|(name, g)| {
            let cur = current.get(name).copied();
            let status = match cur {
                None => Status::Missing,
                Some(c) => {
                    let worse = match g.direction {
                        Direction::Higher => g.value - c,
                        Direction::Lower => c - g.value,
                    };
                    if worse > g.tolerance + EPS {
                        Status::Regressed
                    } else if worse < -EPS {
                        Status::Improved
                    } else {
                        Status::Ok
                    }
                }
            };
            Row { name: name.clone(), baseline: Some(g.value), current: cur, tolerance: g.tolerance, direction: Some(g.direction), status }
        })
        .collect();
    for (name, &c) in current.iter().filter(|(n, _)| !baseline.metrics.contains_key(*n)) {
        rows.push(Row { name: name.clone(), baseline: None, current: Some(c), tolerance: 0.0, direction: None, status: Status::New });
    }
    rows
}

/// Rows that block the change.
pub fn regressions(rows: &[Row]) -> Vec<&Row> {
    rows.iter().filter(|r| matches!(r.status, Status::Regressed | Status::Missing)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(entries: &[(&str, f64, Direction, f64)]) -> Baseline {
        Baseline {
            version: 1,
            metrics: entries.iter().map(|&(n, value, direction, tolerance)| (n.to_string(), Gated { value, direction, tolerance })).collect(),
        }
    }

    fn run(entries: &[(&str, f64)]) -> Metrics {
        entries.iter().map(|&(n, v)| (n.to_string(), v)).collect()
    }

    fn status(rows: &[Row], name: &str) -> Status {
        rows.iter().find(|r| r.name == name).unwrap().status
    }

    #[test]
    fn higher_is_better_with_tolerance() {
        let b = base(&[("r", 0.80, Direction::Higher, 0.02)]);
        assert_eq!(status(&compare(&b, &run(&[("r", 0.80)])), "r"), Status::Ok);
        assert_eq!(status(&compare(&b, &run(&[("r", 0.78)])), "r"), Status::Ok, "exactly at tolerance passes");
        assert_eq!(status(&compare(&b, &run(&[("r", 0.77)])), "r"), Status::Regressed);
        assert_eq!(status(&compare(&b, &run(&[("r", 0.95)])), "r"), Status::Improved);
    }

    #[test]
    fn lower_is_better_flips_the_sign() {
        let b = base(&[("fp", 0.0, Direction::Lower, 0.0)]);
        assert_eq!(status(&compare(&b, &run(&[("fp", 0.0)])), "fp"), Status::Ok);
        assert_eq!(status(&compare(&b, &run(&[("fp", 0.03)])), "fp"), Status::Regressed);
        let b = base(&[("n", 1.0, Direction::Lower, 0.5)]);
        assert_eq!(status(&compare(&b, &run(&[("n", 1.5)])), "n"), Status::Ok);
        assert_eq!(status(&compare(&b, &run(&[("n", 0.5)])), "n"), Status::Improved);
    }

    #[test]
    fn missing_metric_is_a_regression_and_new_metric_is_not() {
        let b = base(&[("a", 1.0, Direction::Higher, 0.0)]);
        let rows = compare(&b, &run(&[("b", 1.0)]));
        assert_eq!(status(&rows, "a"), Status::Missing);
        assert_eq!(status(&rows, "b"), Status::New);
        let bad: Vec<_> = regressions(&rows).iter().map(|r| r.name.as_str()).collect();
        assert_eq!(bad, ["a"]);
    }

    #[test]
    fn delta_and_tolerance_are_reported() {
        let b = base(&[("r", 0.9, Direction::Higher, 0.0)]);
        let rows = compare(&b, &run(&[("r", 0.7)]));
        assert!((rows[0].delta().unwrap() + 0.2).abs() < 1e-12);
    }

    #[test]
    fn rebaseline_keeps_tuned_policy_and_applies_defaults() {
        let prev = base(&[("retrieval.1x.mrr", 0.5, Direction::Higher, 0.1)]);
        let cur = run(&[("retrieval.1x.mrr", 0.6), ("scanner.false_positive_rate", 0.0), ("retrieval.1x.searches_per_question", 1.0)]);
        let b = Baseline::from_metrics(&cur, Some(&prev));
        assert_eq!(b.metrics["retrieval.1x.mrr"], Gated { value: 0.6, direction: Direction::Higher, tolerance: 0.1 });
        assert_eq!(b.metrics["scanner.false_positive_rate"].direction, Direction::Lower);
        assert_eq!(b.metrics["retrieval.1x.searches_per_question"].direction, Direction::Lower);
    }

    #[test]
    fn baseline_round_trips_through_a_file() {
        let b = base(&[("x", 0.25, Direction::Lower, 0.01)]);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b.json");
        b.save(&p).unwrap();
        assert_eq!(Baseline::load(&p).unwrap(), b);
    }
}

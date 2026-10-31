//! Metric math and the flat metric map the gate compares (ENT-1, RET-6).

use std::collections::BTreeMap;

/// Metric name (`retrieval.10x.recall@5`) to value.
pub type Metrics = BTreeMap<String, f64>;

/// Fraction of `relevant` found in the first `k` of `ranked`. 0 when `relevant` is empty.
pub fn recall_at_k(ranked: &[String], relevant: &[String], k: usize) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    let found = relevant.iter().filter(|r| ranked.iter().take(k).any(|x| x == *r)).count();
    found as f64 / relevant.len() as f64
}

/// 1 / rank of the first relevant entry within the first `k`, else 0.
pub fn reciprocal_rank(ranked: &[String], relevant: &[String], k: usize) -> f64 {
    ranked.iter().take(k).position(|x| relevant.contains(x)).map_or(0.0, |i| 1.0 / (i as f64 + 1.0))
}

pub fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

/// Round to 6 places so baselines do not churn on float noise.
pub fn round6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn recall_counts_relevant_inside_cutoff() {
        let ranked = v(&["a", "b", "c", "d", "e", "f"]);
        assert_eq!(recall_at_k(&ranked, &v(&["a", "f"]), 5), 0.5);
        assert_eq!(recall_at_k(&ranked, &v(&["a", "f"]), 6), 1.0);
        assert_eq!(recall_at_k(&ranked, &v(&["x"]), 6), 0.0);
        assert_eq!(recall_at_k(&ranked, &[], 6), 0.0);
        assert_eq!(recall_at_k(&[], &v(&["a"]), 5), 0.0);
    }

    #[test]
    fn mrr_uses_first_relevant_rank_within_cutoff() {
        let ranked = v(&["a", "b", "c"]);
        assert_eq!(reciprocal_rank(&ranked, &v(&["c", "b"]), 10), 0.5);
        assert_eq!(reciprocal_rank(&ranked, &v(&["a"]), 10), 1.0);
        assert_eq!(reciprocal_rank(&ranked, &v(&["c"]), 2), 0.0);
        assert_eq!(reciprocal_rank(&ranked, &v(&["z"]), 10), 0.0);
    }

    #[test]
    fn mean_of_empty_is_zero() {
        assert_eq!(mean(&[]), 0.0);
        assert_eq!(mean(&[1.0, 0.5]), 0.75);
        assert_eq!(round6(0.123_456_789), 0.123457);
    }
}

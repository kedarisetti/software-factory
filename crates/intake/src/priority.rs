//! Priority scoring (TRG-2): source base score plus label and keyword rules.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::event::{normalise, IntakeEvent};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriorityRule {
    pub name: String,
    #[serde(default)]
    pub labels: Vec<String>,
    /// Whole-word (or whole-phrase) matches in title and text, case-insensitive.
    #[serde(default)]
    pub keywords: Vec<String>,
    pub score: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriorityConfig {
    pub base: BTreeMap<String, i32>,
    pub rules: Vec<PriorityRule>,
}

/// Score plus the names of the rules that fired (stored as `payload.priority_rules`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scored {
    pub score: i32,
    pub rules: Vec<String>,
}

fn rule(name: &str, labels: &[&str], keywords: &[&str], score: i32) -> PriorityRule {
    PriorityRule {
        name: name.into(),
        labels: labels.iter().map(|s| s.to_string()).collect(),
        keywords: keywords.iter().map(|s| s.to_string()).collect(),
        score,
    }
}

impl Default for PriorityConfig {
    fn default() -> Self {
        PriorityConfig {
            base: BTreeMap::from([("github".into(), 10), ("slack".into(), 5)]),
            rules: vec![
                rule("p0", &["p0", "priority:p0"], &["p0"], 100),
                rule("incident", &["incident", "sev1"], &["incident", "outage"], 80),
                rule("urgent", &["urgent"], &["urgent", "asap"], 50),
                rule("p2", &["p2", "priority:p2"], &[], -20),
                rule("nice-to-have", &["nice-to-have"], &["nice to have"], -30),
            ],
        }
    }
}

impl PriorityConfig {
    /// base + strongest boost + strongest demotion, so stacked labels do not inflate the score.
    pub fn score(&self, ev: &IntakeEvent) -> Scored {
        let labels: Vec<String> = ev.labels.iter().map(|l| normalise(l)).collect();
        let hay = format!(" {} ", normalise(&format!("{} {}", ev.title, ev.text)));
        let fired: Vec<&PriorityRule> = self.rules.iter().filter(|r| fires(r, &labels, &hay)).collect();
        let up = fired.iter().map(|r| r.score).filter(|s| *s > 0).max().unwrap_or(0);
        let down = fired.iter().map(|r| r.score).filter(|s| *s < 0).min().unwrap_or(0);
        Scored {
            score: self.base.get(&ev.source).copied().unwrap_or(0) + up + down,
            rules: fired.iter().map(|r| r.name.clone()).collect(),
        }
    }
}

fn fires(r: &PriorityRule, labels: &[String], hay: &str) -> bool {
    r.labels.iter().any(|l| labels.contains(&normalise(l))) || r.keywords.iter().any(|k| hay.contains(&format!(" {} ", normalise(k))))
}

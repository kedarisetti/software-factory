//! Deterministic distractor entries for the nested store-size ladder (RET-6, [1]). Entry `i` depends only on
//! `(seed, i, vocabulary)`, so the 10x store is a strict subset of the 30x store and reruns are byte-identical.
//! Distractors reuse the fixture's own vocabulary and tags so they compete with the relevant entries
//! without answering any question.

use std::collections::BTreeSet;

use sf_kb::{Caller, Entry, EntryKind, EntryStatus, Meta};

const STOP: [&str; 14] = [
    "which", "their", "there", "these", "those", "would", "about", "after", "before", "should", "never", "where", "while", "other",
];
const SENTENCES: [&str; 8] = [
    "The {} {} is reviewed before the {} step and reported to the {} owner.",
    "Teams usually revisit the {} when a {} changes, then check the {} again.",
    "A {} {} was discussed in planning; no change to {} was agreed.",
    "Older notes about {} mention the {} and the {}, which no longer apply here.",
    "Ask the {} team before touching the {} or the {}.",
    "We compared the {} with the {} and found the {} acceptable for now.",
    "The {} dashboard shows the {} per {} over the last week.",
    "Document the {} next to the {} so the {} stays easy to find.",
];
const TITLES: [&str; 5] = ["Notes on {} {}", "{} and {} review", "Planning the {} {} {}", "Weekly {} {} summary", "Open questions about {} {}"];
const KINDS: [EntryKind; 10] = [
    EntryKind::Note,
    EntryKind::Note,
    EntryKind::Note,
    EntryKind::Note,
    EntryKind::Learning,
    EntryKind::Learning,
    EntryKind::Learning,
    EntryKind::Convention,
    EntryKind::Glossary,
    EntryKind::Page,
];

/// Vocabulary taken from a fixture: lowercase words of 5+ letters (stop words dropped), tags and task classes. Sorted, so stable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Vocab {
    pub words: Vec<String>,
    pub tags: Vec<String>,
    pub classes: Vec<String>,
}

impl Vocab {
    pub fn from_entries(entries: &[Entry]) -> Vocab {
        let (mut words, mut tags, mut classes) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        for e in entries {
            let text = format!("{} {}", e.meta.title, e.body);
            for w in text.split(|c: char| !c.is_ascii_alphabetic()) {
                let w = w.to_ascii_lowercase();
                if w.len() >= 5 && !STOP.contains(&w.as_str()) {
                    words.insert(w);
                }
            }
            tags.extend(e.meta.tags.iter().cloned());
            classes.extend(e.meta.task_class.iter().cloned());
        }
        Vocab { words: words.into_iter().collect(), tags: tags.into_iter().collect(), classes: classes.into_iter().collect() }
    }
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        (splitmix(&mut self.0) % n as u64) as usize
    }
}

fn fill(template: &str, rng: &mut Rng, words: &[String]) -> String {
    let mut out = String::new();
    for (i, part) in template.split("{}").enumerate() {
        if i > 0 {
            out.push_str(&words[rng.below(words.len())]);
        }
        out.push_str(part);
    }
    out
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

pub struct Distractors {
    pub seed: u64,
    pub vocab: Vocab,
}

impl Distractors {
    pub fn new(seed: u64, vocab: Vocab) -> Distractors {
        assert!(vocab.words.len() >= 8, "vocabulary too small to generate distractors");
        Distractors { seed, vocab }
    }

    /// The `i`th distractor. ID `<kind>/archive/<slug>-<i>` never collides with a fixture ID.
    pub fn entry(&self, i: usize) -> Entry {
        let mut rng = Rng(self.seed ^ (i as u64 + 1).wrapping_mul(0xD6E8_FEB8_6659_FD93));
        let w = &self.vocab.words;
        let title = capitalize(&fill(TITLES[rng.below(TITLES.len())], &mut rng, w));
        let body = (0..3).map(|_| fill(SENTENCES[rng.below(SENTENCES.len())], &mut rng, w)).collect::<Vec<_>>().join(" ");
        let kind = KINDS[rng.below(KINDS.len())];
        let tags: Vec<String> = if self.vocab.tags.is_empty() {
            vec![]
        } else {
            (0..2).map(|_| self.vocab.tags[rng.below(self.vocab.tags.len())].clone()).collect::<BTreeSet<_>>().into_iter().collect()
        };
        // Four in ten are class-less (visible to every task class, RET-3); the rest take a fixture class.
        let task_class = (!self.vocab.classes.is_empty() && rng.below(10) >= 4).then(|| self.vocab.classes[rng.below(self.vocab.classes.len())].clone());
        let slug: String = title.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let slug = slug.split('-').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("-");
        let id = format!("{}/archive/{}-{}", kind.dir(), slug, i);
        let caller = Caller { agent: "golden".into(), session: "ladder".into() };
        Entry {
            meta: Meta {
                id,
                kind,
                title,
                tags,
                task_class,
                links: vec![],
                anchors: vec![],
                source: "golden:ladder".into(),
                author: caller,
                created_at: 1_767_225_600_000,
                updated_at: 1_767_225_600_000,
                expires_at: None,
                status: EntryStatus::Active,
                outcome: None,
                count: 1,
                source_runs: vec![],
                key: None,
                helped: 0,
                misled: 0,
            },
            body,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab() -> Vocab {
        Vocab {
            words: "ledger postgres migration settlement webhook retry cache index tenant balance journal posting".split(' ').map(String::from).collect(),
            tags: vec!["money".into(), "ci".into()],
            classes: vec!["fix-ci".into(), "migrate-db".into()],
        }
    }

    #[test]
    fn same_seed_same_entries() {
        let (a, b) = (Distractors::new(7, vocab()), Distractors::new(7, vocab()));
        assert!((0..50).all(|i| a.entry(i) == b.entry(i)));
        assert_ne!(a.entry(3), Distractors::new(8, vocab()).entry(3));
    }

    #[test]
    fn entries_are_independent_of_how_many_are_generated() {
        let d = Distractors::new(1, vocab());
        let small: Vec<_> = (0..10).map(|i| d.entry(i)).collect();
        let large: Vec<_> = (0..100).map(|i| d.entry(i)).collect();
        assert_eq!(small[..], large[..10], "10x ladder rung is a prefix of 30x");
    }

    #[test]
    fn ids_are_valid_unique_and_rendered_files_parse_back() {
        let d = Distractors::new(1, vocab());
        let es: Vec<_> = (0..200).map(|i| d.entry(i)).collect();
        assert!(es.iter().all(|e| sf_kb::valid_id(&e.meta.id)), "{:?}", es.iter().find(|e| !sf_kb::valid_id(&e.meta.id)).map(|e| &e.meta.id));
        let ids: BTreeSet<_> = es.iter().map(|e| e.meta.id.clone()).collect();
        assert_eq!(ids.len(), es.len());
        assert!(es.iter().all(|e| sf_kb::parse_entry(&sf_kb::render_entry(e)).unwrap() == *e));
    }

    #[test]
    fn vocab_is_sorted_and_drops_short_and_stop_words() {
        let mut e = d_entry();
        e.body = "The ledger which never lies. Postgres is fine, ok".into();
        let v = Vocab::from_entries(&[e]);
        assert_eq!(v.words, ["ledger", "postgres"]);
    }

    fn d_entry() -> Entry {
        let mut e = Distractors::new(1, vocab()).entry(0);
        e.meta.title = String::new();
        e.meta.tags.clear();
        e
    }
}

//! Generated wiki files (WIKI-1 to WIKI-3, WIKI-6): slugs, `index.md` text, `log.md` lines, `AGENTS.md`.
//! Everything here is plain code over the entries; no LLM is involved (the structure must not cost tokens to build).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::*;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomic file write: temp file in the same folder, fsync, rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("f");
    let tmp = path.with_file_name(format!(".{name}.tmp-{}-{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
    let r = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// Lowercase alnum words joined by `-`, at most `max` chars, cut at a word boundary (WIKI-1).
pub fn slug(s: &str, max: usize) -> String {
    let lower = s.to_lowercase();
    let mut out = String::new();
    for w in lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()) {
        if out.len() + w.len() + usize::from(!out.is_empty()) > max {
            if out.is_empty() {
                out.push_str(&w[..max.min(w.len())]);
            }
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(w);
    }
    out
}

/// File name stem for an entry title. Never empty and never `index` (that file is generated).
pub fn entry_slug(title: &str) -> String {
    match slug(title, 60).as_str() {
        "" => "entry".into(),
        "index" => "index-entry".into(),
        s => s.into(),
    }
}

/// Folder of a new entry (WIKI-1): `<kind>/<topic>`, topic = task class, else first tag, else `general`.
/// Glossary entries without either use the two-segment form `glossary/<slug>`.
pub fn dir_of(kind: EntryKind, task_class: Option<&str>, tags: &[String]) -> String {
    let topic = [task_class, tags.first().map(String::as_str)].into_iter().flatten().map(|s| slug(s, 40)).find(|s| !s.is_empty());
    match (kind, topic) {
        (EntryKind::Glossary, None) => kind.dir().into(),
        (_, t) => format!("{}/{}", kind.dir(), t.as_deref().unwrap_or("general")),
    }
}

/// First sentence or line of a body, at most 120 chars (RET-7).
pub fn summary(body: &str) -> String {
    let line = body.lines().map(|l| l.trim().trim_start_matches('#').trim()).find(|l| !l.is_empty()).unwrap_or("");
    let s = &line[..line.find(". ").map_or(line.len(), |i| i + 1)];
    if s.chars().count() > 120 { s.chars().take(119).chain("…".chars()).collect() } else { s.to_string() }
}

/// `2001-09-09T01:46:40Z` from unix milliseconds (civil-from-days, no chrono).
pub fn iso_utc(ms: Timestamp) -> String {
    let secs = ms / 1000;
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// One `log.md` line (WIKI-3).
pub fn log_line(ms: Timestamp, agent: &str, op: &str, id: &str, detail: &str) -> String {
    let clean = |s: &str| s.replace(['\n', '\r'], " ");
    format!("- {} **{}** {op} [[{id}]] ({})\n", iso_utc(ms), clean(agent), clean(detail))
}

pub const LOG_HEADER: &str = "# Change log\n\nAppend-only, one line per change. Written by the knowledge service; do not edit.\n\n";

fn plural(n: usize) -> String {
    format!("{n} {}", if n == 1 { "entry" } else { "entries" })
}

fn entry_line(e: &Entry, href: &str) -> String {
    let m = &e.meta;
    let mut line = format!("- [{}]({href}.md)", m.title.replace('[', "(").replace(']', ")").replace('\n', " "));
    let sum = summary(&e.body);
    // Learnings usually have body == title; repeating it adds nothing.
    if !sum.is_empty() && !m.title.starts_with(sum.trim_end_matches(['.', '…'])) {
        line.push_str(&format!(": {sum}"));
    }
    match m.status {
        EntryStatus::Active => {}
        EntryStatus::Expired => line.push_str(" · expired"),
        EntryStatus::BrokenAnchor => line.push_str(" · broken-anchor"),
        EntryStatus::Stale => line.push_str(" · stale"),
    }
    if m.count > 1 {
        line.push_str(&format!(" · ×{}", m.count));
    }
    line.push('\n');
    line
}

/// `index.md` text for the root (`""`) and for every folder that holds entries (WIKI-2).
/// Keys are folder paths. Relative Markdown links, so GitHub and Obsidian both follow them.
pub fn render_indexes(entries: &[Entry]) -> BTreeMap<String, String> {
    let mut sorted: Vec<&Entry> = entries.iter().collect();
    sorted.sort_by(|a, b| b.meta.updated_at.cmp(&a.meta.updated_at).then_with(|| a.meta.id.cmp(&b.meta.id)));
    let mut direct: BTreeMap<String, Vec<&Entry>> = BTreeMap::from([(String::new(), vec![])]);
    let mut total: BTreeMap<String, usize> = BTreeMap::new();
    for e in &sorted {
        let dir = e.meta.id.rsplit_once('/').map_or("", |x| x.0);
        direct.entry(dir.to_string()).or_default().push(e);
        let mut p = String::new();
        *total.entry(p.clone()).or_default() += 1;
        for seg in dir.split('/') {
            p = if p.is_empty() { seg.to_string() } else { format!("{p}/{seg}") };
            *total.entry(p.clone()).or_default() += 1;
            direct.entry(p.clone()).or_default();
        }
    }
    let parent = |p: &str| p.rsplit_once('/').map_or(String::new(), |x| x.0.to_string());
    direct
        .iter()
        .map(|(dir, mine)| {
            let mut s = format!("# {}\n\n_Generated by the knowledge service from the entries. Do not edit._\n", if dir.is_empty() { "Knowledge base" } else { dir });
            if dir.is_empty() {
                s.push_str("\nSee [AGENTS.md](AGENTS.md) for the conventions and [log.md](log.md) for the change log.\n");
            }
            let kids: Vec<_> = total.iter().filter(|(p, _)| !p.is_empty() && parent(p) == *dir).collect();
            if !kids.is_empty() {
                s.push_str("\n## Folders\n\n");
                for (p, n) in kids {
                    let name = p.rsplit('/').next().unwrap_or(p);
                    s.push_str(&format!("- [{name}/]({name}/index.md): {}\n", plural(*n)));
                }
            }
            if dir.is_empty() && !sorted.is_empty() {
                s.push_str("\n## Recently changed\n\n");
                sorted.iter().take(10).for_each(|e| s.push_str(&entry_line(e, &e.meta.id)));
            }
            if !mine.is_empty() {
                s.push_str("\n## Entries\n\n");
                mine.iter().for_each(|e| s.push_str(&entry_line(e, e.meta.id.rsplit('/').next().unwrap_or(&e.meta.id))));
            }
            (dir.clone(), s)
        })
        .collect()
}

/// Written once on open if missing, then owned by humans (WIKI-6).
pub const AGENTS_MD: &str = "# Knowledge base guide

This folder is a wiki of plain Markdown files, shared by humans and agents.

## Layout
- `<kind>/<topic>/<slug>.md`, kinds: `notes`, `learnings`, `conventions`, `glossary`, `pages`.
- Topic is the task class, else the first tag, else `general`. Glossary may be `glossary/<slug>.md`.
- An entry's ID is its path without `.md`, e.g. `learnings/fix-ci/pin-the-rust-toolchain`.
- `index.md` (root and every folder) lists subfolders and entries with one-line summaries.
- `log.md` is the append-only change log: time, agent, operation, entry.
- `.kb/` is cache and call log; safe to delete.

## Finding things
1. `search` first, using the task's own words (BM25 over title, tags, body).
2. Then `get` the best hits, or `list` a folder (start at the root) to see everything about a topic.
3. Reformulate and search again if the hits are off. Do not read the whole store.

## Writing
- Use `add` for a note, convention or glossary term; `learn` at the end of a run.
- When a topic has many entries, write one synthesis page: kind `page`, with `derived-from`
  links to its sources. One page at a time, no batch rewrites.
- Report whether an entry helped with `feedback`.

## Rules
- Frontmatter is the source of truth. Links live there; the `[[wikilink]]` footer is generated.
- Never hand-edit `index.md`, `log.md` entries or the links footer; they are regenerated.
- Edit this file freely; it is never overwritten.
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_and_slug() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_000_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(iso_utc(1_709_164_800_000), "2024-02-29T00:00:00Z");
        assert_eq!(slug("Pin the Rust toolchain!", 60), "pin-the-rust-toolchain");
        assert_eq!(slug("alpha beta gamma", 12), "alpha-beta");
        assert_eq!(slug("abcdefghij", 4), "abcd");
        assert!(AGENTS_MD.lines().count() < 40);
    }
}

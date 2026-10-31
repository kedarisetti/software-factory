//! Recurring-bottleneck detection (TRG-4): clusters of similar intake items become backlog items.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use sf_core::{ItemStatus, Timestamp, WorkItem};

use crate::event::{normalise, short_hash};
use crate::intake::item_id;

pub const SOURCE: &str = "intake-analysis";
const STOP: &[&str] = &["the", "and", "for", "with", "not", "from", "this", "that", "when", "its", "are", "was", "has", "have", "can", "cant", "does"];

fn labels(i: &WorkItem) -> Vec<String> {
    let l = i.payload.get("labels").and_then(Value::as_array).into_iter().flatten();
    l.filter_map(Value::as_str).filter(|s| !s.starts_with("factory:")).map(normalise).collect()
}

/// Signatures an item belongs to: its title keyword set (2+ words) and its label set.
fn signatures(i: &WorkItem) -> Vec<String> {
    let title = i.payload.get("title").and_then(Value::as_str).unwrap_or("");
    let mut words: Vec<String> =
        normalise(title).split(' ').filter(|w| w.len() >= 3 && !STOP.contains(w)).map(String::from).collect();
    words.sort();
    words.dedup();
    let mut sigs = vec![];
    if words.len() >= 2 {
        sigs.push(format!("title:{}", words.join(" ")));
    }
    let mut ls = labels(i);
    ls.sort();
    ls.dedup();
    if !ls.is_empty() {
        sigs.push(format!("labels:{}", ls.join(",")));
    }
    sigs
}

/// Backlog items for signatures shared by at least `min_count` items received in
/// `[now - window_ms, now]`. The dedupe key holds the signature and the window bucket start, so
/// re-running in the same window yields the same keys.
pub fn bottlenecks(items: &[WorkItem], now: Timestamp, window_ms: u64, min_count: usize) -> Vec<WorkItem> {
    let window_ms = window_ms.max(1);
    let (from, bucket) = (now.saturating_sub(window_ms), now / window_ms * window_ms);
    let mut clusters: BTreeMap<String, Vec<&WorkItem>> = BTreeMap::new();
    for i in items.iter().filter(|i| i.source != SOURCE && i.received_at >= from && i.received_at <= now) {
        for s in signatures(i) {
            clusters.entry(s).or_default().push(i);
        }
    }
    clusters.into_iter().filter(|(_, v)| v.len() >= min_count.max(1)).map(|(sig, mut v)| {
        v.sort_by(|a, b| (a.received_at, &a.item_id).cmp(&(b.received_at, &b.item_id)));
        let h = short_hash(&[&sig], 12);
        let dedupe_key = format!("bottleneck:{h}:{bucket}");
        WorkItem {
            item_id: item_id(&dedupe_key),
            source: SOURCE.into(),
            source_ref: format!("bottleneck:{h}"),
            dedupe_key,
            actor: SOURCE.into(),
            channel: "intake".into(),
            workflow: None,
            priority: 0,
            status: ItemStatus::Backlog,
            reason: Some(format!("{} similar items in window", v.len())),
            run_id: None,
            payload: json!({
                "title": format!("Recurring: {sig} ({} items)", v.len()),
                "signature": sig,
                "window_start": bucket, "window_ms": window_ms, "count": v.len(),
                "evidence": v.iter().map(|i| json!({"item_id": i.item_id, "source_ref": i.source_ref})).collect::<Vec<_>>(),
            }),
            received_at: now,
            updated_at: now,
        }
    }).collect()
}

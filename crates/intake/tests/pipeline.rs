mod common;
use std::collections::BTreeMap;

use common::{ev, intake_with};
use sf_core::ItemStatus;
use sf_intake::{IntakeConfig, PriorityConfig, RateLimits};

const T0: u64 = 1_700_000_000_000;
const MIN: u64 = 60_000;

/// No source has a built-in score; this config says what `github` is worth.
fn based() -> IntakeConfig {
    IntakeConfig { priority: PriorityConfig { base: BTreeMap::from([("github".to_string(), 10)]), ..Default::default() }, ..Default::default() }
}

fn all(i: &sf_intake::Intake) -> Vec<sf_core::WorkItem> {
    i.queue().list_items(&[ItemStatus::Queued, ItemStatus::Rejected, ItemStatus::RateLimited], 100).unwrap()
}

#[test]
fn allowed_event_is_queued_with_payload() {
    let i = intake_with(based());
    let mut e = ev("github", "acme/shop", "octocat", "fix-bug", "Coupon bug\nshipping doubles", "github:acme/shop#42:opened", T0);
    e.labels = vec!["p0".into()];
    let o = i.accept(&e).unwrap();
    assert!(o.inserted && o.status == ItemStatus::Queued && o.duplicate_of.is_none() && o.reason.is_none());
    assert_eq!(o.item.workflow.as_deref(), Some("fix-bug"));
    assert_eq!(o.item.payload["priority_rules"], serde_json::json!(["p0"]));
    assert_eq!(o.item.payload["untrusted"], true);
    assert_eq!(o.item.priority, 110);
    // Without a configured base the same event scores only its rules.
    let plain = intake_with(IntakeConfig::default()).accept(&e).unwrap();
    assert_eq!(plain.item.priority, 100);
}

#[test]
fn rejected_items_are_stored_with_reason() {
    let i = intake_with(IntakeConfig::default());
    let o = i.accept(&ev("slack", "C024BE91L", "U1", "deploy-prod", "ship it", "slack:E1", T0)).unwrap();
    assert!(o.inserted && o.status == ItemStatus::Rejected);
    assert!(o.reason.unwrap().contains("deploy"));
    let none = i.accept(&ev("slack", "C024BE91L", "U1", "", "hello", "slack:E2", T0)).unwrap();
    assert_eq!((none.status, none.reason.as_deref()), (ItemStatus::Rejected, Some("no workflow requested")));
    let unknown = i.accept(&ev("slack", "CNOPE", "U1", "fix-bug", "hi", "slack:E3", T0)).unwrap();
    assert_eq!(unknown.status, ItemStatus::Rejected);
    assert_eq!(all(&i).len(), 3);
}

#[test]
fn exact_duplicate_inserts_once() {
    let i = intake_with(IntakeConfig::default());
    let first = i.accept(&ev("slack", "C024BE91L", "U1", "fix-bug", "one", "slack:Ev1", T0)).unwrap();
    // A Slack retry carries the same event id; later and with different text, so only the key matches.
    let retry = i.accept(&ev("slack", "C024BE91L", "U1", "fix-bug", "different words", "slack:Ev1", T0 + 20 * MIN)).unwrap();
    assert!(!retry.inserted);
    assert_eq!(retry.duplicate_of.as_deref(), Some(first.item.item_id.as_str()));
    assert_eq!(all(&i).len(), 1);
}

#[test]
fn near_duplicate_respects_window_and_identity() {
    let i = intake_with(IntakeConfig::default());
    let a = i.accept(&ev("slack", "C024BE91L", "U1", "fix-bug", "Login page 500s!", "slack:A", T0)).unwrap();
    // Same words modulo case and punctuation, new event id, inside 10 min.
    let b = i.accept(&ev("slack", "C024BE91L", "U1", "fix-bug", "login page 500s", "slack:B", T0 + 5 * MIN)).unwrap();
    assert!(!b.inserted);
    assert_eq!(b.duplicate_of.as_deref(), Some(a.item.item_id.as_str()));
    assert!(b.reason.unwrap().contains("near-duplicate"));
    // Different actor, different channel: not a duplicate.
    assert!(i.accept(&ev("slack", "C024BE91L", "U2", "fix-bug", "login page 500s", "slack:C", T0 + MIN)).unwrap().inserted);
    assert!(i.accept(&ev("slack", "C0DEFAULT1", "U1", "triage", "login page 500s", "slack:D", T0 + MIN)).is_ok_and(|o| o.status == ItemStatus::Rejected));
    // Outside the window: new item.
    assert!(i.accept(&ev("slack", "C024BE91L", "U1", "fix-bug", "login page 500s", "slack:E", T0 + 11 * MIN)).unwrap().inserted);
}

#[test]
fn rate_limit_stores_item_as_rate_limited() {
    let cfg = IntakeConfig { rate: RateLimits { window_ms: 10 * MIN, per_source: BTreeMap::new(), per_actor: Some(2) }, ..Default::default() };
    let i = intake_with(cfg);
    let mut last = None;
    for n in 0..3 {
        last = Some(i.accept(&ev("github", "acme/shop", "octocat", "fix-bug", &format!("issue number {n} differs"), &format!("k{n}"), T0 + n * 1000)).unwrap());
    }
    let o = last.unwrap();
    assert!(o.inserted && o.status == ItemStatus::RateLimited);
    assert!(o.reason.unwrap().contains("octocat"));
    assert_eq!(o.item.workflow.as_deref(), Some("fix-bug"));
    // Another actor is unaffected; after the window the same actor is accepted again.
    assert_eq!(i.accept(&ev("github", "acme/shop", "hubot", "fix-bug", "other", "k9", T0 + 5000)).unwrap().status, ItemStatus::Queued);
    assert_eq!(i.accept(&ev("github", "acme/shop", "octocat", "fix-bug", "later", "k10", T0 + 30 * MIN)).unwrap().status, ItemStatus::Queued);
}

#[test]
fn per_source_limit() {
    let cfg = IntakeConfig { rate: RateLimits { window_ms: 10 * MIN, per_source: BTreeMap::from([("slack".into(), 2)]), per_actor: None }, ..Default::default() };
    let i = intake_with(cfg);
    let st: Vec<_> = (0..3)
        .map(|n| i.accept(&ev("slack", "C024BE91L", &format!("U{n}"), "fix-bug", &format!("thing {n}"), &format!("s{n}"), T0 + n)).unwrap().status)
        .collect();
    assert_eq!(st, [ItemStatus::Queued, ItemStatus::Queued, ItemStatus::RateLimited]);
    // GitHub has no source limit.
    assert_eq!(i.accept(&ev("github", "acme/shop", "o", "fix-bug", "gh", "g1", T0)).unwrap().status, ItemStatus::Queued);
}

#[test]
fn priority_orders_the_queue() {
    let i = intake_with(based());
    let mk = |key: &str, text: &str, labels: &[&str], at: u64| {
        let mut e = ev("github", "acme/shop", &format!("a{key}"), "fix-bug", text, key, at);
        e.labels = labels.iter().map(|s| s.to_string()).collect();
        i.accept(&e).unwrap()
    };
    mk("low", "tidy docs", &["p2"], T0);
    mk("mid-new", "plain bug b", &[], T0 + 2000);
    mk("mid-old", "plain bug a", &[], T0 + 1000);
    mk("hot", "prod is down", &["incident", "urgent"], T0 + 3000);
    let kw = mk("kw", "URGENT: payments failing", &[], T0 + 4000);
    assert_eq!(kw.item.payload["priority_rules"], serde_json::json!(["urgent"]));
    let order: Vec<_> = i.queue().list_items(&[ItemStatus::Queued], 10).unwrap().into_iter().map(|x| x.dedupe_key).collect();
    assert_eq!(order, ["hot", "kw", "mid-old", "mid-new", "low"]);
    let hot = i.queue().list_items(&[ItemStatus::Queued], 1).unwrap().remove(0);
    // Strongest boost only: 10 + 80, not 10 + 80 + 50.
    assert_eq!((hot.priority, hot.payload["priority_rules"].clone()), (90, serde_json::json!(["incident", "urgent"])));
}

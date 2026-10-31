mod common;
use common::{ev, intake_with};
use sf_core::ItemStatus;
use sf_intake::bottleneck::{bottlenecks, SOURCE};
use sf_intake::IntakeConfig;

const T0: u64 = 1_700_000_000_000;
const HOUR: u64 = 3_600_000;

fn seeded() -> sf_intake::Intake {
    let i = intake_with(IntakeConfig::default());
    for (n, (title, at)) in [
        ("CI flaky: payments test timeout", 0),
        ("Payments test timeout in CI, flaky", 1),
        ("flaky CI payments test timeout", 2),
        ("Unrelated logo request", 3),
    ]
    .into_iter()
    .enumerate()
    {
        let actor = format!("u{n}");
        i.accept(&ev("github", "acme/shop", &actor, "fix-bug", title, &format!("k{n}"), T0 + at * 60_000)).unwrap();
    }
    i
}

#[test]
fn cluster_becomes_backlog_item_with_evidence() {
    let i = seeded();
    let items = i.queue().list_items(&[ItemStatus::Queued], 100).unwrap();
    let out = bottlenecks(&items, T0 + 10 * 60_000, HOUR, 3);
    assert_eq!(out.len(), 1, "{out:#?}");
    let b = &out[0];
    assert_eq!((b.source.as_str(), b.status), (SOURCE, ItemStatus::Backlog));
    assert_eq!(b.payload["count"], 3);
    let ev = b.payload["evidence"].as_array().unwrap();
    assert_eq!(ev.len(), 3);
    assert!(ev.iter().all(|e| e["item_id"].as_str().unwrap().starts_with("wi-") && e["source_ref"].as_str().unwrap().starts_with("acme/shop/")));
    assert!(b.dedupe_key.starts_with("bottleneck:"));
    assert!(b.workflow.is_none());
}

#[test]
fn threshold_and_window_are_respected() {
    let i = seeded();
    let items = i.queue().list_items(&[ItemStatus::Queued], 100).unwrap();
    assert!(bottlenecks(&items, T0 + 10 * 60_000, HOUR, 4).is_empty());
    // Items are older than the window.
    assert!(bottlenecks(&items, T0 + 3 * HOUR, HOUR, 3).is_empty());
}

#[test]
fn label_sets_cluster_too() {
    let i = intake_with(IntakeConfig::default());
    for n in 0..3u64 {
        let mut e = ev("github", "acme/shop", &format!("u{n}"), "fix-bug", &format!("widget {} broken", ["alpha", "bravo", "charlie"][n as usize]), &format!("l{n}"), T0 + n);
        e.labels = vec!["factory:fix-bug".into(), "area/billing".into()];
        i.accept(&e).unwrap();
    }
    let out = i.run_bottlenecks(T0 + 1000, HOUR, 3).unwrap();
    assert_eq!(out.len(), 1);
    assert!(out[0].item.payload["signature"].as_str().unwrap().starts_with("labels:area billing"));
}

#[test]
fn rerun_is_idempotent_and_new_window_is_new() {
    let i = seeded();
    let now = T0 + 10 * 60_000;
    let first = i.run_bottlenecks(now, HOUR, 3).unwrap();
    assert_eq!(first.len(), 1);
    assert!(first[0].inserted);
    let again = i.run_bottlenecks(now + 60_000, HOUR, 3).unwrap();
    assert!(again.iter().all(|o| !o.inserted && o.duplicate_of.as_deref() == Some(first[0].item.item_id.as_str())));
    let backlog = i.queue().list_items(&[ItemStatus::Backlog], 10).unwrap();
    assert_eq!(backlog.len(), 1);
    // Analysis items never feed the next analysis, and a later window gets its own key.
    let next = i.run_bottlenecks(now + HOUR, 2 * HOUR, 3).unwrap();
    assert!(next.iter().all(|o| o.item.source == SOURCE));
}

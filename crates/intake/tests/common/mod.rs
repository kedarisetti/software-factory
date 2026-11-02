#![allow(dead_code)]
use std::sync::{Arc, Mutex};

use sf_core::{Error, ItemStatus, Queue, Result, Timestamp, WorkItem};
use sf_intake::{Intake, IntakeConfig, IntakeEvent, PermissionTable};

/// Minimal in-memory queue honoring the `Queue` contract.
#[derive(Default)]
pub struct MemQueue(Mutex<Vec<WorkItem>>);

impl Queue for MemQueue {
    fn enqueue(&self, item: &WorkItem) -> Result<(WorkItem, bool)> {
        let mut v = self.0.lock().unwrap();
        if let Some(e) = v.iter().find(|i| i.dedupe_key == item.dedupe_key) {
            return Ok((e.clone(), false));
        }
        v.push(item.clone());
        Ok((item.clone(), true))
    }
    fn get_item(&self, id: &str) -> Result<Option<WorkItem>> {
        Ok(self.0.lock().unwrap().iter().find(|i| i.item_id == id).cloned())
    }
    fn list_items(&self, statuses: &[ItemStatus], limit: usize) -> Result<Vec<WorkItem>> {
        let mut v: Vec<_> = self.0.lock().unwrap().iter().filter(|i| statuses.contains(&i.status)).cloned().collect();
        v.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.received_at.cmp(&b.received_at)));
        v.truncate(limit);
        Ok(v)
    }
    fn update_item(&self, item: &WorkItem) -> Result<()> {
        let mut v = self.0.lock().unwrap();
        let slot = v.iter_mut().find(|i| i.item_id == item.item_id).ok_or_else(|| Error::NotFound(item.item_id.clone()))?;
        *slot = item.clone();
        Ok(())
    }
    fn count_items_since(&self, source: &str, actor: Option<&str>, since: Timestamp) -> Result<u64> {
        let v = self.0.lock().unwrap();
        Ok(v.iter().filter(|i| i.source == source && actor.is_none_or(|a| i.actor == a) && i.received_at >= since).count() as u64)
    }
}

pub fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

pub fn table() -> PermissionTable {
    // Chat-ness comes from the source config, never from a name: the loader passes it in.
    PermissionTable::from_json_with_chat(&fixture("permissions.json"), &["slack".to_string()]).unwrap()
}

pub fn intake_with(cfg: IntakeConfig) -> Intake {
    Intake::new(Arc::new(MemQueue::default()), table(), cfg)
}

pub fn ev(source: &str, channel: &str, actor: &str, wf: &str, text: &str, key: &str, at: Timestamp) -> IntakeEvent {
    IntakeEvent {
        source: source.into(),
        source_ref: format!("{channel}/{key}"),
        dedupe_key: key.into(),
        actor: actor.into(),
        channel: channel.into(),
        title: text.lines().next().unwrap_or("").into(),
        text: text.into(),
        labels: vec![],
        requested_workflow: (!wf.is_empty()).then(|| wf.to_string()),
        received_at: at,
    }
}

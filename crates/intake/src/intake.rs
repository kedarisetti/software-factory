//! `Intake::accept` (TRG-2, TRG-3): workflow -> permission -> near-dup -> rate limit -> priority -> enqueue.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;
use sf_core::{ItemStatus, Queue, Result, Timestamp, WorkItem};

use crate::event::{normalise, short_hash, IntakeEvent};
use crate::permissions::{Permission, PermissionTable};
use crate::priority::{PriorityConfig, Scored};

const ALL: [ItemStatus; 6] =
    [ItemStatus::Queued, ItemStatus::Started, ItemStatus::Rejected, ItemStatus::RateLimited, ItemStatus::Backlog, ItemStatus::Done];
/// How many recent items the near-duplicate scan reads (the Queue has no lookup by content).
const NEAR_DUP_SCAN: usize = 1000;

/// Max items per window (TRG-2).
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimits {
    pub window_ms: u64,
    pub per_source: BTreeMap<String, u64>,
    /// Applies to each actor within a source.
    pub per_actor: Option<u64>,
}

impl Default for RateLimits {
    fn default() -> Self {
        RateLimits { window_ms: 3_600_000, per_source: BTreeMap::new(), per_actor: Some(20) }
    }
}

#[derive(Debug, Clone)]
pub struct IntakeConfig {
    pub near_dup_window_ms: u64,
    pub rate: RateLimits,
    pub priority: PriorityConfig,
}

impl Default for IntakeConfig {
    fn default() -> Self {
        IntakeConfig { near_dup_window_ms: 600_000, rate: RateLimits::default(), priority: PriorityConfig::default() }
    }
}

/// What `accept` did. For a duplicate, `item` is the existing one and `inserted` is false.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub item: WorkItem,
    pub inserted: bool,
    pub status: ItemStatus,
    pub reason: Option<String>,
    pub duplicate_of: Option<String>,
}

pub struct Intake {
    queue: Arc<dyn Queue>,
    perms: PermissionTable,
    cfg: IntakeConfig,
}

/// Stable id from the dedupe key, so a duplicate never mints a second id.
pub fn item_id(dedupe_key: &str) -> String {
    format!("wi-{}", short_hash(&[dedupe_key], 16))
}

/// Near-duplicate key: same source, channel and actor saying the same normalised thing.
pub fn content_key(ev: &IntakeEvent) -> String {
    let body = normalise(&format!("{} {}", ev.title, ev.text));
    short_hash(&[&ev.source, &ev.channel, &ev.actor, &body], 32)
}

impl Intake {
    pub fn new(queue: Arc<dyn Queue>, perms: PermissionTable, cfg: IntakeConfig) -> Self {
        Intake { queue, perms, cfg }
    }

    pub fn queue(&self) -> &Arc<dyn Queue> {
        &self.queue
    }

    pub fn accept(&self, ev: &IntakeEvent) -> Result<Outcome> {
        let Some(wf) = ev.requested_workflow.clone().filter(|w| !w.is_empty()) else {
            return self.store(ev, None, Scored::default(), ItemStatus::Rejected, Some("no workflow requested".into()));
        };
        if let Permission::Rejected(why) = self.perms.decide(ev, &wf) {
            return self.store(ev, Some(wf), Scored::default(), ItemStatus::Rejected, Some(why));
        }
        if let Some(dup) = self.near_duplicate(ev)? {
            let reason = Some(format!("near-duplicate of {}", dup.item_id));
            return Ok(Outcome { status: dup.status, duplicate_of: Some(dup.item_id.clone()), item: dup, inserted: false, reason });
        }
        let scored = self.cfg.priority.score(ev);
        match self.rate_limited(ev)? {
            Some(why) => self.store(ev, Some(wf), scored, ItemStatus::RateLimited, Some(why)),
            None => self.store(ev, Some(wf), scored, ItemStatus::Queued, None),
        }
    }

    fn near_duplicate(&self, ev: &IntakeEvent) -> Result<Option<WorkItem>> {
        let key = content_key(ev);
        let window = self.cfg.near_dup_window_ms;
        let found = self.queue.list_items(&ALL, NEAR_DUP_SCAN)?.into_iter().filter(|i| {
            i.source == ev.source
                && i.status != ItemStatus::Rejected
                && i.payload.get("content_key").and_then(|k| k.as_str()) == Some(key.as_str())
                && i.received_at.abs_diff(ev.received_at) <= window
        });
        Ok(found.min_by_key(|i| i.received_at))
    }

    fn rate_limited(&self, ev: &IntakeEvent) -> Result<Option<String>> {
        let r = &self.cfg.rate;
        let since: Timestamp = ev.received_at.saturating_sub(r.window_ms);
        if let Some(max) = r.per_source.get(&ev.source) {
            let n = self.queue.count_items_since(&ev.source, None, since)?;
            if n >= *max {
                return Ok(Some(format!("source {} over limit: {n} of {max} per window", ev.source)));
            }
        }
        if let Some(max) = r.per_actor {
            let n = self.queue.count_items_since(&ev.source, Some(&ev.actor), since)?;
            if n >= max {
                return Ok(Some(format!("actor {} over limit: {n} of {max} per window", ev.actor)));
            }
        }
        Ok(None)
    }

    fn store(&self, ev: &IntakeEvent, workflow: Option<String>, scored: Scored, status: ItemStatus, reason: Option<String>) -> Result<Outcome> {
        let item = WorkItem {
            item_id: item_id(&ev.dedupe_key),
            source: ev.source.clone(),
            source_ref: ev.source_ref.clone(),
            dedupe_key: ev.dedupe_key.clone(),
            actor: ev.actor.clone(),
            channel: ev.channel.clone(),
            workflow,
            priority: scored.score,
            status,
            reason,
            run_id: None,
            payload: json!({
                "title": ev.title, "text": ev.text, "labels": ev.labels,
                "requested_workflow": ev.requested_workflow,
                "content_key": content_key(ev),
                "priority_rules": scored.rules,
                "untrusted": true,
            }),
            received_at: ev.received_at,
            updated_at: ev.received_at,
        };
        let (stored, inserted) = self.queue.enqueue(&item)?;
        let duplicate_of = (!inserted).then(|| stored.item_id.clone());
        let reason = if inserted { stored.reason.clone() } else { Some(format!("duplicate of {}", stored.item_id)) };
        Ok(Outcome { status: stored.status, item: stored, inserted, reason, duplicate_of })
    }

    /// TRG-4: enqueue backlog items for recurring bottlenecks. Safe to re-run (same window, same keys).
    pub fn run_bottlenecks(&self, now: Timestamp, window_ms: u64, min_count: usize) -> Result<Vec<Outcome>> {
        let items = self.queue.list_items(&ALL, 10_000)?;
        crate::bottleneck::bottlenecks(&items, now, window_ms, min_count)
            .into_iter()
            .map(|b| {
                let (stored, inserted) = self.queue.enqueue(&b)?;
                let duplicate_of = (!inserted).then(|| stored.item_id.clone());
                Ok(Outcome { status: stored.status, reason: stored.reason.clone(), item: stored, inserted, duplicate_of })
            })
            .collect()
    }
}

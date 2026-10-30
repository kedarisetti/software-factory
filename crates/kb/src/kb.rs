//! `Kb`: the file-backed implementation of `Knowledge` (KIF-1 to KIF-7).

use std::path::{Path, PathBuf};

use crate::*;

/// Clock in unix milliseconds; injectable so TTL tests need no sleeping.
pub type Now = fn() -> Timestamp;

pub struct Kb {
    root: PathBuf,
    index: Box<dyn Index>,
    now: Now,
}

impl Kb {
    /// Open (creating if needed) the store at `root`. Does not rebuild the index;
    /// call `rebuild_index` when the cache is new or suspect.
    pub fn open(root: impl Into<PathBuf>, index: Box<dyn Index>) -> Result<Kb> {
        let _ = (root.into(), index);
        todo!("builder: kb-store")
    }

    /// Replace the clock (tests).
    pub fn with_now(mut self, now: Now) -> Kb {
        self.now = now;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Re-index every entry file (RET-5 nightly rebuild, or a fresh cache).
    pub fn rebuild_index(&self) -> Result<usize> {
        let _ = (&self.index, self.now);
        todo!("builder: kb-store")
    }

    /// KIF-8 handshake for front doors.
    pub fn check_version(expected: u32) -> Result<()> {
        if expected == CONTRACT_VERSION {
            Ok(())
        } else {
            Err(Error::Version { expected, actual: CONTRACT_VERSION })
        }
    }
}

impl Knowledge for Kb {
    fn search(&self, _caller: &Caller, _query: &str, _task_class: Option<&str>, _top_k: usize) -> Result<Vec<Hit>> {
        todo!("builder: kb-store")
    }
    fn get(&self, _caller: &Caller, _id: &str) -> Result<Entry> {
        todo!("builder: kb-store")
    }
    fn add(&self, _caller: &Caller, _entry: NewEntry) -> Result<Entry> {
        todo!("builder: kb-store")
    }
    fn learn(&self, _caller: &Caller, _record: LearningRecord) -> Result<Learned> {
        todo!("builder: kb-store")
    }
    fn feedback(&self, _caller: &Caller, _id: &str, _verdict: Verdict, _note: Option<&str>) -> Result<Entry> {
        todo!("builder: kb-store")
    }
    fn check_anchors(&self, _caller: &Caller) -> Result<AnchorReport> {
        todo!("builder: kb-store")
    }
}

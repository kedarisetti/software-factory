//! The connector-neutral event (TRG-1). Its text is untrusted content (SBX-3).

use serde::{Deserialize, Serialize};
use sf_core::Timestamp;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntakeEvent {
    /// Configured source name.
    pub source: String,
    pub source_ref: String,
    /// Exact-duplicate key (TRG-2).
    pub dedupe_key: String,
    pub actor: String,
    pub channel: String,
    pub title: String,
    pub text: String,
    pub labels: Vec<String>,
    pub requested_workflow: Option<String>,
    pub received_at: Timestamp,
}

/// Lowercase alphanumeric words joined by single spaces.
pub fn normalise(s: &str) -> String {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect::<Vec<_>>().join(" ")
}

/// First `n` hex chars of sha256 over NUL-separated parts.
pub fn short_hash(parts: &[&str], n: usize) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    crate::sig::hex(&h.finalize())[..n].to_string()
}

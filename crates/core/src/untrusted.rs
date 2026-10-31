//! Content from tickets, Slack, email, the web and tool output is data, never instructions (SBX-3).
//!
//! Two layers. `Untrusted::render` frames content so a model sees where data starts and ends.
//! `scan` flags text that reads like instructions to the agent; the engine escalates on a hit.
//! Neither is the safety boundary: gates, egress, credential scoping and filesystem isolation
//! hold even when a model obeys injected text. The injection suite tests exactly that.

use serde::{Deserialize, Serialize};

/// A piece of untrusted content and where it came from (`ticket:ENG-12`, `web:https://...`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Untrusted {
    pub source: String,
    pub content: String,
}

/// One instruction-like span found by `scan`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Short rule name, e.g. `override-instructions`, `role-switch`, `exfiltrate-secret`.
    pub rule: String,
    /// The matched text, at most 80 chars.
    pub excerpt: String,
}

impl Untrusted {
    pub fn new(source: impl Into<String>, content: impl Into<String>) -> Self {
        Untrusted { source: source.into(), content: content.into() }
    }

    /// The content inside a fenced data block whose boundary token does not occur in the content,
    /// headed by the source and a line saying it is data and not instructions.
    pub fn render(&self) -> String {
        todo!("M3 builder")
    }

    /// sha256 hex of `source` NUL `content`; a human waiver for a flagged input is bound to it.
    pub fn digest(&self) -> String {
        todo!("M3 builder")
    }
}

/// Instruction-like spans in `text`. Normalises first (NFKC-ish folding of full-width ASCII,
/// zero-width and Unicode tag characters stripped, case folded) so trivial obfuscation does not
/// hide a hit. Hidden characters themselves are a finding (`hidden-text`).
pub fn scan(text: &str) -> Vec<Finding> {
    let _ = text;
    todo!("M3 builder")
}

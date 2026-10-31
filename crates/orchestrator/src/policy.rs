//! Per-workflow safety policy: egress allowlist, credential scopes, approval gates (SBX-1, SBX-2, GOV).

use std::collections::HashMap;

use sf_core::{ActionKind, HostRule};

/// Whether an action needs a named human before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Auto,
    Human,
}

/// What a workflow may reach, lease and do without asking.
#[derive(Debug, Clone)]
pub struct Policy {
    /// Hosts tool calls may reach (SBX-1). Empty = no network.
    pub egress: Vec<HostRule>,
    /// Credential scopes this workflow may lease (SBX-2).
    pub scopes: Vec<String>,
    pub lease_ttl_ms: u64,
    /// Overrides of the default gate table. They can only tighten.
    pub gates: HashMap<ActionKind, Gate>,
    /// Scan untrusted input for instructions and escalate on a hit (SBX-3).
    pub scan_untrusted: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Policy { egress: vec![], scopes: vec![], lease_ttl_ms: 15 * 60 * 1000, gates: HashMap::new(), scan_untrusted: true }
    }
}

impl Policy {
    pub fn egress(mut self, rules: Vec<HostRule>) -> Self {
        self.egress = rules;
        self
    }
    pub fn scopes(mut self, scopes: &[&str]) -> Self {
        self.scopes = scopes.iter().map(|s| s.to_string()).collect();
        self
    }
    pub fn lease_ttl_ms(mut self, ms: u64) -> Self {
        self.lease_ttl_ms = ms;
        self
    }
    pub fn gate_override(mut self, kind: ActionKind, gate: Gate) -> Self {
        self.gates.insert(kind, gate);
        self
    }
    pub fn scan_untrusted(mut self, on: bool) -> Self {
        self.scan_untrusted = on;
        self
    }

    /// The gate for `kind`: the spec table, tightened by overrides. Loosening is ignored.
    pub fn gate(&self, kind: &ActionKind) -> Gate {
        let default = match kind {
            ActionKind::DraftPr | ActionKind::Branch | ActionKind::OwnTicketComment => Gate::Auto,
            _ => Gate::Human,
        };
        match (default, self.gates.get(kind)) {
            (Gate::Auto, Some(Gate::Human)) => Gate::Human,
            _ => default,
        }
    }
}

//! Static credential issuer for tests and single-host setups (SBX-2).

use std::collections::HashMap;
use std::sync::Arc;

use sf_core::{Clock, Credentials, Error, Lease, Result};

/// Scope -> secret value. Leases live `min(ttl, max_ttl_ms)`; optionally only for runs with a prefix.
pub struct StaticCredentials {
    secrets: HashMap<String, String>,
    max_ttl_ms: u64,
    run_prefix: Option<String>,
    clock: Arc<dyn Clock>,
}

impl StaticCredentials {
    pub fn new(clock: Arc<dyn Clock>, max_ttl_ms: u64) -> Self {
        StaticCredentials { secrets: HashMap::new(), max_ttl_ms, run_prefix: None, clock }
    }
    pub fn secret(mut self, scope: &str, value: &str) -> Self {
        self.secrets.insert(scope.into(), value.into());
        self
    }
    /// Only issue to runs whose id starts with `prefix`.
    pub fn run_prefix(mut self, prefix: &str) -> Self {
        self.run_prefix = Some(prefix.into());
        self
    }
}

impl Credentials for StaticCredentials {
    fn lease(&self, scope: &str, run_id: &str, ttl_ms: u64) -> Result<Lease> {
        if self.run_prefix.as_deref().is_some_and(|p| !run_id.starts_with(p)) {
            return Err(Error::Forbidden(format!("scope {scope} not issuable to run {run_id}")));
        }
        let value = self.secrets.get(scope).ok_or_else(|| Error::Forbidden(format!("unknown scope {scope}")))?;
        let expires_at = self.clock.now().saturating_add(ttl_ms.min(self.max_ttl_ms));
        Ok(Lease { scope: scope.into(), value: value.clone(), expires_at })
    }
}

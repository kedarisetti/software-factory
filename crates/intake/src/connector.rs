//! Shared connector plumbing: case-insensitive headers and the typed error (TRG-1).

#[derive(Debug, Clone, Default)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    pub fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Headers(pairs.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.to_string())).collect())
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        let n = name.to_ascii_lowercase();
        self.0.iter().find(|(k, _)| *k == n).map(|(_, v)| v.as_str())
    }

    pub fn require(&self, name: &'static str) -> Result<&str, ConnectorError> {
        self.get(name).ok_or(ConnectorError::MissingHeader(name))
    }
}

/// Why a webhook produced no event. `Unsupported` is not a failure: answer 200 and move on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectorError {
    #[error("bad signature")]
    BadSignature,
    #[error("missing header {0}")]
    MissingHeader(&'static str),
    #[error("stale or future timestamp ({skew_secs}s from now)")]
    StaleTimestamp { skew_secs: i64 },
    #[error("unsupported event, ignored: {0}")]
    Unsupported(String),
    #[error("malformed body: {0}")]
    Malformed(String),
}

impl ConnectorError {
    /// HTTP status the webhook endpoint should answer.
    pub fn http_status(&self) -> u16 {
        match self {
            ConnectorError::Unsupported(_) => 200,
            ConnectorError::BadSignature | ConnectorError::StaleTimestamp { .. } => 401,
            ConnectorError::MissingHeader(_) | ConnectorError::Malformed(_) => 400,
        }
    }
}

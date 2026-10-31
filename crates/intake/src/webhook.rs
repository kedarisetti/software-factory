//! Generic signed webhook (TRG-1): HMAC-SHA256 check described in the config, optional replay
//! window and challenge echo, then the source's `Pipeline`. No vendor is built in; Slack, GitHub and
//! others are config presets (docs/intake.md).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sf_core::Timestamp;

use crate::connector::{ConnectorError, Headers};
use crate::pipeline::{matches, validate_match, Ctx, Mapped, Match, Pipeline, Template};
use crate::sig;

pub const DEFAULT_MAX_SKEW_SECS: i64 = 300;

/// How the request is signed: `header` carries `prefix` + hex HMAC-SHA256 of `signed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verify {
    /// Env var NAME holding the shared secret.
    pub secret_env: String,
    pub header: String,
    #[serde(default)]
    pub prefix: String,
    /// What is signed: text with `{header:Name}` holes and `{body}` for the raw body. Default `{body}`.
    #[serde(default)]
    pub signed: Option<String>,
    /// Header with unix seconds; when set, requests outside `max_skew_s` are refused (replay).
    #[serde(default)]
    pub timestamp_header: Option<String>,
    #[serde(default)]
    pub max_skew_s: Option<i64>,
}

/// Answer `reply` (a reference) as text when `when` matches, e.g. Slack's url_verification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Challenge {
    pub when: Match,
    pub reply: Template,
}

#[derive(Debug, Clone)]
pub struct Webhook {
    pub source: String,
    pub secret: String,
    pub verify: Verify,
    pub challenge: Option<Challenge>,
    pub pipeline: Pipeline,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WebhookOutcome {
    Challenge(String),
    Events(Mapped),
}

impl Challenge {
    pub fn validate(&self) -> Result<(), String> {
        validate_match(&self.when, "challenge.when")?;
        self.reply.validate().map_err(|e| format!("challenge.reply: {e}"))
    }
}

impl Verify {
    pub fn validate(&self) -> Result<(), String> {
        if self.header.trim().is_empty() {
            return Err("verify.header is required".into());
        }
        if let Some(s) = &self.signed {
            if !s.contains("{body}") {
                return Err("verify.signed must include {body}".into());
            }
            let probe = s.replace("{body}", "");
            Template(probe).validate().map_err(|e| format!("verify.signed: {e}"))?;
        }
        if self.max_skew_s.is_some_and(|s| s <= 0) {
            return Err("verify.max_skew_s must be positive".into());
        }
        Ok(())
    }

    /// The signed bytes: `signed` with header holes filled and `{body}` replaced by the raw body.
    fn base(&self, headers: &Headers, body: &[u8]) -> Result<Vec<u8>, ConnectorError> {
        let Some(spec) = &self.signed else { return Ok(body.to_vec()) };
        let mut out = Vec::new();
        let mut rest = spec.as_str();
        while let Some(o) = rest.find('{') {
            out.extend_from_slice(rest[..o].as_bytes());
            let c = rest[o..].find('}').map(|c| c + o).ok_or_else(|| ConnectorError::Malformed("bad signed template".into()))?;
            match &rest[o + 1..c] {
                "body" => out.extend_from_slice(body),
                r => {
                    let h = r.strip_prefix("header:").ok_or_else(|| ConnectorError::Malformed("bad signed template".into()))?;
                    let v = headers.get(h).ok_or_else(|| ConnectorError::Malformed(format!("missing header {h}")))?;
                    out.extend_from_slice(v.as_bytes());
                }
            }
            rest = &rest[c + 1..];
        }
        out.extend_from_slice(rest.as_bytes());
        Ok(out)
    }

    pub fn check(&self, secret: &str, headers: &Headers, body: &[u8], now_ms: Timestamp) -> Result<(), ConnectorError> {
        let provided = headers.get(&self.header).ok_or_else(|| ConnectorError::Malformed(format!("missing header {}", self.header)))?;
        if let Some(th) = &self.timestamp_header {
            let raw = headers.get(th).ok_or_else(|| ConnectorError::Malformed(format!("missing header {th}")))?;
            let ts: i64 = raw.trim().parse().map_err(|_| ConnectorError::Malformed("bad timestamp header".into()))?;
            let skew_secs = (now_ms / 1000) as i64 - ts;
            if skew_secs.abs() > self.max_skew_s.unwrap_or(DEFAULT_MAX_SKEW_SECS) {
                return Err(ConnectorError::StaleTimestamp { skew_secs });
            }
        }
        let base = self.base(headers, body)?;
        match provided.strip_prefix(self.prefix.as_str()) {
            Some(h) if sig::verify_hex(secret.as_bytes(), &base, h) => Ok(()),
            _ => Err(ConnectorError::BadSignature),
        }
    }
}

/// Verify, then challenge, then the pipeline. A body that maps to no events is `Unsupported` (200).
pub fn handle(w: &Webhook, headers: &Headers, body: &[u8], now_ms: Timestamp) -> Result<WebhookOutcome, ConnectorError> {
    w.verify.check(&w.secret, headers, body, now_ms)?;
    let doc: Value = serde_json::from_slice(body).map_err(|e| ConnectorError::Malformed(e.to_string()))?;
    if let Some(c) = &w.challenge {
        let ctx = Ctx { source: &w.source, item: &doc, headers: Some(headers) };
        if matches(&c.when, &ctx) {
            return Ok(WebhookOutcome::Challenge(c.reply.render(&ctx)));
        }
    }
    let mapped = w.pipeline.events(&w.source, &doc, Some(headers), now_ms).map_err(ConnectorError::Malformed)?;
    if mapped.events.is_empty() {
        return Err(ConnectorError::Unsupported(format!("no events ({} ignored, {} skipped)", mapped.ignored, mapped.skipped)));
    }
    Ok(WebhookOutcome::Events(mapped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hook(verify: Value, challenge: Option<Value>) -> Webhook {
        Webhook {
            source: "chat".into(),
            secret: "s3cret".into(),
            verify: serde_json::from_value(verify).unwrap(),
            challenge: challenge.map(|c| serde_json::from_value(c).unwrap()),
            pipeline: serde_json::from_value(json!({"map": {"id": "/id", "title": "/t", "dedupe": "{header:X-Delivery}"},
                "workflow": [{"default": "demo"}]})).unwrap(),
        }
    }

    fn sign(base: &[u8]) -> String {
        sig::hex(&sig::hmac_sha256(b"s3cret", base))
    }

    #[test]
    fn body_signature_like_github() {
        let w = hook(json!({"secret_env": "S", "header": "X-Hub-Signature-256", "prefix": "sha256="}), None);
        let body = br#"{"id": 1, "t": "hello"}"#;
        let good = Headers::from_pairs(&[("X-Hub-Signature-256", &format!("sha256={}", sign(body))), ("X-Delivery", "d1")]);
        match handle(&w, &good, body, 0).unwrap() {
            WebhookOutcome::Events(m) => assert_eq!(m.events[0].dedupe_key, "chat:d1"),
            o => panic!("{o:?}"),
        }
        let bad = Headers::from_pairs(&[("X-Hub-Signature-256", "sha256=00")]);
        assert_eq!(handle(&w, &bad, body, 0), Err(ConnectorError::BadSignature));
        assert!(matches!(handle(&w, &Headers::default(), body, 0), Err(ConnectorError::Malformed(_))));
    }

    #[test]
    fn templated_signature_replay_window_and_challenge_like_slack() {
        let w = hook(
            json!({"secret_env": "S", "header": "X-Sig", "prefix": "v0=", "signed": "v0:{header:X-Ts}:{body}", "timestamp_header": "X-Ts"}),
            Some(json!({"when": {"/type": "url_verification"}, "reply": "/challenge"})),
        );
        let now_ms = 1_700_000_000_000u64;
        let post = |body: &[u8], ts: i64| {
            let base = [format!("v0:{ts}:").as_bytes(), body].concat();
            let h = Headers::from_pairs(&[("X-Sig", &format!("v0={}", sign(&base))), ("X-Ts", &ts.to_string())]);
            handle(&w, &h, body, now_ms)
        };
        let now = (now_ms / 1000) as i64;
        assert_eq!(post(br#"{"type": "url_verification", "challenge": "abc"}"#, now), Ok(WebhookOutcome::Challenge("abc".into())));
        assert!(matches!(post(br#"{"id": 2, "t": "x"}"#, now - 301), Err(ConnectorError::StaleTimestamp { .. })));
        assert!(matches!(post(br#"{"id": 2, "t": "x"}"#, now), Ok(WebhookOutcome::Events(_))));
        assert!(matches!(post(br#"{"nothing": 1}"#, now), Err(ConnectorError::Unsupported(_))));
    }

    #[test]
    fn verify_validation() {
        let v = |j: Value| serde_json::from_value::<Verify>(j).unwrap().validate();
        assert!(v(json!({"secret_env": "S", "header": ""})).is_err());
        assert!(v(json!({"secret_env": "S", "header": "H", "signed": "v0:{header:T}"})).is_err());
        assert!(v(json!({"secret_env": "S", "header": "H", "signed": "{nope}:{body}"})).is_err());
        assert!(v(json!({"secret_env": "S", "header": "H", "signed": "v0:{header:T}:{body}"})).is_ok());
    }
}

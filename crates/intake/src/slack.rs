//! Slack Events API connector (TRG-1): v0 signature, replay window, command syntax.

use std::collections::BTreeMap;

use sf_core::Timestamp;

use crate::connector::{req, str_at, ConnectorError, Headers};
use crate::event::IntakeEvent;
use crate::sig;

/// Reject requests whose timestamp is further than this from now (replay).
pub const MAX_SKEW_SECS: i64 = 300;

#[derive(Debug, Clone)]
pub struct SlackConfig {
    /// Configured source name: the event's `source`, and the prefix of its dedupe key.
    pub source: String,
    pub signing_secret: String,
    pub bot_user_id: String,
    /// Channel id -> workflow used when the message names none.
    pub channel_defaults: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SlackOutcome {
    /// Answer with this challenge string.
    Challenge(String),
    Event(IntakeEvent),
}

/// Checks the timestamp window and `X-Slack-Signature` over `v0:{ts}:{body}`.
pub fn verify(secret: &str, headers: &Headers, body: &[u8], now_ms: Timestamp) -> Result<(), ConnectorError> {
    let ts_raw = headers.require("X-Slack-Request-Timestamp")?;
    let provided = headers.require("X-Slack-Signature")?;
    let ts: i64 = ts_raw.trim().parse().map_err(|_| ConnectorError::Malformed("bad timestamp header".into()))?;
    let skew_secs = (now_ms / 1000) as i64 - ts;
    if skew_secs.abs() > MAX_SKEW_SECS {
        return Err(ConnectorError::StaleTimestamp { skew_secs });
    }
    let mut base = format!("v0:{ts_raw}:").into_bytes();
    base.extend_from_slice(body);
    match provided.strip_prefix("v0=") {
        Some(h) if sig::verify_hex(secret.as_bytes(), &base, h) => Ok(()),
        _ => Err(ConnectorError::BadSignature),
    }
}

pub fn handle(cfg: &SlackConfig, headers: &Headers, body: &[u8], now_ms: Timestamp) -> Result<SlackOutcome, ConnectorError> {
    verify(&cfg.signing_secret, headers, body, now_ms)?;
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| ConnectorError::Malformed(e.to_string()))?;
    match str_at(&v, "type") {
        Some("url_verification") => Ok(SlackOutcome::Challenge(req(&v, "challenge")?.to_string())),
        Some("event_callback") => event(cfg, &v, now_ms).map(SlackOutcome::Event),
        other => Err(ConnectorError::Unsupported(format!("slack payload type {other:?}"))),
    }
}

fn event(cfg: &SlackConfig, v: &serde_json::Value, now_ms: Timestamp) -> Result<IntakeEvent, ConnectorError> {
    let ev = v.get("event").ok_or_else(|| ConnectorError::Malformed("missing event".into()))?;
    let kind = str_at(ev, "type").unwrap_or("");
    if !matches!(kind, "message" | "app_mention") {
        return Err(ConnectorError::Unsupported(format!("slack event type {kind}")));
    }
    if ev.get("bot_id").is_some() || ev.get("subtype").is_some() {
        return Err(ConnectorError::Unsupported("bot message, edit or subtype".into()));
    }
    let text = req(ev, "text")?;
    let user = req(ev, "user")?;
    let mention = format!("<@{}>", cfg.bot_user_id);
    if user == cfg.bot_user_id {
        return Err(ConnectorError::Unsupported("own message".into()));
    }
    // Slack sends both `message` and `app_mention` for a mention; only the latter counts.
    if kind == "message" && text.contains(&mention) {
        return Err(ConnectorError::Unsupported("mention handled as app_mention".into()));
    }
    let (channel, ts, event_id) = (req(ev, "channel")?, req(ev, "ts")?, req(v, "event_id")?);
    let (named, rest) = parse_command(text, &mention);
    let requested_workflow = named.or_else(|| cfg.channel_defaults.get(channel).cloned());
    Ok(IntakeEvent {
        source: cfg.source.clone(),
        source_ref: format!("{channel}/{ts}"),
        dedupe_key: format!("{}:{event_id}", cfg.source),
        actor: user.into(),
        channel: channel.into(),
        title: rest.lines().next().unwrap_or("").chars().take(80).collect(),
        text: rest,
        labels: vec![],
        requested_workflow,
        received_at: now_ms,
    })
}

/// `<@BOT> <workflow> <rest>` -> (Some(workflow), rest); anything else -> (None, text).
pub fn parse_command(text: &str, mention: &str) -> (Option<String>, String) {
    let t = text.trim();
    let Some(after) = t.strip_prefix(mention) else { return (None, t.to_string()) };
    let after = after.trim();
    let (tok, rest) = after.split_once(char::is_whitespace).unwrap_or((after, ""));
    if !tok.is_empty() && tok.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        (Some(tok.to_string()), rest.trim().to_string())
    } else {
        (None, after.to_string())
    }
}

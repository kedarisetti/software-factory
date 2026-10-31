mod common;
use std::collections::BTreeMap;

use common::fixture;
use sf_intake::github::{self, GithubConfig};
use sf_intake::sig::hmac_sha256_hex;
use sf_intake::slack::{self, SlackConfig, SlackOutcome};
use sf_intake::{ConnectorError, Headers};

const NOW_MS: u64 = 1_700_000_000_000;
const SECRET: &str = "8f742231b10e8888abcd99yyyzzz85a5";

fn slack_cfg() -> SlackConfig {
    SlackConfig {
        signing_secret: SECRET.into(),
        bot_user_id: "U0LAN0Z89".into(),
        channel_defaults: BTreeMap::from([("C0DEFAULT1".to_string(), "triage".to_string())]),
    }
}

fn slack_headers(body: &str, ts: &str, secret: &str) -> Headers {
    let sig = format!("v0={}", hmac_sha256_hex(secret.as_bytes(), format!("v0:{ts}:{body}").as_bytes()));
    Headers::from_pairs(&[("X-Slack-Request-Timestamp", ts), ("X-Slack-Signature", &sig)])
}

fn slack_ok(name: &str) -> Result<SlackOutcome, ConnectorError> {
    let body = fixture(name);
    slack::handle(&slack_cfg(), &slack_headers(&body, "1700000000", SECRET), body.as_bytes(), NOW_MS)
}

fn slack_event(name: &str) -> sf_intake::IntakeEvent {
    match slack_ok(name).unwrap() {
        SlackOutcome::Event(e) => e,
        o => panic!("{o:?}"),
    }
}

#[test]
fn slack_url_verification_returns_challenge() {
    let SlackOutcome::Challenge(c) = slack_ok("slack_url_verification.json").unwrap() else { panic!() };
    assert!(c.starts_with("3eZbrw1a"));
}

#[test]
fn slack_app_mention_parses_command() {
    let e = slack_event("slack_app_mention.json");
    assert_eq!(e.requested_workflow.as_deref(), Some("fix-bug"));
    assert_eq!(e.text, "login page 500s after the password reset flow");
    assert_eq!((e.source.as_str(), e.actor.as_str(), e.channel.as_str()), ("slack", "U061F7AUR", "C024BE91L"));
    assert_eq!(e.dedupe_key, "slack:Ev0PV52K21");
    assert_eq!(e.source_ref, "C024BE91L/1700000000.000100");
    assert_eq!(e.received_at, NOW_MS);
}

#[test]
fn slack_channel_default_workflow() {
    let e = slack_event("slack_message.json");
    assert_eq!(e.requested_workflow.as_deref(), Some("triage"));
    assert_eq!(e.text, "the nightly build is red again");
}

#[test]
fn slack_retry_has_same_dedupe_key() {
    let body = fixture("slack_app_mention.json");
    let mut h = slack_headers(&body, "1700000010", SECRET);
    h = Headers::from_pairs(&[
        ("x-slack-request-timestamp", "1700000010"),
        ("x-slack-signature", h.get("X-Slack-Signature").unwrap()),
        ("X-Slack-Retry-Num", "1"),
        ("X-Slack-Retry-Reason", "http_timeout"),
    ]);
    let SlackOutcome::Event(retry) = slack::handle(&slack_cfg(), &h, body.as_bytes(), NOW_MS + 11_000).unwrap() else { panic!() };
    assert_eq!(retry.dedupe_key, slack_event("slack_app_mention.json").dedupe_key);
}

#[test]
fn slack_ignores_bots_edits_other_types_and_double_mentions() {
    for f in ["slack_bot_message.json", "slack_message_changed.json", "slack_message_with_mention.json"] {
        let e = slack_ok(f).unwrap_err();
        assert!(matches!(e, ConnectorError::Unsupported(_)), "{f}: {e:?}");
        assert_eq!(e.http_status(), 200);
    }
    let body = r#"{"type":"event_callback","event_id":"E1","event":{"type":"reaction_added"}}"#;
    let r = slack::handle(&slack_cfg(), &slack_headers(body, "1700000000", SECRET), body.as_bytes(), NOW_MS);
    assert!(matches!(r, Err(ConnectorError::Unsupported(_))));
}

#[test]
fn slack_signature_edge_cases() {
    let body = fixture("slack_app_mention.json");
    let cfg = slack_cfg();
    let good = slack_headers(&body, "1700000000", SECRET);
    // Wrong secret.
    let bad = slack_headers(&body, "1700000000", "other-secret");
    assert_eq!(slack::handle(&cfg, &bad, body.as_bytes(), NOW_MS), Err(ConnectorError::BadSignature));
    // Tampered body.
    let tampered = body.replace("login", "logout");
    assert_eq!(slack::handle(&cfg, &good, tampered.as_bytes(), NOW_MS), Err(ConnectorError::BadSignature));
    // Missing headers.
    let no_sig = Headers::from_pairs(&[("X-Slack-Request-Timestamp", "1700000000")]);
    assert_eq!(slack::handle(&cfg, &no_sig, body.as_bytes(), NOW_MS), Err(ConnectorError::MissingHeader("X-Slack-Signature")));
    let no_ts = Headers::from_pairs(&[("X-Slack-Signature", good.get("x-slack-signature").unwrap())]);
    assert_eq!(slack::handle(&cfg, &no_ts, body.as_bytes(), NOW_MS), Err(ConnectorError::MissingHeader("X-Slack-Request-Timestamp")));
    // Wrong scheme and garbage.
    let v1 = Headers::from_pairs(&[("X-Slack-Request-Timestamp", "1700000000"), ("X-Slack-Signature", "v1=abc")]);
    assert_eq!(slack::handle(&cfg, &v1, body.as_bytes(), NOW_MS), Err(ConnectorError::BadSignature));
    // Replay window: 301 s old and 301 s in the future are refused, 300 s is fine.
    for (ts, ok) in [(1_699_999_699u64, false), (1_700_000_301, false), (1_699_999_700, true), (1_700_000_300, true)] {
        let h = slack_headers(&body, &ts.to_string(), SECRET);
        let r = slack::handle(&cfg, &h, body.as_bytes(), NOW_MS);
        assert_eq!(r.is_ok(), ok, "ts {ts}: {r:?}");
        if !ok {
            assert!(matches!(r, Err(ConnectorError::StaleTimestamp { .. })));
        }
    }
    // Bad timestamp header and malformed (but correctly signed) body.
    let h = slack_headers(&body, "soon", SECRET);
    assert!(matches!(slack::handle(&cfg, &h, body.as_bytes(), NOW_MS), Err(ConnectorError::Malformed(_))));
    let junk = "not json";
    let r = slack::handle(&cfg, &slack_headers(junk, "1700000000", SECRET), junk.as_bytes(), NOW_MS);
    assert!(matches!(r, Err(ConnectorError::Malformed(_))));
}

const GH_SECRET: &str = "It's a Secret to Everybody";

fn gh_headers(body: &str, event: &str, delivery: &str) -> Headers {
    let sig = format!("sha256={}", hmac_sha256_hex(GH_SECRET.as_bytes(), body.as_bytes()));
    Headers::from_pairs(&[("X-Hub-Signature-256", &sig), ("X-GitHub-Event", event), ("X-GitHub-Delivery", delivery)])
}

fn gh(name: &str, delivery: &str) -> Result<sf_intake::IntakeEvent, ConnectorError> {
    let body = fixture(name);
    github::handle(&GithubConfig { webhook_secret: GH_SECRET.into() }, &gh_headers(&body, "issues", delivery), body.as_bytes(), NOW_MS)
}

#[test]
fn github_opened_maps_fields() {
    let e = gh("github_issue_opened.json", "d-1").unwrap();
    assert_eq!(e.requested_workflow.as_deref(), Some("fix-bug"));
    assert_eq!(e.dedupe_key, "github:acme/shop#42:opened");
    assert_eq!((e.source_ref.as_str(), e.channel.as_str(), e.actor.as_str()), ("acme/shop#42", "acme/shop", "octocat"));
    assert_eq!(e.labels, ["bug", "factory:fix-bug", "p0"]);
    assert_eq!(e.title, "Checkout total wrong with coupon");
}

#[test]
fn github_labeled_uses_added_label_and_redelivery_dedupes() {
    let a = gh("github_issue_labeled.json", "d-1").unwrap();
    let b = gh("github_issue_labeled.json", "d-2").unwrap();
    assert_eq!(a.dedupe_key, "github:acme/shop#43:labeled:factory:docs-fix");
    assert_eq!(a.dedupe_key, b.dedupe_key);
    assert_eq!(a.requested_workflow.as_deref(), Some("docs-fix"));
    assert_eq!(a.actor, "maintainer");
    assert_eq!(a.text, "");
    assert!(matches!(gh("github_issue_labeled_other.json", "d"), Err(ConnectorError::Unsupported(_))));
}

#[test]
fn github_reopen_keys_by_delivery() {
    let a = gh("github_issue_reopened.json", "d-1").unwrap();
    assert_eq!(a.dedupe_key, "github:acme/shop#42:reopened:d-1");
    assert_eq!(a.dedupe_key, gh("github_issue_reopened.json", "d-1").unwrap().dedupe_key);
    assert_ne!(a.dedupe_key, gh("github_issue_reopened.json", "d-2").unwrap().dedupe_key);
}

#[test]
fn github_ignores_other_events_and_actions() {
    let body = fixture("github_issue_closed.json");
    let cfg = GithubConfig { webhook_secret: GH_SECRET.into() };
    assert!(matches!(github::handle(&cfg, &gh_headers(&body, "issues", "d"), body.as_bytes(), NOW_MS), Err(ConnectorError::Unsupported(_))));
    assert!(matches!(github::handle(&cfg, &gh_headers("{}", "ping", "d"), b"{}", NOW_MS), Err(ConnectorError::Unsupported(_))));
}

#[test]
fn github_signature_edge_cases() {
    let body = fixture("github_issue_opened.json");
    let cfg = GithubConfig { webhook_secret: GH_SECRET.into() };
    let good = gh_headers(&body, "issues", "d");
    let tampered = body.replace("octocat", "mallory");
    assert_eq!(github::handle(&cfg, &good, tampered.as_bytes(), NOW_MS), Err(ConnectorError::BadSignature));
    let wrong = GithubConfig { webhook_secret: "nope".into() };
    assert_eq!(github::handle(&wrong, &good, body.as_bytes(), NOW_MS), Err(ConnectorError::BadSignature));
    let none = Headers::from_pairs(&[("X-GitHub-Event", "issues")]);
    assert_eq!(github::handle(&cfg, &none, body.as_bytes(), NOW_MS), Err(ConnectorError::MissingHeader("X-Hub-Signature-256")));
    let bare = Headers::from_pairs(&[("X-Hub-Signature-256", &hmac_sha256_hex(GH_SECRET.as_bytes(), body.as_bytes())), ("X-GitHub-Event", "issues")]);
    assert_eq!(github::handle(&cfg, &bare, body.as_bytes(), NOW_MS), Err(ConnectorError::BadSignature));
    let junk = "{";
    assert!(matches!(github::handle(&cfg, &gh_headers(junk, "issues", "d"), junk.as_bytes(), NOW_MS), Err(ConnectorError::Malformed(_))));
}

#[test]
fn github_poll_normalises_issues_and_matches_webhook_key() {
    let list: Vec<serde_json::Value> = serde_json::from_str(&fixture("github_issues_list.json")).unwrap();
    let e = github::from_issue("acme/shop", &list[0], NOW_MS).unwrap();
    assert_eq!(e.dedupe_key, "github:acme/shop#42:opened");
    assert_eq!(e.dedupe_key, gh("github_issue_opened.json", "d").unwrap().dedupe_key);
    assert_eq!((e.requested_workflow.as_deref(), e.actor.as_str()), (Some("fix-bug"), "octocat"));
    assert!(matches!(github::from_issue("acme/shop", &list[1], NOW_MS), Err(ConnectorError::Unsupported(_))));
    assert!(matches!(github::from_issue("acme/shop", &list[2], NOW_MS), Err(ConnectorError::Unsupported(_))));
}

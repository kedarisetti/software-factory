//! The presets in docs/presets/ run against recorded payloads: signature, challenge, filters and
//! mapping all come from the preset JSON; no vendor is built into sf-intake.
mod common;
use std::path::Path;

use common::fixture;
use serde_json::Value;
use sf_intake::sig::hmac_sha256_hex;
use sf_intake::webhook::{handle, WebhookOutcome};
use sf_intake::{Challenge, ConnectorError, Headers, IntakeEvent, Pipeline, Verify, Webhook};

const NOW_MS: u64 = 1_700_000_000_000;
const SECRET: &str = "8f742231b10e8888abcd99yyyzzz85a5";
const GH_SECRET: &str = "It's a Secret to Everybody";

fn preset_text(file: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/presets")
            .join(file),
    )
    .unwrap()
}

/// A webhook preset with the fixture's ids swapped in (`subs`: from -> to on the JSON text).
fn hook(file: &str, source: &str, secret: &str, subs: &[(&str, &str)]) -> Webhook {
    let mut t = preset_text(file);
    for (a, b) in subs {
        assert!(t.contains(a), "{file} has no {a}");
        t = t.replace(a, b);
    }
    let v: Value = serde_json::from_str(&t).unwrap();
    let verify: Verify = serde_json::from_value(v["verify"].clone()).unwrap();
    verify.validate().unwrap();
    let challenge: Option<Challenge> = v
        .get("challenge")
        .map(|c| serde_json::from_value(c.clone()).unwrap());
    if let Some(c) = &challenge {
        c.validate().unwrap();
    }
    let pipeline: Pipeline = serde_json::from_value(v["events"].clone()).unwrap();
    pipeline.validate().unwrap();
    Webhook {
        source: source.into(),
        secret: secret.into(),
        verify,
        challenge,
        pipeline,
    }
}

fn chat() -> Webhook {
    hook(
        "slack-events.json",
        "slack",
        SECRET,
        &[
            ("\"C0FACTORY\": \"demo\"", "\"C0DEFAULT1\": \"triage\""),
            ("U0FACTORY", "U0LAN0Z89"),
        ],
    )
}

fn gh() -> Webhook {
    hook("github-issues-webhook.json", "github", GH_SECRET, &[])
}

fn chat_headers(body: &str, ts: &str, secret: &str) -> Headers {
    let sig = format!(
        "v0={}",
        hmac_sha256_hex(secret.as_bytes(), format!("v0:{ts}:{body}").as_bytes())
    );
    Headers::from_pairs(&[
        ("X-Slack-Request-Timestamp", ts),
        ("X-Slack-Signature", &sig),
    ])
}

fn chat_ok(name: &str) -> Result<WebhookOutcome, ConnectorError> {
    let body = fixture(name);
    handle(
        &chat(),
        &chat_headers(&body, "1700000000", SECRET),
        body.as_bytes(),
        NOW_MS,
    )
}

fn only_event(o: WebhookOutcome) -> IntakeEvent {
    match o {
        WebhookOutcome::Events(m) => {
            assert_eq!(m.events.len(), 1, "{m:?}");
            m.events.into_iter().next().unwrap()
        }
        o => panic!("{o:?}"),
    }
}

fn chat_event(name: &str) -> IntakeEvent {
    only_event(chat_ok(name).unwrap())
}

#[test]
fn presets_parse_and_validate() {
    for f in [
        "slack-events.json",
        "github-issues-webhook.json",
        "github-issues-poll.json",
        "linear-mcp.json",
    ] {
        let v: Value = serde_json::from_str(&preset_text(f)).unwrap();
        let p: Pipeline =
            serde_json::from_value(v["events"].clone()).unwrap_or_else(|e| panic!("{f}: {e}"));
        p.validate().unwrap_or_else(|e| panic!("{f}: {e}"));
    }
    chat();
    gh();
}

#[test]
fn chat_url_verification_returns_challenge() {
    let WebhookOutcome::Challenge(c) = chat_ok("slack_url_verification.json").unwrap() else {
        panic!()
    };
    assert!(c.starts_with("3eZbrw1a"));
}

#[test]
fn chat_app_mention_parses_command() {
    let e = chat_event("slack_app_mention.json");
    assert_eq!(e.requested_workflow.as_deref(), Some("fix-bug"));
    assert_eq!(e.text, "login page 500s after the password reset flow");
    assert_eq!(
        (e.source.as_str(), e.actor.as_str(), e.channel.as_str()),
        ("slack", "U061F7AUR", "C024BE91L")
    );
    assert_eq!(e.dedupe_key, "slack:Ev0PV52K21");
    assert_eq!(e.source_ref, "C024BE91L/1700000000.000100");
    assert_eq!(e.received_at, NOW_MS);
}

#[test]
fn chat_channel_default_workflow() {
    let e = chat_event("slack_message.json");
    assert_eq!(e.requested_workflow.as_deref(), Some("triage"));
    assert_eq!(e.text, "the nightly build is red again");
}

#[test]
fn chat_retry_has_same_dedupe_key() {
    let body = fixture("slack_app_mention.json");
    let sig = chat_headers(&body, "1700000010", SECRET);
    let h = Headers::from_pairs(&[
        ("x-slack-request-timestamp", "1700000010"),
        ("x-slack-signature", sig.get("X-Slack-Signature").unwrap()),
        ("X-Slack-Retry-Num", "1"),
        ("X-Slack-Retry-Reason", "http_timeout"),
    ]);
    let retry = only_event(handle(&chat(), &h, body.as_bytes(), NOW_MS + 11_000).unwrap());
    assert_eq!(
        retry.dedupe_key,
        chat_event("slack_app_mention.json").dedupe_key
    );
}

#[test]
fn chat_ignores_bots_edits_other_types_and_double_mentions() {
    for f in [
        "slack_bot_message.json",
        "slack_message_changed.json",
        "slack_message_with_mention.json",
    ] {
        let e = chat_ok(f).unwrap_err();
        assert!(matches!(e, ConnectorError::Unsupported(_)), "{f}: {e:?}");
        assert_eq!(e.http_status(), 200);
    }
    let body = r#"{"type":"event_callback","event_id":"E1","event":{"type":"reaction_added"}}"#;
    let r = handle(
        &chat(),
        &chat_headers(body, "1700000000", SECRET),
        body.as_bytes(),
        NOW_MS,
    );
    assert!(matches!(r, Err(ConnectorError::Unsupported(_))));
    // The app's own user never triggers itself.
    let own = r#"{"type":"event_callback","event_id":"E2","event":{"type":"message","user":"U0LAN0Z89","text":"hi","channel":"C0DEFAULT1","ts":"1.1"}}"#;
    let r = handle(
        &chat(),
        &chat_headers(own, "1700000000", SECRET),
        own.as_bytes(),
        NOW_MS,
    );
    assert!(matches!(r, Err(ConnectorError::Unsupported(_))));
}

#[test]
fn chat_signature_edge_cases() {
    let body = fixture("slack_app_mention.json");
    let w = chat();
    let good = chat_headers(&body, "1700000000", SECRET);
    // Wrong secret.
    let bad = chat_headers(&body, "1700000000", "other-secret");
    assert_eq!(
        handle(&w, &bad, body.as_bytes(), NOW_MS),
        Err(ConnectorError::BadSignature)
    );
    // Tampered body.
    let tampered = body.replace("login", "logout");
    assert_eq!(
        handle(&w, &good, tampered.as_bytes(), NOW_MS),
        Err(ConnectorError::BadSignature)
    );
    // Missing headers.
    let no_sig = Headers::from_pairs(&[("X-Slack-Request-Timestamp", "1700000000")]);
    assert!(
        matches!(handle(&w, &no_sig, body.as_bytes(), NOW_MS), Err(ConnectorError::Malformed(m)) if m.contains("X-Slack-Signature"))
    );
    let no_ts =
        Headers::from_pairs(&[("X-Slack-Signature", good.get("x-slack-signature").unwrap())]);
    assert!(
        matches!(handle(&w, &no_ts, body.as_bytes(), NOW_MS), Err(ConnectorError::Malformed(m)) if m.contains("X-Slack-Request-Timestamp"))
    );
    // Wrong scheme prefix.
    let v1 = Headers::from_pairs(&[
        ("X-Slack-Request-Timestamp", "1700000000"),
        ("X-Slack-Signature", "v1=abc"),
    ]);
    assert_eq!(
        handle(&w, &v1, body.as_bytes(), NOW_MS),
        Err(ConnectorError::BadSignature)
    );
    // Replay window: 301 s old and 301 s in the future are refused, 300 s is fine.
    for (ts, ok) in [
        (1_699_999_699u64, false),
        (1_700_000_301, false),
        (1_699_999_700, true),
        (1_700_000_300, true),
    ] {
        let h = chat_headers(&body, &ts.to_string(), SECRET);
        let r = handle(&w, &h, body.as_bytes(), NOW_MS);
        assert_eq!(r.is_ok(), ok, "ts {ts}: {r:?}");
        if !ok {
            assert!(matches!(r, Err(ConnectorError::StaleTimestamp { .. })));
        }
    }
    // Bad timestamp header and malformed (but correctly signed) body.
    let h = chat_headers(&body, "soon", SECRET);
    assert!(matches!(
        handle(&w, &h, body.as_bytes(), NOW_MS),
        Err(ConnectorError::Malformed(_))
    ));
    let junk = "not json";
    let r = handle(
        &w,
        &chat_headers(junk, "1700000000", SECRET),
        junk.as_bytes(),
        NOW_MS,
    );
    assert!(matches!(r, Err(ConnectorError::Malformed(_))));
}

fn gh_headers(body: &str, event: &str, delivery: &str) -> Headers {
    let sig = format!(
        "sha256={}",
        hmac_sha256_hex(GH_SECRET.as_bytes(), body.as_bytes())
    );
    Headers::from_pairs(&[
        ("X-Hub-Signature-256", &sig),
        ("X-GitHub-Event", event),
        ("X-GitHub-Delivery", delivery),
    ])
}

fn gh_post(name: &str, delivery: &str) -> Result<WebhookOutcome, ConnectorError> {
    let body = fixture(name);
    handle(
        &gh(),
        &gh_headers(&body, "issues", delivery),
        body.as_bytes(),
        NOW_MS,
    )
}

fn gh_event(name: &str, delivery: &str) -> IntakeEvent {
    only_event(gh_post(name, delivery).unwrap())
}

#[test]
fn issue_hook_opened_maps_fields() {
    let e = gh_event("github_issue_opened.json", "d-1");
    assert_eq!(e.requested_workflow.as_deref(), Some("fix-bug"));
    assert_eq!(e.dedupe_key, "github:d-1");
    assert_eq!(
        (e.source_ref.as_str(), e.channel.as_str(), e.actor.as_str()),
        ("acme/shop#42", "acme/shop", "octocat")
    );
    assert_eq!(e.labels, ["bug", "factory:fix-bug", "p0"]);
    assert_eq!(e.title, "Checkout total wrong with coupon");
    assert_eq!(
        e.text,
        "Applying a 10% coupon doubles the shipping.\nhttps://github.com/acme/shop/issues/42"
    );
}

#[test]
fn issue_hook_labeled_uses_added_label_and_redelivery_dedupes() {
    let a = gh_event("github_issue_labeled.json", "d-1");
    let b = gh_event("github_issue_labeled.json", "d-1");
    assert_eq!(a.dedupe_key, b.dedupe_key);
    assert_ne!(
        a.dedupe_key,
        gh_event("github_issue_labeled.json", "d-2").dedupe_key,
        "a new delivery is a new event"
    );
    assert_eq!(a.requested_workflow.as_deref(), Some("docs-fix"));
    assert_eq!(a.actor, "maintainer");
    assert_eq!(a.text, "https://github.com/acme/shop/issues/43");
    // Another label added to an issue that carries a factory label: the preset's second rule still finds it.
    assert_eq!(
        gh_event("github_issue_labeled_other.json", "d")
            .requested_workflow
            .as_deref(),
        Some("docs-fix")
    );
}

#[test]
fn issue_hook_reopen_keys_by_delivery() {
    let a = gh_event("github_issue_reopened.json", "d-1");
    assert_eq!(a.dedupe_key, "github:d-1");
    assert_eq!(a.source_ref, "acme/shop#42");
    assert_ne!(
        a.dedupe_key,
        gh_event("github_issue_reopened.json", "d-2").dedupe_key
    );
}

#[test]
fn issue_hook_ignores_other_events_and_actions() {
    let body = fixture("github_issue_closed.json");
    let w = gh();
    assert!(matches!(
        handle(
            &w,
            &gh_headers(&body, "issues", "d"),
            body.as_bytes(),
            NOW_MS
        ),
        Err(ConnectorError::Unsupported(_))
    ));
    assert!(matches!(
        handle(&w, &gh_headers("{}", "ping", "d"), b"{}", NOW_MS),
        Err(ConnectorError::Unsupported(_))
    ));
    // An issue event without any factory label maps to no workflow: dropped, not an error.
    let plain = r#"{"action":"opened","issue":{"number":1,"title":"t","labels":[{"name":"bug"}]},"repository":{"full_name":"a/b"},"sender":{"login":"u"}}"#;
    assert!(matches!(
        handle(
            &w,
            &gh_headers(plain, "issues", "d"),
            plain.as_bytes(),
            NOW_MS
        ),
        Err(ConnectorError::Unsupported(_))
    ));
}

#[test]
fn issue_hook_signature_edge_cases() {
    let body = fixture("github_issue_opened.json");
    let w = gh();
    let good = gh_headers(&body, "issues", "d");
    let tampered = body.replace("octocat", "mallory");
    assert_eq!(
        handle(&w, &good, tampered.as_bytes(), NOW_MS),
        Err(ConnectorError::BadSignature)
    );
    let wrong = hook("github-issues-webhook.json", "github", "nope", &[]);
    assert_eq!(
        handle(&wrong, &good, body.as_bytes(), NOW_MS),
        Err(ConnectorError::BadSignature)
    );
    let none = Headers::from_pairs(&[("X-GitHub-Event", "issues")]);
    assert!(
        matches!(handle(&w, &none, body.as_bytes(), NOW_MS), Err(ConnectorError::Malformed(m)) if m.contains("X-Hub-Signature-256"))
    );
    let bare = Headers::from_pairs(&[
        (
            "X-Hub-Signature-256",
            &hmac_sha256_hex(GH_SECRET.as_bytes(), body.as_bytes()),
        ),
        ("X-GitHub-Event", "issues"),
    ]);
    assert_eq!(
        handle(&w, &bare, body.as_bytes(), NOW_MS),
        Err(ConnectorError::BadSignature)
    );
    let junk = "{";
    assert!(matches!(
        handle(
            &w,
            &gh_headers(junk, "issues", "d"),
            junk.as_bytes(),
            NOW_MS
        ),
        Err(ConnectorError::Malformed(_))
    ));
}

#[test]
fn issue_poll_preset_maps_the_list_and_skips_pull_requests() {
    let v: Value = serde_json::from_str(&preset_text("github-issues-poll.json")).unwrap();
    let p: Pipeline = serde_json::from_value(v["events"].clone()).unwrap();
    p.validate().unwrap();
    assert_eq!(v["kind"], "command");
    let list: Value = serde_json::from_str(&fixture("github_issues_list.json")).unwrap();
    let m = p.events("gh", &list, None, NOW_MS).unwrap();
    // [0] is mapped; [1] is a pull request (ignore); [2] has no factory label (no workflow).
    assert_eq!((m.events.len(), m.ignored, m.skipped), (1, 2, 0), "{m:?}");
    let e = &m.events[0];
    assert_eq!(
        (e.requested_workflow.as_deref(), e.actor.as_str()),
        (Some("fix-bug"), "octocat")
    );
    assert_eq!(
        (
            e.source_ref.as_str(),
            e.channel.as_str(),
            e.dedupe_key.as_str()
        ),
        (
            "kedarisetti/software-factory#42",
            "kedarisetti/software-factory",
            "gh:42"
        )
    );
}

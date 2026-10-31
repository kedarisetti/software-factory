mod common;
use common::{ev, fixture, table};
use sf_intake::{ConfigError, Permission, PermissionTable};

fn allowed(t: &PermissionTable, src: &str, ch: &str, actor: &str, wf: &str) -> bool {
    t.decide(&ev(src, ch, actor, wf, "x", "k", 1), wf) == Permission::Allowed
}

#[test]
fn rules_match_source_channel_actor_workflow() {
    let t = table();
    assert!(allowed(&t, "slack", "C024BE91L", "anyone", "fix-bug"));
    assert!(!allowed(&t, "slack", "C024BE91L", "anyone", "docs-fix"));
    assert!(!allowed(&t, "slack", "COTHER", "anyone", "fix-bug"));
    // Wildcard channel with an actor allowlist.
    assert!(allowed(&t, "slack", "COTHER", "U061F7AUR", "triage"));
    assert!(!allowed(&t, "slack", "COTHER", "U999", "triage"));
}

#[test]
fn default_deny_when_no_rule_matches() {
    let t = table();
    let Permission::Rejected(why) = t.decide(&ev("email", "inbox", "a", "fix-bug", "x", "k", 1), "fix-bug") else { panic!() };
    assert!(why.contains("no rule"));
    assert!(!allowed(&PermissionTable::from_json(r#"{"rules":[]}"#).unwrap(), "github", "acme/shop", "a", "fix-bug"));
}

#[test]
fn deploy_allowed_for_tickets_never_for_chat() {
    let t = table();
    assert!(allowed(&t, "github", "acme/shop", "x", "deploy-staging"));
    assert!(allowed(&t, "github", "acme/ops", "release-bot", "deploy-prod"));
    assert!(!allowed(&t, "github", "acme/ops", "someone-else", "deploy-prod"));
    for wf in ["deploy-prod", "deploy-staging"] {
        let Permission::Rejected(why) = t.decide(&ev("slack", "C024BE91L", "U061F7AUR", wf, "x", "k", 1), wf) else { panic!() };
        assert!(why.contains("cannot start deploy"), "{why}");
    }
}

#[test]
fn chat_rule_allowing_deploy_fails_at_load() {
    let bad = r#"{"deploy_workflows":["deploy-prod"],"rules":[{"source":"slack","channel":"*","workflows":["triage","deploy-prod"]}]}"#;
    let chat = ["slack".to_string()];
    assert_eq!(
        PermissionTable::from_json_with_chat(bad, &chat),
        Err(ConfigError::ChatDeploy { chat_source: "slack".into(), workflow: "deploy-prod".into() })
    );
    // Source name case does not bypass the check.
    assert!(PermissionTable::from_json_with_chat(&bad.replace("\"slack\"", "\"Slack\""), &chat).is_err());
    assert!(PermissionTable::from_json_with_chat(&fixture("permissions.json"), &chat).is_ok());
    // No name is chat unless the config says so.
    assert!(PermissionTable::from_json(bad).is_ok());
}

#[test]
fn only_configured_sources_are_chat() {
    let t = PermissionTable::from_json(&fixture("permissions.json")).unwrap();
    assert!(t.chat_sources.is_empty());
    assert!(allowed(&t, "slack", "C024BE91L", "anyone", "fix-bug"));
}

#[test]
fn invalid_tables_are_refused() {
    for bad in [
        "not json",
        r#"{"rules":[{"source":"*","channel":"*","workflows":["a"]}]}"#,
        r#"{"rules":[{"source":"slack","channel":"*","workflows":["*"]}]}"#,
        r#"{"rules":[{"source":"slack","channel":"*","workflows":[],"surprise":1}]}"#,
    ] {
        assert!(PermissionTable::from_json(bad).is_err(), "{bad}");
    }
}

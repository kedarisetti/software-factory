//! Trigger permission table (TRG-3): which source may start which workflow. Default deny.

use serde::Deserialize;

use crate::event::IntakeEvent;


#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub source: String,
    /// Exact channel (as the source's `channel` template yields it) or `*`.
    pub channel: String,
    /// Allowlist of actors; absent = anyone in the channel.
    #[serde(default)]
    pub actors: Option<Vec<String>>,
    /// Exact workflow ids; no wildcards.
    pub workflows: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionTable {
    pub rules: Vec<Rule>,
    /// Workflows that can deploy. No chat source may allow one.
    #[serde(default)]
    pub deploy_workflows: Vec<String>,
    /// Source names that are chat (`chat: true`). Not part of the JSON; set by the loader from the source config.
    #[serde(skip)]
    pub chat_sources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    Allowed,
    Rejected(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("permission table: {0}")]
    Parse(String),
    #[error("chat source {chat_source} may not allow deploy workflow {workflow}")]
    ChatDeploy { chat_source: String, workflow: String },
    #[error("permission table: {0}")]
    Invalid(String),
}

impl PermissionTable {
    /// Parses and validates. Fails if any chat rule allows a deploy workflow, so TRG-3's
    /// "no chat message can start a deploy" holds by construction.
    pub fn from_json(json: &str) -> Result<Self, ConfigError> {
        let t: PermissionTable = serde_json::from_str(json).map_err(|e| ConfigError::Parse(e.to_string()))?;
        t.validate()?;
        Ok(t)
    }

    /// Like `from_json` but chat-ness comes from the source config: exactly these names are chat.
    pub fn from_json_with_chat(json: &str, chat_sources: &[String]) -> Result<Self, ConfigError> {
        let mut t: PermissionTable = serde_json::from_str(json).map_err(|e| ConfigError::Parse(e.to_string()))?;
        t.chat_sources = chat_sources.to_vec();
        t.validate()?;
        Ok(t)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for r in &self.rules {
            if r.source == "*" || r.source.is_empty() {
                return Err(ConfigError::Invalid("rule source must name a connector".into()));
            }
            if r.workflows.iter().any(|w| w == "*") {
                return Err(ConfigError::Invalid("workflow wildcards are not allowed".into()));
            }
            if let Some(w) = r.workflows.iter().find(|w| self.is_deploy(w)) {
                if self.is_chat(&r.source) {
                    return Err(ConfigError::ChatDeploy { chat_source: r.source.clone(), workflow: w.clone() });
                }
            }
        }
        Ok(())
    }

    fn is_deploy(&self, workflow: &str) -> bool {
        self.deploy_workflows.iter().any(|d| d == workflow)
    }

    /// Allowed only if some rule matches source, channel, actor and workflow.
    pub fn decide(&self, ev: &IntakeEvent, workflow: &str) -> Permission {
        if self.is_chat(&ev.source) && self.is_deploy(workflow) {
            return Permission::Rejected(format!("chat source {} cannot start deploy workflow {workflow}", ev.source));
        }
        let hit = self.rules.iter().any(|r| {
            r.source.eq_ignore_ascii_case(&ev.source)
                && (r.channel == "*" || r.channel == ev.channel)
                && r.actors.as_ref().is_none_or(|a| a.contains(&ev.actor))
                && r.workflows.iter().any(|w| w == workflow)
        });
        if hit {
            Permission::Allowed
        } else {
            Permission::Rejected(format!("no rule lets {} {} in {} start {workflow}", ev.source, ev.actor, ev.channel))
        }
    }
}

impl PermissionTable {
    fn is_chat(&self, source: &str) -> bool {
        self.chat_sources.iter().any(|c| c.eq_ignore_ascii_case(source))
    }
}

//! Agent-callable Telegram forum-topic tool.
//!
//! Lets the LLM create and manage forum topics in a single, operator-fixed
//! master group. Every topic is an isolated conversation session, so the agent
//! can spin up and steer parallel work threads on its own.
//!
//! Security: the target group is resolved **only** from
//! `[channels.telegram.<alias>.topics].group_id` — the model never supplies a
//! `chat_id`. The tool refuses to act unless the feature is `enabled`,
//! `agent_managed` is true, and a `group_id` is configured. The actual Telegram
//! work runs against the live channel via
//! [`crate::topics::perform_topic_op`].

use crate::topics::{perform_topic_op, TopicOp};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::schema::Config;

/// Agent-callable forum-topic management tool bound to one Telegram alias.
pub struct TelegramTopicTool {
    config: Arc<Config>,
    alias: String,
}

impl TelegramTopicTool {
    pub const NAME: &'static str = "telegram_topic";

    pub fn new(config: Arc<Config>, alias: impl Into<String>) -> Self {
        Self {
            config,
            alias: alias.into(),
        }
    }
}

fn reject(msg: impl Into<String>) -> ToolResult {
    ToolResult {
        success: false,
        output: ToolOutput::default(),
        error: Some(msg.into()),
    }
}

fn str_arg(args: &serde_json::Value, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

#[async_trait]
impl Tool for TelegramTopicTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Create and manage Telegram forum topics in this bot's configured master \
         group. Each topic is its own isolated conversation session, letting you \
         open and steer parallel work threads. Actions: `create` (open a topic, \
         optionally seeding it with a `brief`), `list` (show tracked topics and \
         status), `rename`, `close`, `reopen`. Topics are addressed by name. The \
         group is fixed by configuration — you cannot target an arbitrary chat."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "rename", "close", "reopen"],
                    "description": "The topic operation to perform."
                },
                "name": {
                    "type": "string",
                    "description": "Topic name. Required for create/rename/close/reopen; for rename it is the current name."
                },
                "brief": {
                    "type": "string",
                    "description": "Optional. For `create`, records the topic's purpose/goal (stored with the topic). Note: the tool does not auto-start a turn — send a message into the new topic to begin work there."
                },
                "new_name": {
                    "type": "string",
                    "description": "For `rename`: the new topic name."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        // Resolve gating + the master group from config only. Clone the group id
        // to an owned String so no borrow of `self.config` is held across the
        // later await.
        let (enabled, agent_managed, group_id) =
            match self.config.channels.telegram.get(&self.alias) {
                Some(tg) => (
                    tg.topics.enabled,
                    tg.topics.agent_managed,
                    tg.topics.group_id.clone(),
                ),
                None => {
                    return Ok(reject(format!(
                        "Telegram channel '{}' is not configured.",
                        self.alias
                    )));
                }
            };

        if !enabled {
            return Ok(reject(
                "Forum topics are not enabled for this Telegram channel \
                 (set [channels.telegram.<alias>.topics].enabled = true).",
            ));
        }
        if !agent_managed {
            return Ok(reject(
                "Agent-managed topic operations are disabled \
                 (set [channels.telegram.<alias>.topics].agent_managed = true).",
            ));
        }
        let chat_id = match group_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(reject(
                    "No master forum group is configured \
                     (set [channels.telegram.<alias>.topics].group_id = \"<chat_id>\").",
                ));
            }
        };

        let action = str_arg(&args, "action");
        let op = match action.as_str() {
            "create" => {
                let name = str_arg(&args, "name");
                if name.is_empty() {
                    return Ok(reject("`create` requires a non-empty 'name'."));
                }
                let brief_raw = str_arg(&args, "brief");
                let brief = if brief_raw.is_empty() {
                    None
                } else {
                    Some(brief_raw)
                };
                TopicOp::Create { name, brief }
            }
            "list" => TopicOp::List,
            "rename" => {
                let name = str_arg(&args, "name");
                let new_name = str_arg(&args, "new_name");
                if name.is_empty() || new_name.is_empty() {
                    return Ok(reject("`rename` requires both 'name' and 'new_name'."));
                }
                TopicOp::Rename { name, new_name }
            }
            "close" => {
                let name = str_arg(&args, "name");
                if name.is_empty() {
                    return Ok(reject("`close` requires a non-empty 'name'."));
                }
                TopicOp::Close { name }
            }
            "reopen" => {
                let name = str_arg(&args, "name");
                if name.is_empty() {
                    return Ok(reject("`reopen` requires a non-empty 'name'."));
                }
                TopicOp::Reopen { name }
            }
            other => {
                return Ok(reject(format!(
                    "Unknown action {other:?}; use one of create|list|rename|close|reopen."
                )));
            }
        };

        match perform_topic_op(&self.config, &self.alias, &chat_id, op).await {
            Ok(outcome) => Ok(ToolResult {
                success: true,
                output: outcome.message.into(),
                error: None,
            }),
            Err(e) => Ok(reject(format!("topic operation failed: {e:#}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::schema::{TelegramConfig, TopicsConfig};

    fn config_with_topics(topics: TopicsConfig) -> Arc<Config> {
        let mut config = Config::default();
        let tg = TelegramConfig {
            bot_token: "tok".into(),
            topics,
            ..Default::default()
        };
        config.channels.telegram.insert("work".into(), tg);
        Arc::new(config)
    }

    fn tool(topics: TopicsConfig) -> TelegramTopicTool {
        TelegramTopicTool::new(config_with_topics(topics), "work")
    }

    #[tokio::test]
    async fn rejects_when_topics_disabled() {
        let t = tool(TopicsConfig {
            enabled: false,
            group_id: Some("-100200300".into()),
            ..Default::default()
        });
        let res = t.execute(json!({ "action": "list" })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("not enabled"));
    }

    #[tokio::test]
    async fn rejects_when_agent_management_disabled() {
        let t = tool(TopicsConfig {
            enabled: true,
            agent_managed: false,
            group_id: Some("-100200300".into()),
            ..Default::default()
        });
        let res = t.execute(json!({ "action": "list" })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("Agent-managed"));
    }

    #[tokio::test]
    async fn rejects_when_group_id_unset() {
        let t = tool(TopicsConfig {
            enabled: true,
            agent_managed: true,
            group_id: None,
            ..Default::default()
        });
        let res = t.execute(json!({ "action": "list" })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("master forum group"));
    }

    #[tokio::test]
    async fn create_uses_config_group_id_not_args() {
        crate::topics::register_echo_topic_op_fn();
        let t = tool(TopicsConfig {
            enabled: true,
            agent_managed: true,
            group_id: Some("-100200300".into()),
            ..Default::default()
        });
        // The LLM tries to sneak in a different chat via args — it must be ignored.
        let res = t
            .execute(json!({
                "action": "create",
                "name": "Design",
                "brief": "draft the RFC",
                "chat_id": "-999",
                "group_id": "-999"
            }))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
        let out = res.output.as_str();
        // Echo handler encodes: create|<alias>|<chat_id>|<name>|<brief?>
        assert!(
            out.contains("create|work|-100200300|Design|"),
            "chat_id must come from config group_id, not args: {out}"
        );
    }

    #[tokio::test]
    async fn list_builds_list_op() {
        crate::topics::register_echo_topic_op_fn();
        let t = tool(TopicsConfig {
            enabled: true,
            agent_managed: true,
            group_id: Some("-100200300".into()),
            ..Default::default()
        });
        let res = t.execute(json!({ "action": "list" })).await.unwrap();
        assert!(res.success, "{:?}", res.error);
        let out = res.output.as_str();
        assert_eq!(out, "list|work|-100200300");
    }

    #[tokio::test]
    async fn rename_requires_new_name() {
        crate::topics::register_echo_topic_op_fn();
        let t = tool(TopicsConfig {
            enabled: true,
            agent_managed: true,
            group_id: Some("-100200300".into()),
            ..Default::default()
        });
        let res = t
            .execute(json!({ "action": "rename", "name": "Design" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("new_name"));
    }

    #[tokio::test]
    async fn unknown_action_rejected() {
        crate::topics::register_echo_topic_op_fn();
        let t = tool(TopicsConfig {
            enabled: true,
            agent_managed: true,
            group_id: Some("-100200300".into()),
            ..Default::default()
        });
        let res = t.execute(json!({ "action": "destroy" })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("Unknown action"));
    }
}

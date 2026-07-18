//! Agent-callable Telegram multiple-choice tool.
//!
//! Renders a prompt with 2-10 options as native Telegram inline buttons in
//! this bot's configured master group, waits (bounded) for the user's tap,
//! and returns the chosen option to the calling agent. This lets the agent
//! ask a structured multiple-choice question instead of parsing a free-text
//! reply.
//!
//! Security: the target group is resolved **only** from
//! `[channels.telegram.<alias>.topics].group_id` — the model never supplies a
//! `chat_id`. The tool refuses to act unless the feature is `enabled` and a
//! `group_id` is configured. The actual Telegram work runs against the live
//! channel via [`crate::choices::perform_choice_op`].

use crate::choices::{ChoiceOp, perform_choice_op};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::schema::Config;

const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 10;

/// Agent-callable multiple-choice tool bound to one Telegram alias.
pub struct TelegramMcChoiceTool {
    config: Arc<Config>,
    alias: String,
}

impl TelegramMcChoiceTool {
    pub const NAME: &'static str = "telegram_mc_choice";

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

#[async_trait]
impl Tool for TelegramMcChoiceTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Ask the user a multiple-choice question in this bot's configured master \
         group, rendered as native Telegram inline buttons. Waits (bounded) for \
         the user's tap and returns the chosen option. Provide 2 to 10 options. \
         The group is fixed by configuration — you cannot target an arbitrary chat."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The question or prompt to show above the buttons."
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": MIN_OPTIONS,
                    "maxItems": MAX_OPTIONS,
                    "description": "2 to 10 answer options, each rendered as its own inline button."
                }
            },
            "required": ["prompt", "options"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        // Resolve gating + the target group from config only. Clone the group id
        // to an owned String so no borrow of `self.config` is held across the
        // later await.
        let (enabled, group_id) = match self.config.channels.telegram.get(&self.alias) {
            Some(tg) => (tg.topics.enabled, tg.topics.group_id.clone()),
            None => {
                return Ok(reject(format!(
                    "Telegram channel '{}' is not configured.",
                    self.alias
                )));
            }
        };

        if !enabled {
            return Ok(reject(
                "Multiple-choice prompts are not enabled for this Telegram channel \
                 (set [channels.telegram.<alias>.topics].enabled = true).",
            ));
        }
        let chat_id = match group_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(reject(
                    "No target group is configured \
                     (set [channels.telegram.<alias>.topics].group_id = \"<chat_id>\").",
                ));
            }
        };

        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        if prompt.is_empty() {
            return Ok(reject("`prompt` must be a non-empty string."));
        }

        let options: Vec<String> = args
            .get("options")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if options.len() < MIN_OPTIONS || options.len() > MAX_OPTIONS {
            return Ok(reject(format!(
                "`options` must have between {MIN_OPTIONS} and {MAX_OPTIONS} non-empty entries \
                 (got {}).",
                options.len()
            )));
        }

        match perform_choice_op(
            &self.config,
            &self.alias,
            &chat_id,
            ChoiceOp::RenderChoice { prompt, options },
        )
        .await
        {
            Ok(outcome) => Ok(ToolResult {
                success: true,
                output: outcome.message.into(),
                error: None,
            }),
            Err(e) => Ok(reject(format!("multiple-choice prompt failed: {e:#}"))),
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

    fn tool(topics: TopicsConfig) -> TelegramMcChoiceTool {
        TelegramMcChoiceTool::new(config_with_topics(topics), "work")
    }

    #[tokio::test]
    async fn rejects_when_disabled() {
        let t = tool(TopicsConfig {
            enabled: false,
            group_id: Some("-100200300".into()),
            ..Default::default()
        });
        let res = t
            .execute(json!({ "prompt": "Pick one", "options": ["A", "B"] }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("not enabled"));
    }

    #[tokio::test]
    async fn rejects_when_group_id_unset() {
        let t = tool(TopicsConfig {
            enabled: true,
            group_id: None,
            ..Default::default()
        });
        let res = t
            .execute(json!({ "prompt": "Pick one", "options": ["A", "B"] }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("target group"));
    }

    #[tokio::test]
    async fn rejects_when_channel_not_configured() {
        let t = TelegramMcChoiceTool::new(Arc::new(Config::default()), "missing");
        let res = t
            .execute(json!({ "prompt": "Pick one", "options": ["A", "B"] }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("not configured"));
    }

    #[tokio::test]
    async fn rejects_empty_prompt() {
        crate::choices::register_echo_choice_op_fn();
        let t = tool(TopicsConfig {
            enabled: true,
            group_id: Some("-100200300".into()),
            ..Default::default()
        });
        let res = t
            .execute(json!({ "prompt": "  ", "options": ["A", "B"] }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("prompt"));
    }

    #[tokio::test]
    async fn rejects_too_few_options() {
        crate::choices::register_echo_choice_op_fn();
        let t = tool(TopicsConfig {
            enabled: true,
            group_id: Some("-100200300".into()),
            ..Default::default()
        });
        let res = t
            .execute(json!({ "prompt": "Pick one", "options": ["A"] }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("options"));
    }

    #[tokio::test]
    async fn rejects_too_many_options() {
        crate::choices::register_echo_choice_op_fn();
        let t = tool(TopicsConfig {
            enabled: true,
            group_id: Some("-100200300".into()),
            ..Default::default()
        });
        let opts: Vec<String> = (0..11).map(|i| format!("opt{i}")).collect();
        let res = t
            .execute(json!({ "prompt": "Pick one", "options": opts }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("options"));
    }

    #[tokio::test]
    async fn uses_config_group_id_not_args() {
        crate::choices::register_echo_choice_op_fn();
        let t = tool(TopicsConfig {
            enabled: true,
            group_id: Some("-100200300".into()),
            ..Default::default()
        });
        // The LLM tries to sneak in a different chat via args — it must be ignored.
        let res = t
            .execute(json!({
                "prompt": "Pick one",
                "options": ["A", "B"],
                "chat_id": "-999",
                "group_id": "-999"
            }))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
        let out = res.output.as_str();
        assert!(
            out.contains("choice|work|-100200300|Pick one|A,B"),
            "chat_id must come from config group_id, not args: {out}"
        );
    }
}

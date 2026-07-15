//! Telegram forum-topics support: name<->thread registry, service-event
//! parsing, command parsing, and pure request-body/response helpers.
//!
//! All logic here is pure (JSON in / value out) so it is exercised by plain
//! unit tests. The async HTTP glue lives in [`crate::telegram`] and calls into
//! these builders/parsers. `message_thread_id` is encoded as a JSON **string**
//! everywhere in this codebase, so every outbound body built here uses
//! `Value::String(id.to_string())`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// File name (under the channel state/workspace dir) for the persisted registry.
pub const TELEGRAM_TOPICS_STATE_FILE: &str = "telegram-topics.json";

/// Open/closed state of a forum topic, mirrored from Telegram service events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopicStatus {
    #[default]
    Open,
    Closed,
}

/// A single forum topic's persisted metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicEntry {
    pub name: String,
    #[serde(default)]
    pub status: TopicStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// name<->thread registry: `chat_id -> thread_id -> entry`.
///
/// Telegram omits the topic name on ordinary messages and has no list-all
/// endpoint, so the registry is the local source of truth, updated from
/// commands and from observed `forum_topic_*` service events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicRegistry {
    #[serde(default)]
    chats: BTreeMap<String, BTreeMap<String, TopicEntry>>,
}

impl TopicRegistry {
    /// Insert or update a topic on create. Preserves `created_at` when the
    /// topic already exists; always bumps `updated_at`.
    pub fn upsert_created(
        &mut self,
        chat_id: &str,
        thread_id: &str,
        name: &str,
        purpose: Option<String>,
        now: u64,
    ) {
        let chat = self.chats.entry(chat_id.to_string()).or_default();
        chat.entry(thread_id.to_string())
            .and_modify(|e| {
                e.name = name.to_string();
                if purpose.is_some() {
                    e.purpose = purpose.clone();
                }
                e.status = TopicStatus::Open;
                e.updated_at = now;
            })
            .or_insert_with(|| TopicEntry {
                name: name.to_string(),
                status: TopicStatus::Open,
                purpose,
                created_at: now,
                updated_at: now,
            });
    }

    /// Rename an existing topic (or create a placeholder if unknown).
    pub fn rename(&mut self, chat_id: &str, thread_id: &str, new_name: &str, now: u64) {
        let entry = self.entry_or_placeholder(chat_id, thread_id, now);
        entry.name = new_name.to_string();
        entry.updated_at = now;
    }

    /// Set a topic's open/closed status (or create a placeholder if unknown).
    pub fn set_status(&mut self, chat_id: &str, thread_id: &str, status: TopicStatus, now: u64) {
        let entry = self.entry_or_placeholder(chat_id, thread_id, now);
        entry.status = status;
        entry.updated_at = now;
    }

    fn entry_or_placeholder(&mut self, chat_id: &str, thread_id: &str, now: u64) -> &mut TopicEntry {
        self.chats
            .entry(chat_id.to_string())
            .or_default()
            .entry(thread_id.to_string())
            .or_insert_with(|| TopicEntry {
                name: String::new(),
                status: TopicStatus::Open,
                purpose: None,
                created_at: now,
                updated_at: now,
            })
    }

    /// Apply a parsed forum service event to the registry.
    pub fn apply_event(&mut self, chat_id: &str, event: &ForumServiceEvent, now: u64) {
        match event {
            ForumServiceEvent::Created {
                thread_id, name, ..
            } => self.upsert_created(chat_id, thread_id, name, None, now),
            ForumServiceEvent::Edited { thread_id, name } => {
                if let Some(name) = name {
                    self.rename(chat_id, thread_id, name, now);
                }
            }
            ForumServiceEvent::Closed { thread_id } => {
                self.set_status(chat_id, thread_id, TopicStatus::Closed, now);
            }
            ForumServiceEvent::Reopened { thread_id } => {
                self.set_status(chat_id, thread_id, TopicStatus::Open, now);
            }
        }
    }

    pub fn get(&self, chat_id: &str, thread_id: &str) -> Option<&TopicEntry> {
        self.chats.get(chat_id).and_then(|c| c.get(thread_id))
    }

    /// Resolve a topic name to its numeric `message_thread_id` within a chat.
    /// Case-insensitive on the name. Used at schedule-creation time so the
    /// delivery path only ever carries the resolved id.
    pub fn resolve_thread_id_by_name(&self, chat_id: &str, name: &str) -> Option<String> {
        let chat = self.chats.get(chat_id)?;
        let target = name.trim().to_ascii_lowercase();
        chat.iter()
            .find(|(_, entry)| entry.name.to_ascii_lowercase() == target)
            .map(|(thread_id, _)| thread_id.clone())
    }

    /// All topics for a chat, ordered by `thread_id`.
    pub fn list_for_chat(&self, chat_id: &str) -> Vec<(String, TopicEntry)> {
        self.chats
            .get(chat_id)
            .map(|c| c.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// Load the registry from disk, returning an empty registry when the file
    /// is missing. Returns an error only when the file exists but is corrupt.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomically persist the registry as JSON: write to a sibling temp file,
    /// then rename over the target so a crash never leaves a half-written file.
    pub fn save_atomic(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// A parsed `forum_topic_*` service event from a Telegram message object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForumServiceEvent {
    Created {
        thread_id: String,
        name: String,
        icon_color: Option<i64>,
    },
    Edited {
        thread_id: String,
        name: Option<String>,
    },
    Closed {
        thread_id: String,
    },
    Reopened {
        thread_id: String,
    },
}

/// Extract a forum service event from a Telegram `message` object, if any.
/// Service messages carry `message_thread_id` equal to the topic id.
pub fn parse_forum_service_event(message: &Value) -> Option<ForumServiceEvent> {
    let thread_id = message
        .get("message_thread_id")
        .and_then(Value::as_i64)
        .map(|id| id.to_string())?;

    if let Some(created) = message.get("forum_topic_created") {
        let name = created
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let icon_color = created.get("icon_color").and_then(Value::as_i64);
        return Some(ForumServiceEvent::Created {
            thread_id,
            name,
            icon_color,
        });
    }
    if let Some(edited) = message.get("forum_topic_edited") {
        let name = edited
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        return Some(ForumServiceEvent::Edited { thread_id, name });
    }
    if message.get("forum_topic_closed").is_some() {
        return Some(ForumServiceEvent::Closed { thread_id });
    }
    if message.get("forum_topic_reopened").is_some() {
        return Some(ForumServiceEvent::Reopened { thread_id });
    }
    None
}

/// A parsed owner-facing topic slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopicCommand {
    /// `/new_topic <name>` or `/new_topic <name> | <brief>`
    NewTopic { name: String, brief: Option<String> },
    /// `/topics`
    ListTopics,
    /// `/rename_topic <new name>`
    RenameTopic { new_name: String },
    /// `/close_topic`
    CloseTopic,
    /// `/reopen_topic`
    ReopenTopic,
    /// `/chat_id` — bootstrap helper, not owner-gated.
    ChatId,
}

/// Parse a topic slash command from message text. Handles a trailing
/// `@botname` suffix on the command token. Returns `None` when the text is not
/// one of the recognized topic commands.
pub fn parse_topic_command(text: &str) -> Option<TopicCommand> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let raw_command = parts.next()?;
    // Strip a `@botname` suffix (Telegram addresses commands as `/cmd@bot`).
    let command = raw_command.split('@').next().unwrap_or(raw_command);
    let rest = parts.next().unwrap_or("").trim();

    match command {
        "/new_topic" => {
            let (name, brief) = match rest.split_once('|') {
                Some((n, b)) => {
                    let brief = b.trim();
                    (
                        n.trim().to_string(),
                        if brief.is_empty() {
                            None
                        } else {
                            Some(brief.to_string())
                        },
                    )
                }
                None => (rest.to_string(), None),
            };
            Some(TopicCommand::NewTopic { name, brief })
        }
        "/topics" => Some(TopicCommand::ListTopics),
        "/rename_topic" => Some(TopicCommand::RenameTopic {
            new_name: rest.to_string(),
        }),
        "/close_topic" => Some(TopicCommand::CloseTopic),
        "/reopen_topic" => Some(TopicCommand::ReopenTopic),
        "/chat_id" => Some(TopicCommand::ChatId),
        _ => None,
    }
}

/// Owner allowlist check. `owners` are numeric Telegram user-id strings;
/// `sender_id` is `message.from.id` as a string. Usernames never match because
/// they are compared against the numeric id, not the username.
pub fn is_owner(sender_id: Option<&str>, owners: &[String]) -> bool {
    match sender_id {
        Some(id) => owners.iter().any(|o| o == id),
        None => false,
    }
}

/// Build the seed [`ChannelMessage`] pushed through the normal ingestion path
/// when `/new_topic <name> | <brief>` supplies a brief. It is scoped exactly
/// like an inbound topic message (`ReplyTarget`, `reply_target = chat:thread`,
/// `thread_ts = thread`) so the seed and later replies share one session, and
/// it is `explicitly_addressed` so the orchestrator starts a turn even in
/// mention-only groups.
pub fn build_seed_message(
    alias: &str,
    chat_id: &str,
    thread_id: &str,
    brief: &str,
    sender: &str,
) -> zeroclaw_api::channel::ChannelMessage {
    use zeroclaw_api::channel::{ChannelConversationScope, ChannelMessage};
    ChannelMessage {
        id: format!("telegram_{chat_id}_seed_{thread_id}"),
        sender: sender.to_string(),
        reply_target: format!("{chat_id}:{thread_id}"),
        content: brief.to_string(),
        channel: "telegram".to_string(),
        channel_alias: Some(alias.to_string()),
        thread_ts: Some(thread_id.to_string()),
        conversation_scope: ChannelConversationScope::ReplyTarget,
        explicitly_addressed: true,
        ..Default::default()
    }
}

// ── Forum API request-body builders / response parsers (pure) ───────────────

/// `createForumTopic` request body. `icon_color` is optional.
pub fn build_create_forum_topic_body(chat_id: &str, name: &str, icon_color: Option<i64>) -> Value {
    let mut body = serde_json::json!({ "chat_id": chat_id, "name": name });
    if let Some(color) = icon_color {
        body["icon_color"] = Value::from(color);
    }
    body
}

/// Parse the `message_thread_id` (returned as an integer) from a
/// `createForumTopic` response.
pub fn parse_create_forum_topic_response(v: &Value) -> anyhow::Result<i64> {
    v.get("result")
        .and_then(|r| r.get("message_thread_id"))
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow::Error::msg("createForumTopic response missing message_thread_id"))
}

/// `editForumTopic` request body. `message_thread_id` is a JSON **string**.
pub fn build_edit_forum_topic_body(chat_id: &str, thread_id: &str, name: &str) -> Value {
    serde_json::json!({
        "chat_id": chat_id,
        "message_thread_id": thread_id.to_string(),
        "name": name,
    })
}

/// `closeForumTopic` request body. `message_thread_id` is a JSON **string**.
pub fn build_close_forum_topic_body(chat_id: &str, thread_id: &str) -> Value {
    serde_json::json!({
        "chat_id": chat_id,
        "message_thread_id": thread_id.to_string(),
    })
}

/// `reopenForumTopic` request body. `message_thread_id` is a JSON **string**.
pub fn build_reopen_forum_topic_body(chat_id: &str, thread_id: &str) -> Value {
    serde_json::json!({
        "chat_id": chat_id,
        "message_thread_id": thread_id.to_string(),
    })
}

/// `getChat` request body.
pub fn build_get_chat_body(chat_id: &str) -> Value {
    serde_json::json!({ "chat_id": chat_id })
}

/// Whether a `getChat` response reports the chat as a forum supergroup.
pub fn parse_get_chat_is_forum(v: &Value) -> bool {
    v.get("result")
        .and_then(|r| r.get("is_forum"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `getChatMember` request body. `user_id` is a numeric integer per the API.
pub fn build_get_chat_member_body(chat_id: &str, user_id: i64) -> Value {
    serde_json::json!({ "chat_id": chat_id, "user_id": user_id })
}

/// Whether a `getChatMember` response grants topic management. The group
/// creator has the permission implicitly even when `can_manage_topics` is
/// absent, so `status == "creator"` is treated as allowed.
pub fn parse_get_chat_member_can_manage_topics(v: &Value) -> bool {
    let Some(result) = v.get("result") else {
        return false;
    };
    if result.get("status").and_then(Value::as_str) == Some("creator") {
        return true;
    }
    result
        .get("can_manage_topics")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Base `sendMessage` body used by the delivery/text path.
/// `message_thread_id` is encoded as a JSON **string** when present.
pub fn build_telegram_send_body(
    chat_id: &str,
    thread_id: Option<&str>,
    text: &str,
    parse_mode: Option<&str>,
) -> Value {
    let mut body = serde_json::json!({ "chat_id": chat_id, "text": text });
    if let Some(pm) = parse_mode {
        body["parse_mode"] = Value::String(pm.to_string());
    }
    if let Some(tid) = thread_id {
        body["message_thread_id"] = Value::String(tid.to_string());
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Registry upserts ────────────────────────────────────────────────

    #[test]
    fn registry_upsert_from_created() {
        let mut reg = TopicRegistry::default();
        let event = ForumServiceEvent::Created {
            thread_id: "789".into(),
            name: "Design".into(),
            icon_color: None,
        };
        reg.apply_event("-100200300", &event, 1000);
        let entry = reg.get("-100200300", "789").expect("topic present");
        assert_eq!(entry.name, "Design");
        assert_eq!(entry.status, TopicStatus::Open);
        assert_eq!(entry.created_at, 1000);
        assert_eq!(entry.updated_at, 1000);
    }

    #[test]
    fn registry_upsert_from_edited_renames() {
        let mut reg = TopicRegistry::default();
        reg.apply_event(
            "-100200300",
            &ForumServiceEvent::Created {
                thread_id: "789".into(),
                name: "Design".into(),
                icon_color: None,
            },
            1000,
        );
        reg.apply_event(
            "-100200300",
            &ForumServiceEvent::Edited {
                thread_id: "789".into(),
                name: Some("Architecture".into()),
            },
            2000,
        );
        let entry = reg.get("-100200300", "789").expect("topic present");
        assert_eq!(entry.name, "Architecture");
        assert_eq!(entry.created_at, 1000, "create timestamp preserved");
        assert_eq!(entry.updated_at, 2000, "update timestamp bumped");
    }

    #[test]
    fn registry_upsert_from_closed() {
        let mut reg = TopicRegistry::default();
        reg.apply_event(
            "-100200300",
            &ForumServiceEvent::Created {
                thread_id: "789".into(),
                name: "Design".into(),
                icon_color: None,
            },
            1000,
        );
        reg.apply_event(
            "-100200300",
            &ForumServiceEvent::Closed {
                thread_id: "789".into(),
            },
            2000,
        );
        assert_eq!(
            reg.get("-100200300", "789").unwrap().status,
            TopicStatus::Closed
        );
    }

    #[test]
    fn registry_upsert_from_reopened() {
        let mut reg = TopicRegistry::default();
        reg.apply_event(
            "-100200300",
            &ForumServiceEvent::Created {
                thread_id: "789".into(),
                name: "Design".into(),
                icon_color: None,
            },
            1000,
        );
        reg.apply_event(
            "-100200300",
            &ForumServiceEvent::Closed {
                thread_id: "789".into(),
            },
            2000,
        );
        reg.apply_event(
            "-100200300",
            &ForumServiceEvent::Reopened {
                thread_id: "789".into(),
            },
            3000,
        );
        assert_eq!(
            reg.get("-100200300", "789").unwrap().status,
            TopicStatus::Open
        );
    }

    // ── Service-event parsing ───────────────────────────────────────────

    #[test]
    fn parse_forum_service_event_created() {
        let message = serde_json::json!({
            "message_thread_id": 789,
            "forum_topic_created": { "name": "Design", "icon_color": 7322096 }
        });
        assert_eq!(
            parse_forum_service_event(&message),
            Some(ForumServiceEvent::Created {
                thread_id: "789".into(),
                name: "Design".into(),
                icon_color: Some(7322096),
            })
        );
    }

    #[test]
    fn parse_forum_service_event_edited() {
        let message = serde_json::json!({
            "message_thread_id": 789,
            "forum_topic_edited": { "name": "Architecture" }
        });
        assert_eq!(
            parse_forum_service_event(&message),
            Some(ForumServiceEvent::Edited {
                thread_id: "789".into(),
                name: Some("Architecture".into()),
            })
        );
    }

    #[test]
    fn parse_forum_service_event_closed() {
        let message = serde_json::json!({
            "message_thread_id": 789,
            "forum_topic_closed": {}
        });
        assert_eq!(
            parse_forum_service_event(&message),
            Some(ForumServiceEvent::Closed {
                thread_id: "789".into(),
            })
        );
    }

    #[test]
    fn parse_forum_service_event_reopened() {
        let message = serde_json::json!({
            "message_thread_id": 789,
            "forum_topic_reopened": {}
        });
        assert_eq!(
            parse_forum_service_event(&message),
            Some(ForumServiceEvent::Reopened {
                thread_id: "789".into(),
            })
        );
    }

    #[test]
    fn parse_forum_service_event_none() {
        // Ordinary text message in a topic — not a service event.
        let message = serde_json::json!({
            "message_thread_id": 789,
            "text": "hello"
        });
        assert_eq!(parse_forum_service_event(&message), None);
        // No thread id at all.
        let message = serde_json::json!({ "text": "hi" });
        assert_eq!(parse_forum_service_event(&message), None);
    }

    // ── Serde / disk roundtrip ──────────────────────────────────────────

    #[test]
    fn registry_json_roundtrip() {
        let mut reg = TopicRegistry::default();
        reg.upsert_created("-100200300", "789", "Design", Some("brief".into()), 10);
        reg.upsert_created("-100200300", "790", "Ops", None, 20);
        let json = serde_json::to_string(&reg).unwrap();
        let back: TopicRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(reg, back);
    }

    #[test]
    fn registry_writes_then_reloads_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TELEGRAM_TOPICS_STATE_FILE);

        // Missing file loads as empty.
        let empty = TopicRegistry::load(&path).unwrap();
        assert_eq!(empty, TopicRegistry::default());

        let mut reg = TopicRegistry::default();
        reg.upsert_created("-100200300", "789", "Design", None, 10);
        reg.set_status("-100200300", "789", TopicStatus::Closed, 20);
        reg.save_atomic(&path).unwrap();

        let reloaded = TopicRegistry::load(&path).unwrap();
        assert_eq!(reg, reloaded);
        let entry = reloaded.get("-100200300", "789").unwrap();
        assert_eq!(entry.name, "Design");
        assert_eq!(entry.status, TopicStatus::Closed);
    }

    #[test]
    fn resolve_topic_by_name_at_schedule_time_returns_thread_id() {
        let mut reg = TopicRegistry::default();
        reg.upsert_created("-100200300", "789", "Design", None, 10);
        reg.upsert_created("-100200300", "790", "Ops Room", None, 20);
        // Case-insensitive match.
        assert_eq!(
            reg.resolve_thread_id_by_name("-100200300", "ops room"),
            Some("790".to_string())
        );
        assert_eq!(
            reg.resolve_thread_id_by_name("-100200300", "Design"),
            Some("789".to_string())
        );
        // Unknown name / unknown chat.
        assert_eq!(reg.resolve_thread_id_by_name("-100200300", "missing"), None);
        assert_eq!(reg.resolve_thread_id_by_name("-999", "Design"), None);
    }

    // ── Command parsing ─────────────────────────────────────────────────

    #[test]
    fn parse_topic_command_new_topic() {
        assert_eq!(
            parse_topic_command("/new_topic Design Review"),
            Some(TopicCommand::NewTopic {
                name: "Design Review".into(),
                brief: None,
            })
        );
    }

    #[test]
    fn parse_topic_command_new_topic_with_brief() {
        assert_eq!(
            parse_topic_command("/new_topic Design | draft the RFC outline"),
            Some(TopicCommand::NewTopic {
                name: "Design".into(),
                brief: Some("draft the RFC outline".into()),
            })
        );
    }

    #[test]
    fn parse_topic_command_topics() {
        assert_eq!(parse_topic_command("/topics"), Some(TopicCommand::ListTopics));
    }

    #[test]
    fn parse_topic_command_rename_topic() {
        assert_eq!(
            parse_topic_command("/rename_topic New Name"),
            Some(TopicCommand::RenameTopic {
                new_name: "New Name".into(),
            })
        );
    }

    #[test]
    fn parse_topic_command_close_and_reopen() {
        assert_eq!(
            parse_topic_command("/close_topic"),
            Some(TopicCommand::CloseTopic)
        );
        assert_eq!(
            parse_topic_command("/reopen_topic"),
            Some(TopicCommand::ReopenTopic)
        );
    }

    #[test]
    fn parse_topic_command_chat_id() {
        assert_eq!(parse_topic_command("/chat_id"), Some(TopicCommand::ChatId));
    }

    #[test]
    fn parse_topic_command_strips_botname_suffix() {
        assert_eq!(
            parse_topic_command("/new_topic@mybot Design"),
            Some(TopicCommand::NewTopic {
                name: "Design".into(),
                brief: None,
            })
        );
        assert_eq!(
            parse_topic_command("/chat_id@mybot"),
            Some(TopicCommand::ChatId)
        );
    }

    #[test]
    fn parse_topic_command_non_command() {
        assert_eq!(parse_topic_command("hello there"), None);
        assert_eq!(parse_topic_command("/unknown_command x"), None);
    }

    // ── Owner gate ──────────────────────────────────────────────────────

    #[test]
    fn owner_gate_reads_from_id_not_username() {
        let owners = vec!["555".to_string()];
        // Numeric id matches.
        assert!(is_owner(Some("555"), &owners));
        // A username-based owner list never matches a numeric sender id.
        let username_owners = vec!["alice".to_string()];
        assert!(!is_owner(Some("555"), &username_owners));
    }

    #[test]
    fn owner_gate_rejects_non_owner() {
        let owners = vec!["555".to_string()];
        assert!(!is_owner(Some("999"), &owners));
        assert!(!is_owner(None, &owners));
    }

    // ── Forum API builders / parsers ────────────────────────────────────

    #[test]
    fn build_create_forum_topic_body_with_and_without_color() {
        assert_eq!(
            build_create_forum_topic_body("-100200300", "Design", None),
            serde_json::json!({ "chat_id": "-100200300", "name": "Design" })
        );
        assert_eq!(
            build_create_forum_topic_body("-100200300", "Design", Some(7322096)),
            serde_json::json!({ "chat_id": "-100200300", "name": "Design", "icon_color": 7322096 })
        );
    }

    #[test]
    fn parse_create_forum_topic_response_extracts_thread_id() {
        let resp = serde_json::json!({
            "ok": true,
            "result": { "message_thread_id": 789, "name": "Design" }
        });
        assert_eq!(parse_create_forum_topic_response(&resp).unwrap(), 789);
        let bad = serde_json::json!({ "ok": true, "result": {} });
        assert!(parse_create_forum_topic_response(&bad).is_err());
    }

    #[test]
    fn edit_close_reopen_bodies_encode_thread_id_as_string() {
        // message_thread_id MUST be the string form, never an integer.
        assert_eq!(
            build_edit_forum_topic_body("-100200300", "789", "Architecture"),
            serde_json::json!({
                "chat_id": "-100200300",
                "message_thread_id": "789",
                "name": "Architecture",
            })
        );
        assert_eq!(
            build_close_forum_topic_body("-100200300", "789"),
            serde_json::json!({ "chat_id": "-100200300", "message_thread_id": "789" })
        );
        assert_eq!(
            build_reopen_forum_topic_body("-100200300", "789"),
            serde_json::json!({ "chat_id": "-100200300", "message_thread_id": "789" })
        );
    }

    #[test]
    fn parse_get_chat_is_forum_reads_flag() {
        assert!(parse_get_chat_is_forum(
            &serde_json::json!({ "result": { "is_forum": true } })
        ));
        assert!(!parse_get_chat_is_forum(
            &serde_json::json!({ "result": { "is_forum": false } })
        ));
        assert!(!parse_get_chat_is_forum(&serde_json::json!({ "result": {} })));
    }

    #[test]
    fn parse_get_chat_member_can_manage_topics_variants() {
        // Explicit permission.
        assert!(parse_get_chat_member_can_manage_topics(&serde_json::json!({
            "result": { "status": "administrator", "can_manage_topics": true }
        })));
        // Missing permission.
        assert!(!parse_get_chat_member_can_manage_topics(&serde_json::json!({
            "result": { "status": "administrator", "can_manage_topics": false }
        })));
        // Creator is allowed even without the flag.
        assert!(parse_get_chat_member_can_manage_topics(&serde_json::json!({
            "result": { "status": "creator" }
        })));
        // Ordinary member.
        assert!(!parse_get_chat_member_can_manage_topics(&serde_json::json!({
            "result": { "status": "member" }
        })));
    }

    // ── Seed message ────────────────────────────────────────────────────

    #[test]
    fn build_seed_message_is_replytarget_scoped_and_addressed() {
        use zeroclaw_api::channel::ChannelConversationScope;
        let msg = build_seed_message("work", "-100200300", "789", "draft the RFC", "555");
        assert_eq!(msg.reply_target, "-100200300:789");
        assert_eq!(msg.thread_ts.as_deref(), Some("789"));
        assert_eq!(msg.conversation_scope, ChannelConversationScope::ReplyTarget);
        assert!(msg.explicitly_addressed);
        assert_eq!(msg.content, "draft the RFC");
        assert_eq!(msg.channel, "telegram");
        assert_eq!(msg.channel_alias.as_deref(), Some("work"));
        assert_eq!(msg.sender, "555");
    }

    // ── Delivery body builder ───────────────────────────────────────────

    #[test]
    fn delivery_body_includes_string_message_thread_id_when_set() {
        let body = build_telegram_send_body("-100200300", Some("789"), "hi", None);
        assert_eq!(
            body.get("message_thread_id"),
            Some(&Value::String("789".to_string())),
            "message_thread_id must be the string form"
        );
        assert_eq!(body.get("chat_id"), Some(&Value::String("-100200300".into())));
    }

    #[test]
    fn delivery_body_omitted_when_absent() {
        let body = build_telegram_send_body("-100200300", None, "hi", Some("HTML"));
        assert!(body.get("message_thread_id").is_none());
        assert_eq!(body.get("parse_mode"), Some(&Value::String("HTML".into())));
    }
}

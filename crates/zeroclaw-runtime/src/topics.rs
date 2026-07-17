//! Cross-crate bridge for forum-topic operations.
//!
//! `zeroclaw-channels` depends on `zeroclaw-runtime`, so the runtime cannot
//! import channels. Agent tools live in the runtime, yet the actual Telegram
//! forum work must run against the live channel instance that lives in
//! `zeroclaw-channels`. This module mirrors the `DeliveryFn` bridge in
//! [`crate::cron::scheduler`]: it defines the operation types plus a global
//! [`OnceLock`]-backed handler that the binary crate wires at startup with a
//! closure calling `zeroclaw_channels::orchestrator::perform_topic_op`.
//!
//! Unlike `deliver_announcement` (which treats a missing handler as a benign
//! no-op so cron jobs still record success), a topic operation is invoked
//! interactively by an agent tool or at schedule-creation time: the caller must
//! learn that it failed, so a missing handler returns a clear error.

use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use zeroclaw_config::schema::Config;

/// A forum-topic operation requested by an agent tool or the scheduler.
///
/// The requesting agent never supplies an arbitrary `chat_id`: the caller
/// resolves the master forum group from configuration and passes it alongside
/// the op, so the LLM cannot target a group it was not granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopicOp {
    /// Create a new forum topic, optionally seeding its session with a brief.
    Create {
        name: String,
        brief: Option<String>,
    },
    /// List the tracked topics for the group.
    List,
    /// Rename an existing topic identified by its current name.
    Rename { name: String, new_name: String },
    /// Close an existing topic identified by name.
    Close { name: String },
    /// Reopen a closed topic identified by name.
    Reopen { name: String },
    /// Resolve a topic name to its numeric `message_thread_id`.
    /// Used at schedule-creation time so the delivery path only carries the id.
    ResolveThreadId { name: String },
}

/// Result of a [`TopicOp`]. `message` is a human/LLM-facing summary; `thread_id`
/// is the resolved numeric topic id when the op produced or resolved one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicOpOutcome {
    pub message: String,
    pub thread_id: Option<String>,
}

/// Topic-op handler type — takes owned values so the returned future is `'static`.
/// Mirrors [`crate::cron::scheduler::DeliveryFn`].
pub type TopicOpFn = Box<
    dyn Fn(Config, String, String, TopicOp) -> Pin<Box<dyn Future<Output = Result<TopicOpOutcome>> + Send>>
        + Send
        + Sync,
>;

/// Global topic-op handler, injected by the binary crate at startup.
static TOPIC_OP_FN: OnceLock<TopicOpFn> = OnceLock::new();

/// Register the channel topic-op handler. Called once at startup by the binary.
/// Idempotent: the first registration wins (backed by [`OnceLock`]).
pub fn register_topic_op_fn(f: TopicOpFn) {
    let _ = TOPIC_OP_FN.set(f);
}

/// Error returned when no topic-op handler is registered. Extracted as a pure
/// function so it is unit-testable without touching the process-global handler.
fn no_topic_op_handler_error() -> anyhow::Error {
    anyhow::Error::msg(
        "Telegram forum-topic operations are unavailable: no handler is registered \
         (the channel runtime is not running, or register_topic_op_fn was not called).",
    )
}

/// Perform a forum-topic operation against the live channel for `alias`.
///
/// `chat_id` is the master forum group id resolved by the caller from config.
/// Returns a clear error when no handler is registered.
pub async fn perform_topic_op(
    config: &Config,
    alias: &str,
    chat_id: &str,
    op: TopicOp,
) -> Result<TopicOpOutcome> {
    match TOPIC_OP_FN.get() {
        Some(f) => f(config.clone(), alias.to_string(), chat_id.to_string(), op).await,
        None => Err(no_topic_op_handler_error()),
    }
}

/// Shared echo handler for the whole runtime test binary. `TOPIC_OP_FN` is a
/// process-global `OnceLock`, so every test that needs a registered handler
/// funnels through this one deterministic echo so their expectations never
/// conflict. The echo encodes the op + inputs into `message` and resolves the
/// reserved topic name `"planning"` to thread id `"4242"`.
#[cfg(test)]
pub(crate) fn register_echo_topic_op_fn() {
    register_topic_op_fn(Box::new(|_config, alias, chat_id, op| {
        Box::pin(async move {
            let (message, thread_id) = match op {
                TopicOp::Create { name, brief } => (
                    format!("create|{alias}|{chat_id}|{name}|{brief:?}"),
                    Some("1001".to_string()),
                ),
                TopicOp::List => (format!("list|{alias}|{chat_id}"), None),
                TopicOp::Rename { name, new_name } => {
                    (format!("rename|{chat_id}|{name}|{new_name}"), None)
                }
                TopicOp::Close { name } => (format!("close|{chat_id}|{name}"), None),
                TopicOp::Reopen { name } => (format!("reopen|{chat_id}|{name}"), None),
                TopicOp::ResolveThreadId { name } => {
                    let tid = if name.eq_ignore_ascii_case("planning") {
                        Some("4242".to_string())
                    } else {
                        None
                    };
                    (format!("resolve|{chat_id}|{name}"), tid)
                }
            };
            Ok(TopicOpOutcome { message, thread_id })
        })
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_handler_error_is_clear() {
        let err = no_topic_op_handler_error();
        let msg = err.to_string();
        assert!(msg.contains("no handler"), "message should name the cause: {msg}");
        assert!(
            msg.contains("register_topic_op_fn"),
            "message should point at the wiring: {msg}"
        );
    }

    #[tokio::test]
    async fn perform_topic_op_routes_to_registered_handler() {
        register_echo_topic_op_fn();
        let config = Config::default();
        let outcome = perform_topic_op(
            &config,
            "work",
            "-100200300",
            TopicOp::Create {
                name: "Design".to_string(),
                brief: Some("draft".to_string()),
            },
        )
        .await
        .expect("registered handler returns Ok");
        assert_eq!(outcome.thread_id.as_deref(), Some("1001"));
        assert!(outcome.message.starts_with("create|work|-100200300|Design|"));
    }

    #[tokio::test]
    async fn perform_topic_op_resolve_thread_id_via_handler() {
        register_echo_topic_op_fn();
        let config = Config::default();
        let outcome = perform_topic_op(
            &config,
            "work",
            "-100200300",
            TopicOp::ResolveThreadId {
                name: "planning".to_string(),
            },
        )
        .await
        .expect("registered handler returns Ok");
        assert_eq!(outcome.thread_id.as_deref(), Some("4242"));
    }
}

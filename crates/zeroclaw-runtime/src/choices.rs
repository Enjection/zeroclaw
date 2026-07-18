//! Cross-crate bridge for multiple-choice "render inline buttons and wait for
//! a tap" operations.
//!
//! Mirrors [`crate::topics`]: `zeroclaw-channels` depends on `zeroclaw-runtime`,
//! so the runtime cannot import channels directly. The agent-callable
//! `telegram_mc_choice` tool lives in the runtime, yet the actual Telegram
//! send-and-wait-for-tap work must run against the live channel instance that
//! lives in `zeroclaw-channels`. This module defines the operation types plus
//! a global [`OnceLock`]-backed handler that the binary crate wires at
//! startup with a closure calling
//! `zeroclaw_channels::orchestrator::perform_choice_op`.
//!
//! Like a topic operation (and unlike `deliver_announcement`), a choice
//! operation is invoked interactively by an agent tool: the caller must learn
//! that it failed, so a missing handler returns a clear error.

use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use zeroclaw_config::schema::Config;

/// A multiple-choice operation requested by an agent tool.
///
/// The requesting agent never supplies an arbitrary `chat_id`: the caller
/// resolves the target group from configuration and passes it alongside the
/// op, so the LLM cannot target a group it was not granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChoiceOp {
    /// Render `prompt` with one inline button per `options` entry and wait
    /// (bounded) for the user's tap.
    RenderChoice { prompt: String, options: Vec<String> },
}

/// Result of a [`ChoiceOp`]. `message` is a human/LLM-facing summary of the
/// outcome (which option was chosen, or that the prompt timed out).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceOutcome {
    pub chosen_index: Option<usize>,
    pub chosen_text: Option<String>,
    pub message: String,
}

/// Choice-op handler type — takes owned values so the returned future is
/// `'static`. Mirrors [`crate::topics::TopicOpFn`].
pub type ChoiceOpFn = Box<
    dyn Fn(Config, String, String, ChoiceOp) -> Pin<Box<dyn Future<Output = Result<ChoiceOutcome>> + Send>>
        + Send
        + Sync,
>;

/// Global choice-op handler, injected by the binary crate at startup.
static CHOICE_OP_FN: OnceLock<ChoiceOpFn> = OnceLock::new();

/// Register the channel choice-op handler. Called once at startup by the
/// binary. Idempotent: the first registration wins (backed by [`OnceLock`]).
pub fn register_choice_op_fn(f: ChoiceOpFn) {
    let _ = CHOICE_OP_FN.set(f);
}

/// Error returned when no choice-op handler is registered. Extracted as a
/// pure function so it is unit-testable without touching the process-global
/// handler.
fn no_choice_op_handler_error() -> anyhow::Error {
    anyhow::Error::msg(
        "Telegram multiple-choice operations are unavailable: no handler is registered \
         (the channel runtime is not running, or register_choice_op_fn was not called).",
    )
}

/// Perform a multiple-choice operation against the live channel for `alias`.
///
/// `chat_id` is the target group id resolved by the caller from config.
/// Returns a clear error when no handler is registered.
pub async fn perform_choice_op(
    config: &Config,
    alias: &str,
    chat_id: &str,
    op: ChoiceOp,
) -> Result<ChoiceOutcome> {
    match CHOICE_OP_FN.get() {
        Some(f) => f(config.clone(), alias.to_string(), chat_id.to_string(), op).await,
        None => Err(no_choice_op_handler_error()),
    }
}

/// Shared echo handler for the whole runtime test binary. `CHOICE_OP_FN` is a
/// process-global `OnceLock`, so every test that needs a registered handler
/// funnels through this one deterministic echo so their expectations never
/// conflict. The echo encodes the op + inputs into `message` and always
/// "chooses" the first offered option (or none, if the list is empty).
#[cfg(test)]
pub(crate) fn register_echo_choice_op_fn() {
    register_choice_op_fn(Box::new(|_config, alias, chat_id, op| {
        Box::pin(async move {
            let ChoiceOp::RenderChoice { prompt, options } = op;
            let (chosen_index, chosen_text) = match options.first() {
                Some(first) => (Some(0), Some(first.clone())),
                None => (None, None),
            };
            let message = format!("choice|{alias}|{chat_id}|{prompt}|{}", options.join(","));
            Ok(ChoiceOutcome {
                chosen_index,
                chosen_text,
                message,
            })
        })
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_handler_error_is_clear() {
        let err = no_choice_op_handler_error();
        let msg = err.to_string();
        assert!(msg.contains("no handler"), "message should name the cause: {msg}");
        assert!(
            msg.contains("register_choice_op_fn"),
            "message should point at the wiring: {msg}"
        );
    }

    #[tokio::test]
    async fn perform_choice_op_routes_to_registered_handler() {
        register_echo_choice_op_fn();
        let config = Config::default();
        let outcome = perform_choice_op(
            &config,
            "work",
            "-100200300",
            ChoiceOp::RenderChoice {
                prompt: "Pick one".to_string(),
                options: vec!["A".to_string(), "B".to_string()],
            },
        )
        .await
        .expect("registered handler returns Ok");
        assert_eq!(outcome.chosen_index, Some(0));
        assert_eq!(outcome.chosen_text.as_deref(), Some("A"));
        assert!(
            outcome.message.starts_with("choice|work|-100200300|Pick one|A,B"),
            "unexpected message: {}",
            outcome.message
        );
    }
}

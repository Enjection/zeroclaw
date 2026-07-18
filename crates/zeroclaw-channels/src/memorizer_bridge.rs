//! Memorizer bridge: the thin ZeroClaw side of the memorizer study system.
//!
//! ZeroClaw long-polls the memorizer app's `/bridge/v1/next` for a pending quiz
//! question, renders it fire-and-forget into the deck's forum topic (MC → inline
//! buttons with a `memq:` callback prefix), and — when the user taps — POSTs the
//! answer to `/bridge/v1/answer`. It holds NO scheduling/grading/SRS state; the
//! app's `question_queue` is the durable source of truth, so a restarted bridge
//! rebuilds correlation from the next poll.
//!
//! Pure builders/parsers live here (unit-tested); the async callback glue is in
//! [`crate::telegram`]. `message_thread_id` is a JSON string everywhere, matching
//! [`crate::telegram_topics`].

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

/// Prefix on inline-button `callback_data` for memorizer taps. Distinct from the
/// approval flow's `approval:` prefix so `listen()`'s router never confuses them.
pub const MEMQ_PREFIX: &str = "memq:";

/// Telegram's hard limit on inline-button `callback_data` (bytes).
pub const CALLBACK_DATA_MAX: usize = 64;

/// A pending quiz question pulled from the app's `/bridge/v1/next`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PendingQuestion {
    pub short_qid: String,
    #[serde(default)]
    pub deck_id: String,
    #[serde(rename = "type")]
    pub qtype: String,
    pub front: String,
    #[serde(default)]
    pub options: Vec<String>,
}

/// Envelope of `GET /bridge/v1/next` (200 body). A 204 means no question.
#[derive(Debug, Deserialize)]
struct NextEnvelope {
    question: Option<PendingQuestion>,
}

/// A decoded `memq:` inline-button tap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemqTap {
    pub short_qid: String,
    pub choice_idx: usize,
}

/// Parse a `memq:<short_qid>:<idx>` callback into its parts. Returns `None` for
/// any non-memq or malformed payload, so it can be tried alongside the approval
/// parser without stealing its taps.
pub fn parse_memq_callback(cb_data: &str) -> Option<MemqTap> {
    let rest = cb_data.strip_prefix(MEMQ_PREFIX)?;
    let (short_qid, idx) = rest.rsplit_once(':')?;
    if short_qid.is_empty() {
        return None;
    }
    let choice_idx = idx.parse::<usize>().ok()?;
    Some(MemqTap {
        short_qid: short_qid.to_string(),
        choice_idx,
    })
}

/// Build the `callback_data` for choice `idx` of `short_qid`.
pub fn memq_callback_data(short_qid: &str, idx: usize) -> String {
    format!("{MEMQ_PREFIX}{short_qid}:{idx}")
}

fn letter_for(idx: usize) -> char {
    (b'A' + (idx as u8 % 26)) as char
}

/// Build a `sendRichMessage` body for a multiple-choice question. The front is
/// rendered as markdown so the agent's **bold** framing and any `$…$` LaTeX both
/// typeset (used for ALL MC questions — plain `sendMessage` shows markdown
/// literally). When an option contains math, options are listed in the body with
/// A/B/C buttons (button labels can't render LaTeX); otherwise the option text
/// sits on the button. `callback_data` carries the option index either way.
/// `thread_id` targets the deck's forum topic.
pub fn build_rich_mc_send_body(
    chat_id: &str,
    thread_id: Option<&str>,
    q: &PendingQuestion,
) -> anyhow::Result<Value> {
    let options_have_math = q.options.iter().any(|o| o.contains('$'));
    let mut md = q.front.clone();
    let mut rows: Vec<Value> = Vec::with_capacity(q.options.len());
    for (idx, opt) in q.options.iter().enumerate() {
        let data = memq_callback_data(&q.short_qid, idx);
        if data.len() > CALLBACK_DATA_MAX {
            anyhow::bail!(
                "callback_data {data:?} is {} bytes, over Telegram's {CALLBACK_DATA_MAX}-byte limit",
                data.len(),
            );
        }
        // Button labels can't render LaTeX: when an option carries math, list the
        // options in the body (A) …) with A/B/C buttons; otherwise put the option
        // text directly on the button (nicer to tap).
        if options_have_math {
            let letter = letter_for(idx);
            md.push_str(&format!("\n\n{}) {}", letter, opt));
            rows.push(json!([{ "text": letter.to_string(), "callback_data": data }]));
        } else {
            rows.push(json!([{ "text": opt, "callback_data": data }]));
        }
    }
    let mut body = json!({
        "chat_id": chat_id,
        "rich_message": { "markdown": md },
        "reply_markup": { "inline_keyboard": rows },
    });
    if let Some(tid) = thread_id {
        body["message_thread_id"] = Value::String(tid.to_string());
    }
    Ok(body)
}

/// How the user answered a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerKind {
    Tap,
    Text,
}

impl AnswerKind {
    fn as_str(self) -> &'static str {
        match self {
            AnswerKind::Tap => "tap",
            AnswerKind::Text => "text",
        }
    }
}

/// Transport to the memorizer app's `/bridge/v1` contract. A trait so the poll/
/// answer glue is unit-tested with a fake while the real client is wiremocked.
#[async_trait]
pub trait BridgeTransport: Send + Sync {
    /// Long-poll for the next pending question (`None` after the wait window).
    async fn next(&self, wait_secs: u64) -> anyhow::Result<Option<PendingQuestion>>;
    /// POST the user's answer (idempotent app-side by `short_qid`).
    async fn answer(&self, short_qid: &str, kind: AnswerKind, value: &str) -> anyhow::Result<()>;
}

/// Real HTTP transport (reqwest). `with_base_url` lets tests point at wiremock,
/// mirroring `TelegramChannel::with_api_base`.
pub struct HttpBridgeTransport {
    base_url: String,
    bearer: String,
    client: reqwest::Client,
}

impl HttpBridgeTransport {
    pub fn new(base_url: impl Into<String>, bearer: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            bearer: bearer.into(),
            client: reqwest::Client::new(),
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into().trim_end_matches('/').to_string();
        self
    }
}

#[async_trait]
impl BridgeTransport for HttpBridgeTransport {
    async fn next(&self, wait_secs: u64) -> anyhow::Result<Option<PendingQuestion>> {
        let url = format!("{}/bridge/v1/next?worker=tg&wait={wait_secs}", self.base_url);
        let resp = self.client.get(url).bearer_auth(&self.bearer).send().await?;
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if !resp.status().is_success() {
            anyhow::bail!("bridge /next HTTP {}", resp.status());
        }
        let env: NextEnvelope = resp.json().await?;
        Ok(env.question)
    }

    async fn answer(&self, short_qid: &str, kind: AnswerKind, value: &str) -> anyhow::Result<()> {
        let url = format!("{}/bridge/v1/answer", self.base_url);
        let body = json!({ "short_qid": short_qid, "kind": kind.as_str(), "value": value });
        let resp = self
            .client
            .post(url)
            .bearer_auth(&self.bearer)
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("bridge /answer HTTP {}", resp.status());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample() -> PendingQuestion {
        PendingQuestion {
            short_qid: "q1".into(),
            deck_id: "d1".into(),
            qtype: "mc".into(),
            front: "2+2?".into(),
            options: vec!["3".into(), "4".into(), "5".into()],
        }
    }

    #[test]
    fn parse_memq_happy() {
        let t = parse_memq_callback("memq:q1:2").unwrap();
        assert_eq!(t.short_qid, "q1");
        assert_eq!(t.choice_idx, 2);
    }

    #[test]
    fn parse_memq_rejects_non_memq_and_malformed() {
        assert!(parse_memq_callback("approval:abc:approve").is_none());
        assert!(parse_memq_callback("memq:q1").is_none()); // no idx separator
        assert!(parse_memq_callback("memq::2").is_none()); // empty short_qid
        assert!(parse_memq_callback("memq:q1:x").is_none()); // idx not a number
    }

    #[test]
    fn memq_and_approval_prefixes_do_not_collide() {
        // The approval router uses strip_prefix("approval:"); ours uses "memq:".
        assert!("memq:q1:0".strip_prefix("approval:").is_none());
        assert!(parse_memq_callback("approval:x:approve").is_none());
    }

    #[test]
    fn mc_plain_options_put_text_on_buttons() {
        // No math → option text on the buttons (nicer to tap); front is markdown.
        let body = build_rich_mc_send_body("-100200300", Some("42"), &sample()).unwrap();
        assert!(body["rich_message"]["markdown"].as_str().unwrap().contains("2+2?"));
        let kb = body["reply_markup"]["inline_keyboard"].as_array().unwrap();
        assert_eq!(kb.len(), 3);
        assert_eq!(body["message_thread_id"], Value::String("42".into()));
        assert_eq!(kb[0][0]["text"], "3"); // option text, not a letter
        assert_eq!(kb[0][0]["callback_data"], "memq:q1:0");
        assert_eq!(kb[2][0]["callback_data"], "memq:q1:2");
    }

    fn math_sample() -> PendingQuestion {
        PendingQuestion {
            short_qid: "q1".into(),
            deck_id: String::new(),
            qtype: "mc".into(),
            front: "pick".into(),
            options: vec!["$\\tfrac12$".into(), "$1$".into(), "$2$".into()],
        }
    }

    #[test]
    fn rich_math_options_use_lettered_buttons_and_body() {
        // Options carry math → A/B/C buttons, option text in the body.
        let body = build_rich_mc_send_body("-100200300", Some("42"), &math_sample()).unwrap();
        let kb = body["reply_markup"]["inline_keyboard"].as_array().unwrap();
        assert_eq!(kb.len(), 3);
        assert_eq!(body["message_thread_id"], Value::String("42".into()));
        assert_eq!(kb[0][0]["text"], "A");
        assert_eq!(kb[2][0]["text"], "C");
    }

    #[test]
    fn rich_letters_map_to_callback_index() {
        let body = build_rich_mc_send_body("-1", None, &math_sample()).unwrap();
        let kb = body["reply_markup"]["inline_keyboard"].as_array().unwrap();
        assert_eq!(kb[0][0]["callback_data"], "memq:q1:0");
        assert_eq!(kb[2][0]["callback_data"], "memq:q1:2");
        let md = body["rich_message"]["markdown"].as_str().unwrap();
        assert!(md.contains("A) $\\tfrac12$"), "{md}");
        assert!(md.contains("C) $2$"), "{md}");
    }

    #[test]
    fn rich_body_rejects_oversize_callback_data() {
        let big = PendingQuestion {
            short_qid: "x".repeat(70),
            deck_id: String::new(),
            qtype: "mc".into(),
            front: "q".into(),
            options: vec!["a".into(), "b".into()],
        };
        assert!(build_rich_mc_send_body("-1", None, &big).is_err());
    }

    #[test]
    fn rich_body_passes_markdown_and_math_verbatim() {
        // The front is markdown: intentional **bold** must survive unchanged
        // (escaping it would print literal asterisks), and $…$ LaTeX stays verbatim.
        let body = build_rich_mc_send_body(
            "-1",
            None,
            &PendingQuestion {
                short_qid: "q".into(),
                deck_id: String::new(),
                qtype: "mc".into(),
                front: "**Question 1** — evaluate $\\frac{1}{2}$".into(),
                options: vec!["a".into(), "b".into()],
            },
        )
        .unwrap();
        let md = body["rich_message"]["markdown"].as_str().unwrap();
        assert!(md.contains("**Question 1**"), "markdown bold must pass through: {md}");
        assert!(md.contains("$\\frac{1}{2}$"), "math must be verbatim: {md}");
    }

    #[tokio::test]
    async fn http_next_parses_question() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bridge/v1/next"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "question": {
                    "short_qid": "q1", "deck_id": "d1", "type": "mc",
                    "front": "2+2?", "options": ["3", "4", "5"]
                }
            })))
            .mount(&server)
            .await;
        let t = HttpBridgeTransport::new(server.uri(), "tok");
        let got = t.next(1).await.unwrap().unwrap();
        assert_eq!(got.short_qid, "q1");
        assert_eq!(got.options.len(), 3);
    }

    #[tokio::test]
    async fn http_next_204_is_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bridge/v1/next"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let t = HttpBridgeTransport::new(server.uri(), "tok");
        assert!(t.next(1).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn http_answer_posts_expected_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bridge/v1/answer"))
            .and(body_json(
                json!({ "short_qid": "q1", "kind": "tap", "value": "4" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ack": true, "applied": true})))
            .expect(1)
            .mount(&server)
            .await;
        let t = HttpBridgeTransport::new(server.uri(), "tok");
        t.answer("q1", AnswerKind::Tap, "4").await.unwrap();
    }
}

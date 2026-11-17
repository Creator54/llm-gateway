//! Translate requests and replies between Responses, Chat, and Messages.
//!
//! Adapted from joocode (MIT); see `LICENSE-joocode`.

mod claude;
mod reply;
mod request;
mod sse;
mod stream;

pub use claude::{chat_request_to_messages, messages_reply_to_chat, MessagesToChat};
pub use reply::{chat_to_messages, chat_to_responses, responses_reply_to_chat};
pub use request::{
    chat_to_responses as chat_request_to_responses, messages_to_chat, responses_to_chat,
    tool_namespaces,
};

/// Flattened tool name to the `(namespace, name)` the client declared it under.
pub type ToolNamespaces = std::collections::BTreeMap<String, (String, String)>;

/// A `function_call` item's `name`, plus `namespace` when the wire name was flattened.
fn name_call(item: &mut serde_json::Value, wire: &str, namespaces: &ToolNamespaces) {
    match namespaces.get(wire) {
        Some((namespace, name)) => {
            item["namespace"] = namespace.as_str().into();
            item["name"] = name.as_str().into();
        }
        None => item["name"] = wire.into(),
    }
}
pub use sse::SseParser;
pub use stream::{ChatStream, MessagesStream, ResponsesStream};

use serde_json::{json, Value};

/// The protocol a client spoke, which picks its error envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inbound {
    Chat,
    Responses,
    Messages,
}
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A translation failure with the status the client should see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// HTTP status to return.
    pub status: u16,
    /// Short message safe to show the client.
    pub message: String,
}

impl Error {
    pub fn bad_request(message: &str) -> Self {
        Self {
            status: 400,
            message: message.to_owned(),
        }
    }

    pub fn bad_gateway(message: &str) -> Self {
        Self {
            status: 502,
            message: message.to_owned(),
        }
    }

    /// Error body in the shape the inbound protocol expects.
    pub fn to_body(&self, inbound: Inbound) -> Value {
        let kind = match self.status {
            429 => "rate_limit_error",
            0..500 => "invalid_request_error",
            _ => "upstream_error",
        };
        match inbound {
            Inbound::Messages => json!({
                "type": "error",
                "error": {
                    "type": if self.status < 500 { kind } else { "api_error" },
                    "message": self.message,
                }
            }),
            Inbound::Chat | Inbound::Responses => json!({
                "error": { "message": self.message, "type": kind }
            }),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status, self.message)
    }
}

impl std::error::Error for Error {}

/// A fresh id for an object the client will see.
fn new_id(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let count = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}_{nanos:x}{count:04x}")
}

/// Seconds since the epoch, matching OpenAI's `created_at`.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn usage_number(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn prompt_tokens(usage: &Value) -> u64 {
    usage_number(usage, "prompt_tokens")
}

fn completion_tokens(usage: &Value) -> u64 {
    usage_number(usage, "completion_tokens")
}

fn cached_tokens(usage: &Value) -> u64 {
    usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Responses `usage` object as a Chat `usage` object.
fn chat_usage(usage: &Value) -> Value {
    let prompt = usage_number(usage, "input_tokens");
    let completion = usage_number(usage, "output_tokens");
    let total = usage_number(usage, "total_tokens");
    let cached = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .pointer("/output_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": if total == 0 { prompt + completion } else { total },
        "prompt_tokens_details": { "cached_tokens": cached },
        "completion_tokens_details": { "reasoning_tokens": reasoning },
    })
}

/// Chat usage as a Responses `usage` object.
fn responses_usage(usage: &Value) -> Value {
    let prompt = prompt_tokens(usage);
    let completion = completion_tokens(usage);
    let total = usage_number(usage, "total_tokens");
    json!({
        "input_tokens": prompt,
        "input_tokens_details": { "cached_tokens": cached_tokens(usage) },
        "output_tokens": completion,
        "total_tokens": if total == 0 { prompt + completion } else { total },
    })
}

/// Chat usage as a Messages `usage` object; Anthropic counts cached input apart.
fn messages_usage(usage: &Value) -> Value {
    let cached = cached_tokens(usage);
    json!({
        "input_tokens": prompt_tokens(usage).saturating_sub(cached),
        "cache_read_input_tokens": cached,
        "output_tokens": completion_tokens(usage),
    })
}

/// Anthropic stop reason for a Chat `finish_reason`.
fn stop_reason(finish: Option<&str>, has_tool_calls: bool) -> &'static str {
    if has_tool_calls {
        return "tool_use";
    }
    match finish {
        Some("length") => "max_tokens",
        Some("tool_calls") => "tool_use",
        _ => "end_turn",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_openai_and_anthropic_error_bodies() {
        let error = Error::bad_request("bad input");
        assert_eq!(
            error.to_body(Inbound::Chat),
            json!({ "error": { "message": "bad input", "type": "invalid_request_error" } })
        );
        assert_eq!(
            error.to_body(Inbound::Responses),
            error.to_body(Inbound::Chat)
        );
        assert_eq!(
            error.to_body(Inbound::Messages),
            json!({ "type": "error", "error": { "type": "invalid_request_error", "message": "bad input" } })
        );

        let error = Error {
            status: 429,
            message: "slow down".into(),
        };
        for inbound in [Inbound::Chat, Inbound::Messages] {
            assert_eq!(error.to_body(inbound)["error"]["type"], "rate_limit_error");
        }

        let error = Error::bad_gateway("upstream failed");
        assert_eq!(
            error.to_body(Inbound::Chat)["error"]["type"],
            "upstream_error"
        );
        assert_eq!(
            error.to_body(Inbound::Messages)["error"]["type"],
            "api_error"
        );
    }
}

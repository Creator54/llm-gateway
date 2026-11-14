//! Local token counting for Anthropic `/v1/messages/count_tokens`.
//!
//! No tokenizer ships with the gateway: the estimate is deterministic, cheap, and
//! close enough for context budgeting. Facts from real usage can correct it per model
//! family through [`Calibrate`].

use bytes::Bytes;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Anthropic bills an image as pixels / 750.
const PIXELS_PER_TOKEN: u64 = 750;
/// Anthropic's own fallback when an image has no known dimensions.
const IMAGE_FALLBACK: u64 = 1600;
/// Framing Anthropic adds around each message and each request. Approximate.
const REQUEST_OVERHEAD: u64 = 3;
const MESSAGE_OVERHEAD: u64 = 4;

/// Estimated input tokens for an Anthropic Messages request body.
pub fn estimate(body: &[u8]) -> u64 {
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => estimate_value(&value),
        Err(_) => text_tokens(&String::from_utf8_lossy(body)),
    }
}

/// `{"input_tokens": N}`, the count_tokens response body.
pub fn response(input_tokens: u64) -> Bytes {
    Bytes::from(
        serde_json::to_vec(&json!({ "input_tokens": input_tokens }))
            .expect("count_tokens response is plain JSON"),
    )
}

/// Per-model-family correction factors, fed from journal usage once it is available.
#[derive(Debug, Clone, Default)]
pub struct Calibrate {
    /// Model family (the id after the provider) -> multiplier. Longest match wins.
    factors: BTreeMap<String, f64>,
}

impl Calibrate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a multiplier. Empty families and values that would make a count
    /// meaningless are ignored.
    pub fn set(&mut self, family: &str, multiplier: f64) {
        if family.is_empty() || !multiplier.is_finite() || multiplier <= 0.0 {
            return;
        }
        self.factors.insert(family.to_owned(), multiplier);
    }

    /// Multiplier for a client model id such as `anthropic/claude-sonnet-4-6`.
    pub fn multiplier(&self, model: Option<&str>) -> f64 {
        let Some(model) = model else {
            return 1.0;
        };
        let model = match model.rsplit_once('/') {
            Some((_, family)) => family,
            None => model,
        };
        self.factors
            .iter()
            .filter(|(family, _)| model.starts_with(family.as_str()))
            .max_by_key(|(family, _)| family.len())
            .map_or(1.0, |(_, multiplier)| *multiplier)
    }

    /// Estimate with the multiplier of the request's own model.
    pub fn estimate(&self, body: &[u8]) -> u64 {
        let parsed = serde_json::from_slice::<Value>(body).ok();
        let (model, base) = match &parsed {
            Some(value) => (
                value.get("model").and_then(Value::as_str),
                estimate_value(value),
            ),
            None => (None, text_tokens(&String::from_utf8_lossy(body))),
        };
        scale(base, self.multiplier(model))
    }
}

/// Apply a correction factor, rounding up so a non-1.0 never rounds a count away.
fn scale(tokens: u64, multiplier: f64) -> u64 {
    if multiplier == 1.0 {
        return tokens;
    }
    (tokens as f64 * multiplier).ceil() as u64
}

fn estimate_value(body: &Value) -> u64 {
    let mut tokens = REQUEST_OVERHEAD;
    if let Some(system) = body.get("system") {
        tokens += content_tokens(system);
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            tokens += MESSAGE_OVERHEAD + message_tokens(message);
        }
    }
    if let Some(tools) = body.get("tools") {
        tokens += json_tokens(tools);
    }
    tokens
}

/// Content is a string or an array of blocks; a bare object is one block.
fn content_tokens(content: &Value) -> u64 {
    match content {
        Value::Null => 0,
        Value::String(text) => text_tokens(text),
        Value::Array(blocks) => blocks.iter().map(block_tokens).sum(),
        _ => block_tokens(content),
    }
}

fn message_tokens(message: &Value) -> u64 {
    message.get("content").map_or(0, content_tokens)
}

fn block_tokens(block: &Value) -> u64 {
    let Some(object) = block.as_object() else {
        return match block {
            Value::String(text) => text_tokens(text),
            _ => json_tokens(block),
        };
    };
    match object.get("type").and_then(Value::as_str) {
        Some("text") => string_tokens(object.get("text")),
        Some("thinking") => string_tokens(object.get("thinking")),
        Some("image") => image_tokens(block),
        Some("tool_result") => object.get("content").map_or(0, content_tokens),
        Some("tool_use") => {
            let name = string_tokens(object.get("name"));
            name + object.get("input").map_or(0, json_tokens)
        }
        // Unknown blocks (documents, server tool results) are still worth their bytes.
        _ => json_tokens(block),
    }
}

fn image_tokens(block: &Value) -> u64 {
    let dimensions = block
        .get("width")
        .and_then(Value::as_u64)
        .zip(block.get("height").and_then(Value::as_u64))
        .filter(|(width, height)| *width > 0 && *height > 0)
        .or_else(|| {
            let source = block.get("source")?;
            let width = source.get("width").and_then(Value::as_u64)?;
            let height = source.get("height").and_then(Value::as_u64)?;
            (width > 0 && height > 0).then_some((width, height))
        });
    match dimensions {
        Some((width, height)) => width.saturating_mul(height).div_ceil(PIXELS_PER_TOKEN),
        None => IMAGE_FALLBACK,
    }
}

fn string_tokens(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_str).map_or(0, text_tokens)
}

fn json_tokens(value: &Value) -> u64 {
    text_tokens(&serde_json::to_string(value).expect("JSON values serialize"))
}

/// ceil(chars / 3.5), with the divisor kept integral to avoid float drift.
fn text_tokens(text: &str) -> u64 {
    let chars = text.chars().count() as u64;
    (chars * 2).div_ceil(7)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(body: Value) -> u64 {
        estimate(body.to_string().as_bytes())
    }

    #[test]
    fn text_rounds_up_per_chars() {
        assert_eq!(text_tokens(""), 0);
        assert_eq!(text_tokens("abc"), 1);
        assert_eq!(text_tokens("abcdefg"), 2);
        assert_eq!(text_tokens("abcdefgh"), 3);
        // Multi-byte characters count once each, not once per byte.
        assert_eq!(text_tokens("ééééééé"), 2);
    }

    #[test]
    fn counts_text_system_and_framing() {
        let body = json!({
            "system": "be brief",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello world"}]}],
        });
        // 11 chars -> 4, 8 chars -> 3, plus one message overhead and request overhead.
        assert_eq!(count(body), 4 + 3 + MESSAGE_OVERHEAD + REQUEST_OVERHEAD);
    }

    #[test]
    fn counts_images_by_pixels_or_fallback() {
        let with_dims = json!({
            "messages": [{"role": "user", "content": [{
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": "AAAA",
                           "width": 1500, "height": 1500},
            }]}],
        });
        assert_eq!(
            count(with_dims),
            1500 * 1500 / PIXELS_PER_TOKEN + MESSAGE_OVERHEAD + REQUEST_OVERHEAD
        );

        let unknown_dims = json!({
            "messages": [{"role": "user", "content": [{
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"},
            }]}],
        });
        assert_eq!(
            count(unknown_dims),
            IMAGE_FALLBACK + MESSAGE_OVERHEAD + REQUEST_OVERHEAD
        );
    }

    #[test]
    fn counts_tool_results_and_tool_use() {
        let nested = json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "name": "read", "input": {"path": "/etc/hosts"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "content": [
                        {"type": "text", "text": "127.0.0.1 localhost"}
                    ]}
                ]},
            ],
        });
        let expected = REQUEST_OVERHEAD
            + 2 * MESSAGE_OVERHEAD
            + text_tokens("read")
            + json_tokens(&json!({"path": "/etc/hosts"}))
            + text_tokens("127.0.0.1 localhost");
        assert_eq!(count(nested), expected);
    }

    #[test]
    fn counts_tool_schemas_as_json() {
        let tools = json!([{
            "name": "read",
            "description": "Read a file from disk",
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            },
        }]);
        let body = json!({"messages": [], "tools": tools});
        assert_eq!(count(body), REQUEST_OVERHEAD + json_tokens(&tools));
    }

    #[test]
    fn calibrate_scales_by_longest_family_match() {
        let body = json!({
            "model": "anthropic/claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "hello"}],
        });
        let raw = estimate(body.to_string().as_bytes());

        let mut calibrate = Calibrate::new();
        assert_eq!(calibrate.estimate(body.to_string().as_bytes()), raw);

        calibrate.set("claude", 1.5);
        calibrate.set("claude-sonnet", 2.0);
        assert_eq!(
            calibrate.multiplier(Some("anthropic/claude-sonnet-4-6")),
            2.0
        );
        assert_eq!(calibrate.multiplier(Some("claude-opus-4-8")), 1.5);
        assert_eq!(calibrate.multiplier(Some("gpt-6-luna")), 1.0);
        assert_eq!(calibrate.multiplier(None), 1.0);
        assert_eq!(calibrate.estimate(body.to_string().as_bytes()), raw * 2);
    }

    #[test]
    fn calibrate_ignores_unusable_multipliers() {
        let mut calibrate = Calibrate::new();
        for bad in ["", "x"] {
            calibrate.set(bad, 2.0);
        }
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            calibrate.set("claude", bad);
        }
        assert_eq!(calibrate.multiplier(Some("claude-sonnet-4-6")), 1.0);
    }

    #[test]
    fn non_json_body_falls_back_to_text() {
        assert_eq!(estimate(b"abcdefgh"), 3);
        assert_eq!(estimate(b""), 0);
    }

    #[test]
    fn response_has_the_anthropic_field() {
        assert_eq!(response(42), Bytes::from_static(br#"{"input_tokens":42}"#));
    }
}

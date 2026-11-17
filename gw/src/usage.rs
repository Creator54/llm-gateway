//! Token counts as upstreams report them, for the usage journal.
//!
//! Every reader is passive and fail-open: an unknown shape yields zeros rather
//! than an error, because journaling must never change a reply.

use serde_json::Value;

/// Token counts of one request. `input` excludes cached tokens, matching how
/// Anthropic reports them and how the journal stores them side by side.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub cached_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

impl Tokens {
    /// Keep the larger count per field: streams repeat usage worse, then better.
    pub fn merge(&mut self, other: Self) {
        self.input = self.input.max(other.input);
        self.cached_read = self.cached_read.max(other.cached_read);
        self.cache_write = self.cache_write.max(other.cache_write);
        self.output = self.output.max(other.output);
    }
}

/// Usage from a complete reply body in any of the three protocols.
pub fn body(bytes: &[u8]) -> Tokens {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return Tokens::default();
    };
    collect(&value)
}

/// Usage carried by one SSE `data` payload; `None` when the event has none.
pub fn event(payload: &str) -> Option<Tokens> {
    if payload.trim().is_empty() {
        return None;
    }
    let value = serde_json::from_str::<Value>(payload).ok()?;
    let tokens = collect(&value);
    (tokens != Tokens::default()).then_some(tokens)
}

fn collect(value: &Value) -> Tokens {
    let mut tokens = Tokens::default();
    walk(value, &mut tokens);
    tokens
}

fn walk(value: &Value, out: &mut Tokens) {
    match value {
        Value::Object(map) => {
            if let Some(usage) = map.get("usage") {
                out.merge(read(usage));
            }
            for value in map.values() {
                walk(value, out);
            }
        }
        Value::Array(items) => {
            for value in items {
                walk(value, out);
            }
        }
        _ => {}
    }
}

fn read(usage: &Value) -> Tokens {
    let num = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let nested = |path: &str| usage.pointer(path).and_then(Value::as_u64).unwrap_or(0);
    if usage.get("prompt_tokens").is_some() {
        // OpenAI Chat: prompt_tokens includes the cached and cache-write parts.
        let cached = nested("/prompt_tokens_details/cached_tokens");
        let written = nested("/prompt_tokens_details/cache_write_tokens");
        return Tokens {
            input: num("prompt_tokens").saturating_sub(cached + written),
            cached_read: cached,
            cache_write: written,
            output: num("completion_tokens"),
        };
    }
    let output = num("output_tokens");
    if usage.get("cache_read_input_tokens").is_some() {
        // Anthropic Messages: input_tokens already excludes the cache read.
        return Tokens {
            input: num("input_tokens"),
            cached_read: num("cache_read_input_tokens"),
            cache_write: num("cache_creation_input_tokens"),
            output,
        };
    }
    // OpenAI Responses: input_tokens includes the cached part.
    let cached = nested("/input_tokens_details/cached_tokens");
    Tokens {
        input: num("input_tokens").saturating_sub(cached),
        cached_read: cached,
        cache_write: 0,
        output,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_cache_writes_are_not_counted_as_input() {
        let chat = body(br#"{"usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":40,"cache_write_tokens":50}}}"#);
        assert_eq!(
            (chat.input, chat.cached_read, chat.cache_write),
            (10, 40, 50)
        );
    }

    #[test]
    fn reads_all_three_protocols() {
        let chat = body(br#"{"usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":40}}}"#);
        assert_eq!(
            chat,
            Tokens {
                input: 60,
                cached_read: 40,
                cache_write: 0,
                output: 7
            }
        );
        let messages = body(
            br#"{"usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":5,"output_tokens":7}}"#,
        );
        assert_eq!(
            messages,
            Tokens {
                input: 10,
                cached_read: 90,
                cache_write: 5,
                output: 7
            }
        );
        let responses = body(
            br#"{"response":{"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":40},"output_tokens":7}}}"#,
        );
        assert_eq!(responses, chat);
    }

    #[test]
    fn unknown_shapes_are_zero() {
        assert_eq!(body(b"not json"), Tokens::default());
        assert_eq!(body(br#"{"id":"x"}"#), Tokens::default());
        assert_eq!(event(r#"{"type":"response.created"}"#), None);
        assert_eq!(event("[DONE]"), None);
    }

    #[test]
    fn streamed_usage_keeps_the_largest_count() {
        for payload in [
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":1}}}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":9}}}"#,
        ] {
            assert!(event(payload).is_some());
        }
        let mut out = Tokens::default();
        for payload in [
            r#"{"message":{"usage":{"input_tokens":3,"output_tokens":1}}}"#,
            r#"{"message":{"usage":{"input_tokens":3,"output_tokens":9}}}"#,
        ] {
            out.merge(event(payload).unwrap());
        }
        assert_eq!(out.output, 9);
    }
}

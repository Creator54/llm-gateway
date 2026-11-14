//! Gemini envelope payloads rewritten as Chat completions, plus signature replay.

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const MISSING_WRAPPER: &str = "google-antigravity response missing response wrapper";
pub const TRUNCATED: &str = "google-antigravity turn was cut off mid tool call";
const REPLAY_TTL: Duration = Duration::from_secs(60 * 60);
const REPLAY_MAX_ENTRIES: usize = 10_240;

#[derive(Default)]
pub struct Turn {
    pub text: String,
    pub calls: Vec<(String, String, Value)>,
    pub finish: Option<String>,
    pub usage: Option<Value>,
    tool_calls: u64,
}

/// One streamed frame reduced to Chat deltas.
#[derive(Debug, Default)]
pub struct Delta {
    pub text: String,
    pub calls: Vec<(String, Value)>,
    pub finish: Option<String>,
    pub signatures: Vec<(String, Value, String)>,
}

impl Turn {
    pub fn tool_calls(&self) -> u64 {
        self.tool_calls
    }

    pub fn apply(&mut self, delta: Delta) {
        if !delta.text.is_empty() {
            self.text.push_str(&delta.text);
        }
        for (name, args) in delta.calls {
            self.tool_calls += 1;
            self.calls.push((
                format!("call_{}", &super::util::uuid4()[..8]),
                name,
                args.clone(),
            ));
        }
        if let Some(finish) = delta.finish {
            self.finish = Some(finish);
        }
    }

    pub fn completion(&self) -> Value {
        let mut message = serde_json::Map::new();
        message.insert("role".into(), json!("assistant"));
        message.insert(
            "content".into(),
            if self.text.is_empty() {
                Value::Null
            } else {
                json!(self.text)
            },
        );
        if !self.calls.is_empty() {
            message.insert(
                "tool_calls".into(),
                Value::Array(
                    self.calls
                        .iter()
                        .map(|(id, name, args)| {
                            json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(args).unwrap_or_else(|_| "{}".into()),
                                }
                            })
                        })
                        .collect(),
                ),
            );
        }
        json!({
            "id": format!("chatcmpl-{}", super::util::uuid4()),
            "object": "chat.completion",
            "created": super::util::now_seconds(),
            "choices": [{
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": finish_reason(self.finish.as_deref(), !self.calls.is_empty()),
            }],
            "usage": chat_usage(self.usage.as_ref()),
        })
    }
}

fn finish_reason(finish: Option<&str>, has_calls: bool) -> Value {
    json!(chat_finish(finish, has_calls))
}

pub fn chat_finish(finish: Option<&str>, has_calls: bool) -> &'static str {
    if has_calls {
        return "tool_calls";
    }
    match finish {
        Some("MAX_TOKENS") => "length",
        Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII") => {
            "content_filter"
        }
        _ => "stop",
    }
}

fn count(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(Value::as_u64).unwrap_or(0)
}

pub fn usage_of_chat(usage: &Value) -> Value {
    chat_usage(Some(usage))
}

fn chat_usage(usage: Option<&Value>) -> Value {
    let Some(usage) = usage else {
        return json!({});
    };
    let mut details = serde_json::Map::new();
    if let Some(cached) = usage.get("cachedContentTokenCount").and_then(Value::as_u64) {
        details.insert("cached_tokens".into(), json!(cached));
    }
    let mut out = serde_json::Map::new();
    out.insert(
        "prompt_tokens".into(),
        json!(count(usage, "promptTokenCount")),
    );
    out.insert(
        "completion_tokens".into(),
        json!(count(usage, "candidatesTokenCount")),
    );
    out.insert(
        "total_tokens".into(),
        json!(count(usage, "promptTokenCount") + count(usage, "candidatesTokenCount")),
    );
    if let Some(thoughts) = usage.get("thoughtsTokenCount").and_then(Value::as_u64) {
        details.insert("reasoning_tokens".into(), json!(thoughts));
    }
    if !details.is_empty() {
        out.insert("prompt_tokens_details".into(), Value::Object(details));
    }
    Value::Object(out)
}

/// One Gemini payload; `Ok` is the deltas it carried, `Err` a terminal message.
pub fn translate(payload: &Value, names: &super::util::ToolNames) -> Result<Delta, String> {
    if let Some(error) = payload.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("upstream error");
        return Err(message.to_owned());
    }
    let Some(root) = payload.get("response").filter(|value| value.is_object()) else {
        return Err(MISSING_WRAPPER.to_owned());
    };
    let mut delta = Delta::default();
    let candidates = match root.get("candidates") {
        None | Some(Value::Null) => return Ok(delta),
        Some(Value::Array(candidates)) if candidates.is_empty() => return Ok(delta),
        Some(Value::Array(candidates)) => candidates,
        Some(_) => return Err("google-antigravity candidates is not an array".to_owned()),
    };
    let Some(candidate) = candidates[0].as_object() else {
        return Err("google-antigravity candidate is not an object".to_owned());
    };
    if let Some(finish) = candidate.get("finishReason").and_then(Value::as_str) {
        delta.finish = Some(finish.to_owned());
    }
    let parts = candidate
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array);
    for part in parts.into_iter().flatten() {
        if part.get("thought").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                delta.text.push_str(text);
            }
        }
        let Some(call) = part.get("functionCall") else {
            continue;
        };
        let wire = call.get("name").and_then(Value::as_str).unwrap_or("tool");
        let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
        if let Some(signature) = observe_signature(part) {
            delta
                .signatures
                .push((wire.to_owned(), args.clone(), signature.to_owned()));
        }
        delta.calls.push((names.original(wire), args));
    }
    Ok(delta)
}

/// Usage of one payload, if it carries any.
pub fn usage_of(payload: &Value) -> Option<Value> {
    payload
        .pointer("/response/usageMetadata")
        .filter(|value| value.is_object())
        .cloned()
}

/// Signatures seen in `part.functionCall`, with the replay-hit protection from the spec.
pub fn observe_signature(part: &Value) -> Option<&str> {
    let signature = part.get("thoughtSignature").and_then(Value::as_str)?;
    is_likely_real(signature).then_some(signature)
}

pub fn is_likely_real(signature: &str) -> bool {
    if signature.len() < 16 || signature == "skip_thought_signature_validator" {
        return false;
    }
    const FOREIGN: [&str; 14] = [
        "fc",
        "ctc",
        "tsc",
        "call",
        "msg",
        "rs",
        "resp",
        "reasoning",
        "item",
        "ws",
        "toolu",
        "tool",
        "func",
        "function",
    ];
    let lower = signature.to_ascii_lowercase();
    if FOREIGN.iter().any(|prefix| {
        lower.starts_with(&format!("{prefix}-")) || lower.starts_with(&format!("{prefix}_"))
    }) {
        return false;
    }
    signature
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '_' | '=' | '-'))
}

struct Entry {
    key: String,
    signature: String,
    seen: Instant,
}

fn cache() -> &'static Mutex<VecDeque<Entry>> {
    static CACHE: OnceLock<Mutex<VecDeque<Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn key(model: &str, session: &str, name: &str, args: &Value) -> String {
    format!(
        "{model}\u{0}{session}\u{0}{name}\u{0}{}",
        serde_json::to_string(args).unwrap_or_default()
    )
}

pub fn remember(model: &str, session: &str, seen: &[(String, Value, String)]) {
    let now = Instant::now();
    let Ok(mut cache) = cache().lock() else {
        return;
    };
    cache.retain(|entry| now.duration_since(entry.seen) < REPLAY_TTL);
    for (name, args, signature) in seen {
        let entry_key = key(model, session, name, args);
        cache.retain(|entry| entry.key != entry_key);
        cache.push_back(Entry {
            key: entry_key,
            signature: signature.clone(),
            seen: now,
        });
    }
    while cache.len() > REPLAY_MAX_ENTRIES {
        cache.pop_front();
    }
}

/// Signature previously observed for this exact call in this session.
pub fn lookup(model: &str, session: &str, name: &str, args: &Value) -> Option<String> {
    let now = Instant::now();
    let cache = cache().lock().ok()?;
    let entry_key = key(model, session, name, args);
    cache
        .iter()
        .rev()
        .find(|entry| entry.key == entry_key && now.duration_since(entry.seen) < REPLAY_TTL)
        .map(|entry| entry.signature.clone())
}

/// Only Gemini wire ids accept the validator-bypass sentinel.
fn supports_sentinel(model: &str) -> bool {
    let wire = model.rsplit([':', '/']).next().unwrap_or(model);
    match wire.strip_prefix("gemini") {
        Some(rest) => rest
            .chars()
            .next()
            .is_none_or(|c| c.is_ascii_digit() || c == '-' || c == '.'),
        None => false,
    }
}

/// A model turn whose first functionCall lost its signature gets the validator-bypass
/// sentinel, so CCA does not reject the turn outright.
pub fn signature_fallback(contents: &mut [Value], model: &str) {
    if !supports_sentinel(model) {
        return;
    }
    for turn in contents.iter_mut() {
        if turn.get("role").and_then(Value::as_str) != Some("model") {
            continue;
        }
        let Some(parts) = turn.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        let Some(part) = parts
            .iter_mut()
            .find(|part| part.get("functionCall").is_some())
        else {
            continue;
        };
        if let Some(object) = part.as_object_mut() {
            object
                .entry("thoughtSignature")
                .or_insert_with(|| json!("skip_thought_signature_validator"));
        }
    }
}

/// Applies replay, then the sentinel, then drops the sentinel Claude-on-CCA refuses.
pub fn apply_replay(
    contents: &mut [Value],
    model: &str,
    session: &str,
    claude: bool,
    history: &HashMap<(String, String), String>,
) {
    let mut seen = Vec::new();
    for turn in contents.iter_mut() {
        let model_turn = turn.get("role").and_then(Value::as_str) == Some("model");
        let Some(parts) = turn.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        parts.retain_mut(|part| {
            let Some(call) = part.get("functionCall").cloned() else {
                if !model_turn && part.get("thoughtSignature").is_some() {
                    if let Some(object) = part.as_object_mut() {
                        object.remove("thoughtSignature");
                    }
                }
                return true;
            };
            if !model_turn {
                if let Some(object) = part.as_object_mut() {
                    object.remove("thoughtSignature");
                }
                return true;
            }
            let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            let signature = if claude {
                history.get(&(name.to_owned(), args.to_string())).cloned()
            } else {
                lookup(model, session, name, &args)
            };
            match signature {
                Some(signature) => {
                    seen.push((name.to_owned(), args, signature.clone()));
                    if let Some(object) = part.as_object_mut() {
                        object.insert("thoughtSignature".into(), json!(signature));
                    }
                }
                None => {
                    if let Some(object) = part.as_object_mut() {
                        object.remove("thoughtSignature");
                    }
                }
            }
            true
        });
    }
    if !claude {
        remember(model, session, &seen);
    }
    signature_fallback(contents, model);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::antigravity::util::ToolNames;

    fn frame(parts: Value, finish: Option<&str>) -> Value {
        let mut candidate = json!({ "content": { "parts": parts } });
        if let Some(finish) = finish {
            candidate["finishReason"] = json!(finish);
        }
        json!({ "response": { "candidates": [candidate] } })
    }

    #[test]
    fn translates_text_and_tool_calls() {
        let names = ToolNames::new(["read", "update plan"]);
        let wire_second = names.to_wire("update plan");
        let mut turn = Turn::default();
        let delta = translate(
            &frame(
                json!([
                    { "text": "hi" },
                    { "thought": true, "text": "hidden" },
                    { "functionCall": { "name": "read", "args": { "path": "a" }, "id": "t1" } },
                    { "functionCall": { "name": wire_second, "args": {} } }
                ]),
                Some("STOP"),
            ),
            &names,
        )
        .unwrap();
        assert_eq!(delta.text, "hi");
        assert_eq!(delta.calls.len(), 2);
        assert_eq!(delta.calls[0].0, "read");
        assert_eq!(delta.calls[1].0, "update plan");
        turn.apply(delta);
        let completion = turn.completion();
        assert_eq!(completion["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            completion["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );
        assert!(completion["choices"][0]["message"]["tool_calls"][0]["id"]
            .as_str()
            .unwrap()
            .starts_with("call_"));
    }

    #[test]
    fn rejects_missing_wrapper_and_reports_errors() {
        assert_eq!(
            translate(&json!({ "candidates": [] }), &ToolNames::default())
                .err()
                .unwrap(),
            MISSING_WRAPPER
        );
        assert_eq!(
            translate(
                &json!({ "error": { "message": "boom" } }),
                &ToolNames::default()
            )
            .err()
            .unwrap(),
            "boom"
        );
        assert!(translate(
            &json!({ "response": { "candidates": {} } }),
            &ToolNames::default()
        )
        .is_err());
        assert!(translate(&json!({ "response": {} }), &ToolNames::default()).is_ok());
    }

    #[test]
    fn maps_usage_and_finish() {
        let payload = json!({ "response": {
            "usageMetadata": { "promptTokenCount": 9, "candidatesTokenCount": 4,
                "cachedContentTokenCount": 5, "thoughtsTokenCount": 2 },
            "candidates": [{ "content": { "parts": [{ "text": "x" }] }, "finishReason": "MAX_TOKENS" }]
        }});
        let delta = translate(&payload, &ToolNames::default()).unwrap();
        assert_eq!(delta.finish.as_deref(), Some("MAX_TOKENS"));
        let usage = usage_of_chat(&payload["response"]["usageMetadata"]);
        assert_eq!(usage["prompt_tokens"], 9);
        assert_eq!(usage["completion_tokens"], 4);
        assert_eq!(usage["total_tokens"], 13);
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 5);
        assert_eq!(usage["prompt_tokens_details"]["reasoning_tokens"], 2);

        let turn = Turn {
            finish: Some("MAX_TOKENS".into()),
            ..Default::default()
        };
        assert_eq!(turn.completion()["choices"][0]["finish_reason"], "length");
        let turn = Turn {
            finish: Some("SAFETY".into()),
            ..Default::default()
        };
        assert_eq!(
            turn.completion()["choices"][0]["finish_reason"],
            "content_filter"
        );
    }

    #[test]
    fn filters_foreign_signatures() {
        assert!(is_likely_real("AbCdEfGhIjKlMnOpQrStUvWxYz012345"));
        assert!(!is_likely_real("short"));
        assert!(!is_likely_real("skip_thought_signature_validator"));
        assert!(!is_likely_real("fc_1234567890abcdef"));
        assert!(!is_likely_real("toolu_1234567890abcdef"));
        assert!(!is_likely_real("has spaces 1234567890abcd"));
        assert!(is_likely_real("sig_AbCdEfGhIjKlMnOp"));
    }

    #[test]
    fn replay_round_trips_and_adds_the_sentinel() {
        let model = "test-replay-model";
        let session = "-42";
        let signature = "Rk9PQkFSQkFaUXV1eDEyMzQ1Njc4OTA=";
        let mut body = json!([

                { "role": "user", "parts": [{ "text": "go" }] },
            { "role": "model", "parts": [
                { "functionCall": { "name": "read", "args": { "path": "a" }, "id": "t1" } }
            ]}
        ]);
        remember(
            model,
            session,
            &[("read".into(), json!({ "path": "a" }), signature.into())],
        );
        let mut contents = body.as_array_mut().unwrap().clone();
        apply_replay(&mut contents, model, session, false, &HashMap::new());
        assert_eq!(contents[1]["parts"][0]["thoughtSignature"], signature);
        assert_eq!(contents[0]["parts"][0]["text"], "go");

        let mut fresh = vec![json!({ "role": "model", "parts": [
            { "functionCall": { "name": "write", "args": {}, "id": "t2" } }
        ]})];
        apply_replay(
            &mut fresh,
            "gemini-3.1-pro",
            session,
            false,
            &HashMap::new(),
        );
        assert_eq!(
            fresh[0]["parts"][0]["thoughtSignature"],
            "skip_thought_signature_validator"
        );

        let mut claude = vec![json!({ "role": "model", "parts": [
            { "functionCall": { "name": "write", "args": {}, "id": "t2" } }
        ]})];
        apply_replay(
            &mut claude,
            "claude-sonnet-4-6",
            session,
            true,
            &HashMap::new(),
        );
        assert!(claude[0]["parts"][0].get("thoughtSignature").is_none());
    }
}

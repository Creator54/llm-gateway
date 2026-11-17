//! Whole (non-streamed) Chat replies in Responses or Messages form, and the inverse.

use super::{
    chat_usage, messages_usage, name_call, new_id, now, responses_usage, stop_reason, Error,
    ToolNamespaces,
};
use serde_json::{json, Value};

/// Responses reply as a Chat completion; text and tool calls keep their order.
pub fn responses_reply_to_chat(response: &Value, requested_model: &str) -> Result<Value, Error> {
    let object = response
        .as_object()
        .ok_or_else(|| Error::bad_gateway("upstream response is not an object"))?;
    let mut text = String::new();
    let mut calls = Vec::new();
    for item in object
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        text.push_str(part.get("text").and_then(Value::as_str).unwrap_or_default());
                    }
                }
            }
            Some("function_call") => calls.push(json!({
                "id": item.get("call_id").or_else(|| item.get("id"))
                    .cloned().unwrap_or_else(|| json!(new_id("call"))),
                "type": "function",
                "function": {
                    "name": item.get("name").and_then(Value::as_str).unwrap_or("tool"),
                    "arguments": item.get("arguments").and_then(Value::as_str).unwrap_or_default(),
                },
            })),
            _ => {}
        }
    }
    let finish = if !calls.is_empty() {
        "tool_calls"
    } else if object.get("status").and_then(Value::as_str) == Some("incomplete") {
        "length"
    } else {
        "stop"
    };
    let mut message = json!({
        "role": "assistant",
        "content": if text.is_empty() { Value::Null } else { json!(text) },
    });
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    Ok(json!({
        "id": object.get("id").and_then(Value::as_str).map(str::to_owned)
            .unwrap_or_else(|| new_id("chatcmpl")),
        "object": "chat.completion",
        "created": object.get("created_at").and_then(Value::as_u64).unwrap_or_else(now),
        "model": requested_model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": chat_usage(object.get("usage").unwrap_or(&Value::Null)),
    }))
}

/// Chat reply as a Responses object. `parallel_tool_calls` echoes the request
/// when the client sent one.
pub fn chat_to_responses(
    chat: &Value,
    requested_model: &str,
    request: &Value,
) -> Result<Value, Error> {
    let namespaces = super::tool_namespaces(request);
    let parallel_tool_calls = request.get("parallel_tool_calls").and_then(Value::as_bool);
    let message = chat
        .pointer("/choices/0/message")
        .ok_or_else(|| Error::bad_gateway("upstream response has no choices"))?;
    let calls = tool_calls(message);
    let mut output = Vec::new();
    if let Some(text) = text_of(message) {
        output.push(json!({
            "id": new_id("msg"), "type": "message", "status": "completed", "role": "assistant",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }],
        }));
    }
    for call in &calls {
        output.push(function_call_item(call, "completed", &namespaces));
    }
    let finish = finish_reason(chat);
    let truncated = finish == Some("length");
    let mut response = json!({
        "id": new_id("resp"), "object": "response", "created_at": now(),
        "status": if truncated { "incomplete" } else { "completed" },
        "error": null, "model": requested_model,
        "output": output, "parallel_tool_calls": parallel_tool_calls.unwrap_or(true),
        "usage": responses_usage(&usage_of(chat)),
    });
    if truncated {
        response["incomplete_details"] = json!({ "reason": "max_output_tokens" });
    }
    Ok(response)
}

/// Chat reply as a Messages object.
pub fn chat_to_messages(chat: &Value, requested_model: &str) -> Result<Value, Error> {
    let message = chat
        .pointer("/choices/0/message")
        .ok_or_else(|| Error::bad_gateway("upstream response has no choices"))?;
    let calls = tool_calls(message);
    let mut content = Vec::new();
    if let Some(text) = text_of(message) {
        content.push(json!({ "type": "text", "text": text }));
    }
    for call in &calls {
        content.push(json!({
            "type": "tool_use",
            "id": call_id(call),
            "name": name_of(call),
            "input": input_of(call),
        }));
    }
    Ok(json!({
        "id": new_id("msg"), "type": "message", "role": "assistant",
        "model": requested_model, "content": content,
        "stop_reason": stop_reason(finish_reason(chat), !calls.is_empty()),
        "stop_sequence": null,
        "usage": messages_usage(&usage_of(chat)),
    }))
}

pub(super) fn usage_of(chat: &Value) -> Value {
    chat.get("usage").cloned().unwrap_or_else(|| json!({}))
}

pub(super) fn finish_reason(chat: &Value) -> Option<&str> {
    chat.pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
}

pub(super) fn text_of(message: &Value) -> Option<&str> {
    message
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

pub(super) fn tool_calls(message: &Value) -> Vec<Value> {
    message
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

pub(super) fn call_id(call: &Value) -> Value {
    call.get("id")
        .cloned()
        .unwrap_or_else(|| json!(new_id("call")))
}

pub(super) fn name_of(call: &Value) -> String {
    call.pointer("/function/name")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_owned()
}

/// Tool arguments as an object, since both clients expect one.
pub(super) fn input_of(call: &Value) -> Value {
    call.pointer("/function/arguments")
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn function_call_item(call: &Value, status: &str, namespaces: &ToolNamespaces) -> Value {
    let mut item = json!({
        "id": new_id("fc"), "type": "function_call", "status": status,
        "call_id": call_id(call),
        "arguments": call.pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    });
    name_call(&mut item, &name_of(call), namespaces);
    item
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_text_reply_to_responses() {
        let response = chat_to_responses(
            &json!({
                "choices": [{ "message": { "content": "done" }, "finish_reason": "stop" }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13 }
            }),
            "provider/model",
            &json!({}),
        )
        .unwrap();
        assert_eq!(response["object"], "response");
        assert_eq!(response["status"], "completed");
        assert_eq!(response["model"], "provider/model");
        assert_eq!(response["output"][0]["content"][0]["text"], "done");
        assert_eq!(response["usage"]["input_tokens"], 10);
        assert_eq!(response["usage"]["output_tokens"], 3);
        assert_eq!(response["usage"]["total_tokens"], 13);
        assert_eq!(response["parallel_tool_calls"], true);
        assert!(response.get("incomplete_details").is_none());
    }

    #[test]
    fn maps_cached_tokens_to_responses_details() {
        let response = chat_to_responses(
            &json!({
                "choices": [{ "message": { "content": "done" } }],
                "usage": {
                    "prompt_tokens": 100, "completion_tokens": 5, "total_tokens": 105,
                    "prompt_tokens_details": { "cached_tokens": 80 }
                }
            }),
            "m",
            &json!({}),
        )
        .unwrap();
        assert_eq!(response["usage"]["input_tokens"], 100);
        assert_eq!(
            response["usage"]["input_tokens_details"]["cached_tokens"],
            80
        );
    }

    #[test]
    fn converts_tool_call_to_responses() {
        let response = chat_to_responses(
            &json!({ "choices": [{ "message": { "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": { "name": "read", "arguments": "{\"path\":\"a\"}" }
            }]}, "finish_reason": "tool_calls" }] }),
            "m",
            &json!({}),
        )
        .unwrap();
        let item = &response["output"][0];
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["call_id"], "call_1");
        assert_eq!(item["name"], "read");
        assert_eq!(item["arguments"], "{\"path\":\"a\"}");
    }

    #[test]
    fn converts_reply_to_messages() {
        let message = chat_to_messages(
            &json!({
                "choices": [{ "message": { "content": "done", "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": { "name": "read", "arguments": "{\"path\":\"a\"}" }
                }]}, "finish_reason": "tool_calls" }],
                "usage": {
                    "prompt_tokens": 100, "completion_tokens": 7,
                    "prompt_tokens_details": { "cached_tokens": 60 }
                }
            }),
            "provider/model",
        )
        .unwrap();
        assert_eq!(message["type"], "message");
        assert_eq!(
            message["content"][0],
            json!({ "type": "text", "text": "done" })
        );
        assert_eq!(message["content"][1]["type"], "tool_use");
        assert_eq!(message["content"][1]["id"], "call_1");
        assert_eq!(message["content"][1]["input"], json!({ "path": "a" }));
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(message["usage"]["cache_read_input_tokens"], 60);
        assert_eq!(message["usage"]["input_tokens"], 40);
        assert_eq!(message["usage"]["output_tokens"], 7);
    }

    #[test]
    fn rejects_reply_without_choices() {
        assert_eq!(chat_to_messages(&json!({}), "m").unwrap_err().status, 502);
        assert_eq!(
            chat_to_responses(&json!({}), "m", &json!({}))
                .unwrap_err()
                .status,
            502
        );
    }

    #[test]
    fn maps_length_finish_to_incomplete_response() {
        let response = chat_to_responses(
            &json!({ "choices": [{ "message": { "content": "cut" }, "finish_reason": "length" }] }),
            "m",
            &json!({ "parallel_tool_calls": false }),
        )
        .unwrap();
        assert_eq!(response["status"], "incomplete");
        assert_eq!(
            response["incomplete_details"]["reason"],
            "max_output_tokens"
        );
        assert_eq!(response["parallel_tool_calls"], false);
    }

    #[test]
    fn restores_namespaces_on_flattened_tool_calls() {
        let request = json!({ "tools": [{ "type": "namespace", "name": "multi_agent_v1",
            "tools": [{ "type": "function", "name": "spawn_agent" }] }] });
        let response = chat_to_responses(
            &json!({ "choices": [{ "message": { "tool_calls": [
                { "id": "c1", "function": { "name": "multi_agent_v1__spawn_agent", "arguments": "{}" } },
                { "id": "c2", "function": { "name": "exec_command", "arguments": "{}" } },
            ] }, "finish_reason": "tool_calls" }] }),
            "m",
            &request,
        )
        .unwrap();
        assert_eq!(response["output"][0]["namespace"], "multi_agent_v1");
        assert_eq!(response["output"][0]["name"], "spawn_agent");
        assert!(response["output"][1].get("namespace").is_none());
        assert_eq!(response["output"][1]["name"], "exec_command");
    }
}

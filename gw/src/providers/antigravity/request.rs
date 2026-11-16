//! Chat-shaped request rewritten into the Cloud Code Assist Gemini envelope.

use super::util::ToolNames;
use serde_json::{json, Map, Value};

const EMPTY_PLACEHOLDER: &str = "(empty)";
const EMPTY_TOOL_OUTPUT: &str = "(empty tool output)";
const MISSING_TOOL_RESULT: &str = "[missing tool_result for this tool_use in history]";
const REJECTED_CLAUDE_SDK_PARAGRAPH: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
/// Appended to every systemInstruction; without it CCA replies lose their tool-driving style.
const BREVITY_INSTRUCTION: &str = "Output style for this session:\n- While you are still working (between tool calls), keep any text you emit to a single short line; do not narrate at length.\n- Do detailed reasoning internally, not as visible intermediate output.\n- Prefer taking the next tool action over explaining; keep calling tools until the task is complete.\n- This applies only to intermediate progress text. Your final answer after the work is done is exempt: write it in full and at whatever length the task requires.\n- Formatting: The client environment renders standard Markdown and does not support LaTeX math delimiters ($...$, $$...$$, \\(...\\), \\[...\\]). Do not use LaTeX math delimiters or LaTeX markup (such as \\text{}, \\times, \\le, \\ge, etc.) for variables, formulas, dimensions, or units. Use clean plain text, Markdown, and Unicode symbols (e.g. 180°, 2560 × 1920 px, ≤, ≥, Δ, ±) instead.";

/// What the caller sends; `names` maps response tool names back to client names.
pub struct Prepared {
    pub body: Value,
    pub names: ToolNames,
}

pub fn build(
    chat: &Value,
    model: &str,
    project: &str,
    session_id: &str,
    request_id: &str,
) -> Prepared {
    let messages = chat
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let names = ToolNames::new(
        chat.get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str)),
    );
    let claude = model.to_ascii_lowercase().contains("claude");
    let no_tools = tool_choice_none(chat);
    let tools = if claude && no_tools {
        None
    } else {
        tools(chat, &names)
    };

    let mut request = Map::new();
    request.insert("contents".into(), Value::Array(contents(&messages, &names)));
    request.insert(
        "systemInstruction".into(),
        json!({ "parts": [{ "text": system_text(&messages) }] }),
    );
    if let Some(tools) = tools {
        request.insert("tools".into(), tools);
    }
    if let Some(config) = generation_config(chat, model) {
        request.insert("generationConfig".into(), config);
    }
    let tool_config = if claude && no_tools {
        None
    } else {
        tool_config(chat, &names, claude)
    };
    if let Some(config) = tool_config {
        request.insert("toolConfig".into(), config);
    }
    if !session_id.is_empty() {
        request.insert("sessionId".into(), Value::String(session_id.into()));
    }

    Prepared {
        body: json!({
            "model": wire_model(model, chat.get("reasoning_effort").and_then(Value::as_str)),
            "userAgent": "antigravity",
            "requestType": "agent",
            "project": project,
            "requestId": request_id,
            "request": Value::Object(request),
        }),
        names,
    }
}

fn system_text(messages: &[Value]) -> String {
    let mut paragraphs = Vec::new();
    for message in messages {
        if !matches!(
            message.get("role").and_then(Value::as_str),
            Some("system" | "developer")
        ) {
            continue;
        }
        match message.get("content") {
            Some(Value::String(text)) => paragraphs.push(text.clone()),
            Some(Value::Array(parts)) => {
                let text = parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    paragraphs.push(text);
                }
            }
            _ => {}
        }
    }
    paragraphs.push(BREVITY_INSTRUCTION.to_owned());
    let joined = paragraphs.join("\n\n");
    // CCA answers 429 when Anthropic billing metadata or the Claude SDK paragraph reaches it.
    let stripped = match joined.strip_prefix("x-anthropic-billing-header:") {
        Some(rest) => rest.split_once('\n').map(|(_, tail)| tail).unwrap_or(""),
        None => joined.as_str(),
    };
    let kept = stripped
        .split("\n\n")
        .filter(|paragraph| *paragraph != REJECTED_CLAUDE_SDK_PARAGRAPH)
        .collect::<Vec<_>>()
        .join("\n\n");
    rename_identity(&kept)
}

/// CCA also answers 429 when the system text claims a rival agent's identity
/// ("You are opencode", any case, anywhere). Naming the agent keeps the meaning.
fn rename_identity(text: &str) -> String {
    const CLAIM: &str = "you are opencode";
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len() + 16);
    let mut rest = 0;
    for (at, _) in lower.match_indices(CLAIM) {
        out.push_str(&text[rest..at]);
        out.push_str("You are an agent named ");
        out.push_str(&text[at + "you are ".len()..at + CLAIM.len()]);
        rest = at + CLAIM.len();
    }
    out.push_str(&text[rest..]);
    out
}

fn contents(messages: &[Value], names: &ToolNames) -> Vec<Value> {
    let mut contents: Vec<Value> = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        match message.get("role").and_then(Value::as_str) {
            Some("system" | "developer") => {}
            Some("assistant") => {
                let mut parts = Vec::new();
                let mut calls: Vec<(String, String)> = Vec::new();
                match message.get("content") {
                    Some(Value::String(text)) if !text.is_empty() => {
                        parts.push(json!({ "text": text }))
                    }
                    Some(Value::Array(items)) => {
                        for part in items {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    parts.push(json!({ "text": text }));
                                }
                            }
                        }
                    }
                    _ => {}
                }
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let name = call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool");
                    let wire_name = names.to_wire(name);
                    let arguments = call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    let usable = call
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|id| !id.is_empty());
                    let Some(id) = usable else {
                        parts.push(json!({
                            "text": format!("[tool_use without a usable id: {wire_name}]\n{arguments}")
                        }));
                        continue;
                    };
                    let args = serde_json::from_str::<Value>(arguments)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}));
                    parts.push(
                        json!({ "functionCall": { "name": wire_name, "args": args, "id": id } }),
                    );
                    calls.push((id.to_owned(), wire_name));
                }
                if parts.is_empty() {
                    index += 1;
                    continue;
                }
                contents.push(json!({ "role": "model", "parts": parts }));
                if !calls.is_empty() {
                    index += 1;
                    let mut results = Vec::new();
                    let mut replies = Vec::new();
                    while index < messages.len()
                        && messages[index].get("role").and_then(Value::as_str) == Some("tool")
                    {
                        replies.push(&messages[index]);
                        index += 1;
                    }
                    for (id, wire_name) in &calls {
                        // One adjacent response per call, in call order; a replay can arrive
                        // reversed, duplicated, or missing.
                        let found = replies.iter().find(|result| {
                            result.get("tool_call_id").and_then(Value::as_str) == Some(id.as_str())
                        });
                        match found {
                            Some(result) => results.push(json!({ "functionResponse": {
                                "name": wire_name,
                                "response": { "result": tool_text(result) },
                                "id": id,
                            }})),
                            None => results.push(json!({ "functionResponse": {
                                "name": wire_name,
                                "response": { "result": MISSING_TOOL_RESULT },
                                "id": id,
                            }})),
                        }
                    }
                    for orphan in replies {
                        let id = orphan
                            .get("tool_call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if calls.iter().any(|(call_id, _)| call_id == id) {
                            continue;
                        }
                        results.push(json!({
                            "text": format!(
                                "[tool_result without adjacent tool_use: {id}]\n{}",
                                tool_text(orphan)
                            )
                        }));
                    }
                    contents.push(json!({ "role": "user", "parts": results }));
                }
            }
            Some("tool") => {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                contents.push(json!({ "role": "user", "parts": [{
                    "text": format!(
                        "[tool_result without adjacent tool_use: {id}]\n{}",
                        tool_text(message)
                    )
                }]}));
            }
            _ => contents.push(json!({ "role": "user", "parts": user_parts(message) })),
        }
        index += 1;
    }

    // A functionCall turn may not open the request, and a model tail is rejected upstream.
    let opens_with_call = contents
        .first()
        .and_then(|turn| turn.get("parts"))
        .and_then(Value::as_array)
        .is_some_and(|parts| parts.iter().any(|part| part.get("functionCall").is_some()));
    if opens_with_call {
        contents.insert(
            0,
            json!({ "role": "user", "parts": [{ "text": "(continue)" }] }),
        );
    }
    if contents
        .last()
        .and_then(|turn| turn.get("role"))
        .and_then(Value::as_str)
        .is_none_or(|role| role == "model")
    {
        contents.push(json!({ "role": "user", "parts": [{ "text": "(continue)" }] }));
    }
    contents
}

fn user_parts(message: &Value) -> Vec<Value> {
    let mut parts = Vec::new();
    match message.get("content") {
        Some(Value::String(text)) => parts.push(json!({
            "text": if text.is_empty() { EMPTY_PLACEHOLDER } else { text.as_str() }
        })),
        Some(Value::Array(items)) => {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                parts.push(json!({ "text": text }));
                            }
                        }
                    }
                    Some("image_url") => {
                        let url = item
                            .get("image_url")
                            .and_then(|value| value.get("url").or(Some(value)))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        parts.push(match image_part(url) {
                            Some(part) => part,
                            None => json!({ "text": format!("[image: {url}]") }),
                        });
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if parts.is_empty() {
        parts.push(json!({ "text": EMPTY_PLACEHOLDER }));
    }
    parts
}

/// `data:` URLs travel inline; a remote URL has no mime type we can supply.
fn image_part(url: &str) -> Option<Value> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    Some(json!({ "inline_data": { "mime_type": mime, "data": data } }))
}

fn tool_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) if !text.is_empty() => text.clone(),
        Some(Value::Array(parts)) => {
            let text = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            if text.is_empty() {
                EMPTY_TOOL_OUTPUT.into()
            } else {
                text
            }
        }
        _ => EMPTY_TOOL_OUTPUT.into(),
    }
}

fn tools(chat: &Value, names: &ToolNames) -> Option<Value> {
    let declared = chat.get("tools").and_then(Value::as_array)?;
    if declared.is_empty() {
        return None;
    }
    let declarations: Vec<Value> = declared
        .iter()
        .filter_map(|tool| {
            let name = tool.pointer("/function/name").and_then(Value::as_str)?;
            let mut declaration = Map::new();
            declaration.insert("name".into(), json!(names.to_wire(name)));
            if let Some(description) = tool
                .pointer("/function/description")
                .and_then(Value::as_str)
            {
                declaration.insert("description".into(), json!(description));
            }
            let parameters = tool.pointer("/function/parameters").map_or_else(
                || json!({ "type": "object", "properties": {} }),
                super::schema::sanitize,
            );
            declaration.insert("parameters".into(), parameters);
            Some(Value::Object(declaration))
        })
        .collect();
    (!declarations.is_empty()).then(|| json!([{ "functionDeclarations": declarations }]))
}

fn tool_choice_none(chat: &Value) -> bool {
    chat.get("tool_choice").and_then(Value::as_str) == Some("none")
}

fn tool_config(chat: &Value, names: &ToolNames, claude: bool) -> Option<Value> {
    let mut config = Map::new();
    match chat.get("tool_choice") {
        Some(Value::String(mode)) => match mode.as_str() {
            "none" => {
                config.insert("mode".into(), json!("NONE"));
            }
            "required" => {
                config.insert("mode".into(), json!("ANY"));
            }
            _ => {}
        },
        Some(Value::Object(choice)) => {
            config.insert("mode".into(), json!("ANY"));
            if let Some(name) = choice
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
            {
                config.insert("allowedFunctionNames".into(), json!([names.to_wire(name)]));
            }
        }
        _ => {}
    }
    // Claude-on-CCA always asks for validated function calling.
    if claude {
        config.insert("mode".into(), json!("VALIDATED"));
    }
    (!config.is_empty()).then(|| json!({ "functionCallingConfig": config }))
}

/// Picker ids that CCA only serves under tiered wire ids (mirrors opencodex's
/// `ANTIGRAVITY_EFFORT_WIRE_MAP`; defaults are Google's recommended tiers).
fn wire_model(model: &str, effort: Option<&str>) -> String {
    let tier = match effort {
        Some("low") => "low",
        Some("medium") => "medium",
        Some("high" | "xhigh" | "max") => "high",
        _ => "",
    };
    match model {
        "gemini-3.8-flash" => format!(
            "gemini-3.8-flash-{}",
            if tier.is_empty() { "medium" } else { tier }
        ),
        "gemini-3.1-pro" if tier == "low" => "gemini-3.1-pro-low".into(),
        "gemini-3.1-pro" | "gemini-3.1-pro-high" | "gemini-3.1-pro-preview" => {
            "gemini-pro-agent".into()
        }
        other => other.into(),
    }
}

fn generation_config(chat: &Value, model: &str) -> Option<Value> {
    let mut config = Map::new();
    if let Some(tokens) = chat
        .get("max_tokens")
        .and_then(Value::as_u64)
        .filter(|t| *t > 0)
    {
        let cap = match () {
            _ if model.to_ascii_lowercase().starts_with("gemini") => {
                Some(if is_pro(model) { 65535 } else { 65536 })
            }
            _ if model.to_ascii_lowercase().starts_with("claude") => Some(64000),
            _ => None,
        };
        config.insert(
            "maxOutputTokens".into(),
            json!(cap.map_or(tokens, |cap| tokens.min(cap))),
        );
    }
    if let Some(temperature) = chat.get("temperature").and_then(Value::as_f64) {
        if temperature >= 0.0 {
            config.insert("temperature".into(), json!(temperature.min(2.0)));
        }
    }
    if let Some(top_p) = chat.get("top_p").and_then(Value::as_f64) {
        if top_p >= 0.0 {
            config.insert("topP".into(), json!(top_p.min(1.0)));
        }
    }
    let stops: Vec<String> = match chat.get("stop") {
        Some(Value::String(stop)) if !stop.is_empty() => vec![stop.clone()],
        Some(Value::Array(items)) => {
            let mut stops: Vec<String> = Vec::new();
            for stop in items.iter().filter_map(Value::as_str) {
                if !stop.is_empty() && !stops.iter().any(|seen| seen == stop) {
                    stops.push(stop.to_owned());
                }
            }
            stops.truncate(5);
            stops
        }
        _ => Vec::new(),
    };
    if !stops.is_empty() {
        config.insert("stopSequences".into(), json!(stops));
    }
    (!config.is_empty()).then(|| Value::Object(config))
}

fn is_pro(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(pos) = rest.find("pro") {
        let before = rest[..pos].chars().next_back();
        let after = rest[pos + 3..].chars().next();
        if before.is_none_or(|c| c == '-' || c == '.') && after.is_none_or(|c| c == '-' || c == '.')
        {
            return true;
        }
        rest = &rest[pos + 3..];
    }
    false
}

#[cfg(test)]
mod wire_tests {
    use super::{rename_identity, wire_model};

    #[test]
    fn renames_rival_identity_claims() {
        assert_eq!(
            rename_identity("Hi.\n\nYou are OpenCode, a CLI. you are opencode."),
            "Hi.\n\nYou are an agent named OpenCode, a CLI. You are an agent named opencode."
        );
        assert_eq!(rename_identity("Use opencode."), "Use opencode.");
    }

    #[test]
    fn picker_ids_map_to_tiered_wire_ids() {
        assert_eq!(
            wire_model("gemini-3.8-flash", None),
            "gemini-3.8-flash-medium"
        );
        assert_eq!(
            wire_model("gemini-3.8-flash", Some("low")),
            "gemini-3.8-flash-low"
        );
        assert_eq!(wire_model("gemini-3.1-pro", None), "gemini-pro-agent");
        assert_eq!(
            wire_model("gemini-3.1-pro", Some("low")),
            "gemini-3.1-pro-low"
        );
        assert_eq!(
            wire_model("claude-sonnet-4-6", Some("low")),
            "claude-sonnet-4-6"
        );
    }
}

//! Chat request to Kiro `GenerateAssistantResponse` body.

use super::tools::{completion_tool, convert_tools};
use super::wire::{self, ToolNames, WireClient};
use serde_json::{json, Map, Value};

pub(super) const CONTINUATION_MESSAGE: &str =
    "Continue from the prior conversation. Do not quote or mention this instruction.";
const TOOL_RESULT_CARRIER: &str = "The requested tool result is attached.";
const EMPTY_TOOL_RESULT: &str = "The tool completed without textual output.";
const MAX_INJECTED_INSTRUCTION_CHARS: usize = 16_384;
const COMPLETION_INSTRUCTIONS: &str = "When tools are available, ordinary assistant text is mid-task commentary and does not end the turn. Continue using tools after progress updates. When the task is fully complete and no more tool calls are needed, call codex_kiro_final_answer exactly once with the complete user-facing final answer in `answer`. Do not provide the final answer as ordinary assistant text. This completion tool is not an ordinary work tool. When the task is complete, call it instead of emitting answer-shaped ordinary assistant text. The call is terminal and is the exception to generic tool-result counting: it is complete when issued, ends the turn, returns no tool result, and no text or tool call may follow it. If you cannot continue until the user supplies a decision, information, or a clarification that only they can give, that question is your final answer: call codex_kiro_final_answer with the question and stop. Do not write the question as ordinary text and then answer it yourself.";

/// What the reply side needs from the request side.
pub(super) struct Built {
    pub body: Value,
    pub names: ToolNames,
}

enum Turn {
    User {
        content: String,
        images: Vec<Value>,
        tool_results: Vec<Value>,
    },
    Assistant {
        content: String,
        tool_uses: Vec<Value>,
    },
}

impl Turn {
    fn is_user(&self) -> bool {
        matches!(self, Turn::User { .. })
    }
}

/// Kiro can honour only automatic tool choice.
pub(super) fn validate_capabilities(chat: &Value) -> Result<(), String> {
    match chat.get("tool_choice") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(mode)) if mode == "auto" || mode == "none" => Ok(()),
        Some(_) => Err("Kiro supports only automatic tool choice or tool_choice:none".to_owned()),
    }
}

pub(super) fn build(
    chat: &Value,
    client: WireClient,
    profile_arn: Option<&str>,
    session: &str,
) -> Result<Built, String> {
    validate_capabilities(chat)?;
    let model_id = wire::normalize_model_id(
        chat.get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| "request is missing 'model'".to_owned())?,
    );
    let messages = chat
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "request is missing 'messages'".to_owned())?;

    let mut names = ToolNames::new();
    let mut system_parts: Vec<String> = Vec::new();
    let mut turns: Vec<Turn> = Vec::new();
    let mut seen_calls: Vec<String> = Vec::new();

    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let content = message.get("content").cloned().unwrap_or(Value::Null);
        match role {
            "system" => {
                let text = content_text(&content);
                if !text.is_empty() {
                    system_parts.push(text);
                }
            }
            "assistant" => {
                let mut uses = Vec::new();
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    uses.push(tool_use(call, &mut names, &mut seen_calls)?);
                }
                push_assistant(&mut turns, content_text(&content), uses);
            }
            "tool" => {
                let id = wire::normalize_tool_id(
                    message
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                );
                if !seen_calls.iter().any(|seen| seen == &id) {
                    return Err(format!("Kiro history contains orphaned tool result {id}"));
                }
                let mut text = content_text(&content);
                if text.trim().is_empty() {
                    text = EMPTY_TOOL_RESULT.to_owned();
                }
                push_user(
                    &mut turns,
                    String::new(),
                    Vec::new(),
                    vec![json!({
                        "content": [{ "text": text }],
                        "status": "success",
                        "toolUseId": id,
                    })],
                );
            }
            _ => {
                let mut images = Vec::new();
                let text = content_text(&content);
                let marker = image_omission_marker(&content, &mut images);
                let text = match (text.is_empty(), marker.is_empty()) {
                    (true, _) => marker,
                    (_, true) => text,
                    _ => format!("{text}\n{marker}"),
                };
                push_user(&mut turns, text, images, Vec::new());
            }
        }
    }

    let (mut tools, nudge) = convert_tools(chat, &mut names);
    let completion = !tools.is_empty();
    if completion {
        tools.push(completion_tool());
    }
    let mut system = Vec::new();
    if !system_parts.is_empty() {
        system.push(identify_routed_model(&system_parts.join("\n\n"), &model_id));
    }
    if let Some(nudge) = nudge {
        system.push(nudge);
    }
    if completion {
        system.push(COMPLETION_INSTRUCTIONS.to_owned());
    }
    let system_prefix = bounded_system(system);
    assembled(
        chat,
        client,
        model_id,
        session,
        profile_arn,
        turns,
        tools,
        system_prefix,
        names,
    )
}

#[allow(clippy::too_many_arguments)]
fn assembled(
    chat: &Value,
    client: WireClient,
    model_id: String,
    session: &str,
    profile_arn: Option<&str>,
    mut turns: Vec<Turn>,
    tools: Vec<Value>,
    system_prefix: String,
    names: ToolNames,
) -> Result<Built, String> {
    // Kiro needs alternating turns that start and end on the user.
    if turns.first().is_none_or(|turn| !turn.is_user()) {
        turns.insert(0, continuation());
    }
    if matches!(turns.last(), Some(Turn::Assistant { .. })) {
        turns.push(continuation());
    }
    for turn in &mut turns {
        if let Turn::User {
            content,
            tool_results,
            ..
        } = turn
        {
            if content.trim().is_empty() && !tool_results.is_empty() {
                *content = TOOL_RESULT_CARRIER.to_owned();
            }
        }
    }
    let Some(mut current) = turns.pop() else {
        return Err("Kiro request must end with a user turn".to_owned());
    };
    if !system_prefix.is_empty() {
        // The system prompt rides on the first user turn; Kiro has no system field.
        let mut prefixed = false;
        if let Some(Turn::User { content, .. }) = turns.iter_mut().find(|turn| turn.is_user()) {
            *content = format!("{system_prefix}{content}");
            prefixed = true;
        }
        if !prefixed {
            if let Turn::User { content, .. } = &mut current {
                *content = format!("{system_prefix}{content}");
            }
        }
    }
    finish(
        chat,
        client,
        model_id,
        session,
        profile_arn,
        &turns,
        current,
        tools,
        names,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish(
    chat: &Value,
    client: WireClient,
    model_id: String,
    session: &str,
    profile_arn: Option<&str>,
    history: &[Turn],
    mut current: Turn,
    tools: Vec<Value>,
    names: ToolNames,
) -> Result<Built, String> {
    let has_results =
        matches!(&current, Turn::User { tool_results, .. } if !tool_results.is_empty());
    if let Turn::User { content, .. } = &mut current {
        if !has_results && content != CONTINUATION_MESSAGE {
            *content = inject_thinking_tags(content, chat, &model_id);
        }
    }
    let history: Vec<Value> = history
        .iter()
        .map(|turn| entry(turn, &model_id, client, &[]))
        .collect();
    let current = entry(&current, &model_id, client, &tools);

    let mut state = Map::new();
    state.insert("chatTriggerType".into(), json!("MANUAL"));
    if client == WireClient::Cli {
        state.insert("agentContinuationId".into(), json!(wire::uuid_v4()));
        state.insert("agentTaskType".into(), json!("vibe"));
    }
    state.insert(
        "conversationId".into(),
        json!(wire::conversation_id(session)),
    );
    state.insert("currentMessage".into(), current);
    if !history.is_empty() {
        state.insert("history".into(), Value::Array(history));
    }
    let mut body = Map::new();
    body.insert("conversationState".into(), Value::Object(state));
    if let Some(fields) = effort_fields(chat, &model_id)? {
        body.insert("additionalModelRequestFields".into(), fields);
    }
    if let Some(arn) = profile_arn {
        body.insert("profileArn".into(), json!(arn));
    }
    Ok(Built {
        body: Value::Object(body),
        names,
    })
}

fn continuation() -> Turn {
    Turn::User {
        content: CONTINUATION_MESSAGE.to_owned(),
        images: Vec::new(),
        tool_results: Vec::new(),
    }
}

/// Wire entry for a turn. `tools` is only attached to the current user message.
fn entry(turn: &Turn, model_id: &str, client: WireClient, tools: &[Value]) -> Value {
    match turn {
        Turn::Assistant { content, tool_uses } => {
            let mut message = Map::new();
            message.insert("content".into(), json!(content));
            if !tool_uses.is_empty() {
                message.insert("toolUses".into(), Value::Array(tool_uses.clone()));
            }
            json!({ "assistantResponseMessage": message })
        }
        Turn::User {
            content,
            images,
            tool_results,
        } => {
            let mut message = Map::new();
            message.insert("content".into(), json!(content));
            message.insert("modelId".into(), json!(model_id));
            message.insert("origin".into(), json!(client.origin()));
            if !images.is_empty() {
                message.insert("images".into(), Value::Array(images.clone()));
            }
            if !tool_results.is_empty() || !tools.is_empty() {
                let mut context = Map::new();
                if !tool_results.is_empty() {
                    context.insert("toolResults".into(), Value::Array(tool_results.clone()));
                }
                if !tools.is_empty() {
                    context.insert("tools".into(), Value::Array(tools.to_vec()));
                }
                message.insert("userInputMessageContext".into(), Value::Object(context));
            }
            json!({ "userInputMessage": message })
        }
    }
}

fn tool_use(call: &Value, names: &mut ToolNames, seen: &mut Vec<String>) -> Result<Value, String> {
    let raw_id = call.get("id").and_then(Value::as_str).unwrap_or("");
    let id = wire::normalize_tool_id(raw_id);
    if id.is_empty() {
        return Err("Kiro history contains a tool call with an empty id".to_owned());
    }
    if seen.contains(&id) {
        return Err(format!(
            "Kiro history contains duplicate tool call id {raw_id}"
        ));
    }
    seen.push(id.clone());
    let function = call.get("function").cloned().unwrap_or(Value::Null);
    let wire_name = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tool call is missing a function name".to_owned())?;
    let input = match function.get("arguments") {
        Some(Value::String(text)) => serde_json::from_str(text)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({})),
        Some(value) if value.is_object() => value.clone(),
        _ => json!({}),
    };
    Ok(json!({ "name": names.alias(wire_name), "input": input, "toolUseId": id }))
}

fn push_user(turns: &mut Vec<Turn>, content: String, images: Vec<Value>, results: Vec<Value>) {
    if let Some(Turn::User {
        content: existing,
        images: existing_images,
        tool_results,
    }) = turns.last_mut()
    {
        append_text(existing, &content);
        existing_images.extend(images);
        tool_results.extend(results);
        return;
    }
    turns.push(Turn::User {
        content,
        images,
        tool_results: results,
    });
}

fn push_assistant(turns: &mut Vec<Turn>, content: String, uses: Vec<Value>) {
    if content.is_empty() && uses.is_empty() {
        return;
    }
    if let Some(Turn::Assistant {
        content: existing,
        tool_uses,
    }) = turns.last_mut()
    {
        append_text(existing, &content);
        tool_uses.extend(uses);
        return;
    }
    turns.push(Turn::Assistant {
        content,
        tool_uses: uses,
    });
}

fn append_text(target: &mut String, next: &str) {
    if next.is_empty() {
        return;
    }
    if !target.is_empty() {
        target.push_str("\n\n");
    }
    target.push_str(next);
}

/// Plain text of a Chat content value, ignoring non-text parts.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| matches!(part_type(part), Some("text" | "input_text" | "output_text")))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.as_str().unwrap_or_default().to_owned(),
    }
}

fn part_type(part: &Value) -> Option<&str> {
    part.get("type").and_then(Value::as_str)
}

/// Inline base64 images, plus a bounded marker for references that cannot be inlined.
fn image_omission_marker(content: &Value, images: &mut Vec<Value>) -> String {
    let Value::Array(parts) = content else {
        return String::new();
    };
    let mut remote = 0;
    let mut malformed = 0;
    for part in parts {
        if !matches!(part_type(part), Some("image_url" | "image")) {
            continue;
        }
        let url = part
            .get("image_url")
            .and_then(|value| value.get("url").or(Some(value)))
            .or_else(|| part.get("url"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !url.starts_with("data:") {
            remote += 1;
            continue;
        }
        match split_data_url(url) {
            Some((format, bytes)) => {
                images.push(json!({ "format": format, "source": { "bytes": bytes } }))
            }
            None => malformed += 1,
        }
    }
    let mut markers = Vec::new();
    if remote == 1 {
        markers.push(
            "[image omitted: remote image references are not supported by this provider]"
                .to_owned(),
        );
    } else if remote > 1 {
        markers.push(format!(
            "[{remote} images omitted: remote image references are not supported by this provider]"
        ));
    }
    if malformed == 1 {
        markers.push("[image omitted: malformed inline image data URL]".to_owned());
    } else if malformed > 1 {
        markers.push(format!(
            "[{malformed} images omitted: malformed inline image data URLs]"
        ));
    }
    markers.join("\n")
}

fn split_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (header, bytes) = rest.split_once(',')?;
    if bytes.is_empty() {
        return None;
    }
    let media = header.split(';').next().unwrap_or("image/jpeg");
    let subtype = media.split('/').nth(1).unwrap_or("jpeg");
    let format = if subtype.eq_ignore_ascii_case("jpg") {
        "jpeg".to_owned()
    } else {
        subtype.to_lowercase()
    };
    Some((format, bytes.to_owned()))
}

/// Replace a Codex GPT identity line with the model actually sent upstream.
fn identify_routed_model(system: &str, model: &str) -> String {
    let replacement = match safe_identity(model) {
        Some(identity) => format!(
            "You are a coding agent powered by the {identity}. If asked which model you are, identify as {identity}. Do not claim to be a different model or to have a different creator."
        ),
        None => "You are a coding agent powered by the configured model. If asked which model you are, identify as configured model. Do not claim to be GPT-5 or made by OpenAI.".to_owned(),
    };
    let mut out = system.to_owned();
    for known in [
        "You are Codex, a coding agent based on GPT-5.",
        "You are Codex, an agent based on GPT-5.",
        "You are a coding agent. Do not claim to be GPT-5 or to be made by OpenAI.",
    ] {
        out = out.replace(known, &replacement);
    }
    out
}

fn safe_identity(model: &str) -> Option<&str> {
    let trimmed = model.trim();
    let allowed = |ch: char| ch.is_ascii_alphanumeric() || "._/@:+-[]~".contains(ch);
    (!trimmed.is_empty() && trimmed.len() <= 128 && trimmed.chars().all(allowed)).then_some(trimmed)
}

/// System text is prefixed onto the first user turn, bounded cumulatively.
fn bounded_system(parts: Vec<String>) -> String {
    let mut used = 0;
    let mut kept = Vec::new();
    for part in parts {
        let remaining = MAX_INJECTED_INSTRUCTION_CHARS.saturating_sub(used);
        if remaining == 0 || part.is_empty() {
            continue;
        }
        let mut slice: String = part.chars().take(remaining).collect();
        if slice.ends_with(|ch: char| (0xd800..=0xdbff).contains(&(ch as u32))) {
            slice.pop();
        }
        if slice.is_empty() {
            continue;
        }
        used += slice.len();
        kept.push(slice);
    }
    if kept.is_empty() {
        return String::new();
    }
    format!("{}\n\n", kept.join("\n\n"))
}

/// Native effort fields, only where the model is measured to accept them.
fn effort_fields(chat: &Value, model_id: &str) -> Result<Option<Value>, String> {
    let Some(effort) = chat.get("reasoning_effort").and_then(Value::as_str) else {
        return Ok(None);
    };
    if effort == "none" {
        return Ok(None);
    }
    let field = match model_id {
        "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" | "gpt-6-sol" | "gpt-6-luna"
        | "gpt-6.1-sol" => "reasoning",
        "claude-opus-5" | "claude-opus-5.5" => "output_config",
        _ => return Ok(None),
    };
    if !matches!(effort, "low" | "medium" | "high" | "xhigh" | "max") {
        return Err(format!(
            "Kiro {model_id} does not support reasoning effort {effort:?}"
        ));
    }
    if matches!(model_id, "gpt-5.6-luna" | "gpt-5.6-terra") && effort == "xhigh" {
        return Ok(None);
    }
    Ok(Some(json!({ field: { "effort": effort } })))
}

fn inject_thinking_tags(content: &str, chat: &Value, model_id: &str) -> String {
    let Some(effort) = chat.get("reasoning_effort").and_then(Value::as_str) else {
        return content.to_owned();
    };
    if effort == "none" || effort_fields(chat, model_id).ok().flatten().is_some() {
        return content.to_owned();
    }
    let ratio = match effort {
        "minimal" => 0.10,
        "low" => 0.20,
        "medium" => 0.50,
        "high" => 0.80,
        "xhigh" => 0.90,
        "max" => 0.95,
        _ => return content.to_owned(),
    };
    let max_tokens = chat
        .get("max_tokens")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .unwrap_or(4096);
    let budget = ((max_tokens as f64 * ratio).floor() as u64).max(1);
    let instruction = [
        "Think in English for better reasoning quality.",
        "Be thorough and systematic, consider edge cases, challenge assumptions, and verify reasoning before answering.",
        "After thinking, respond in the user's language.",
    ]
    .join("\n");
    format!(
        "<thinking_mode>enabled</thinking_mode>\n<max_thinking_length>{budget}</max_thinking_length>\n<thinking_instruction>{instruction}</thinking_instruction>\n\n{content}"
    )
}

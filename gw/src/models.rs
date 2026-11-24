//! Local answers for /v1/models.
//!
//! The gateway publishes its own list instead of asking upstream: one request per
//! client, no account needed, and the ids are exactly the ones the router accepts.

use bytes::Bytes;
use http::HeaderMap;
use serde_json::{json, Value};
use std::collections::HashSet;

/// Clients only check that `created` is present.
const CREATED: u64 = 0;
/// Fixed timestamp for Anthropic-flavor rows.
const CREATED_AT: &str = "2026-01-01T00:00:00Z";

/// One routable model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Public id exactly as a client sends it: `provider/model`, `combo/name`, or `auto`.
    /// A pinned account keeps the `provider@account/model` form.
    pub id: String,
    /// Route namespace; served as `owned_by` in the OpenAI list.
    pub provider: String,
    /// Context window in tokens, 0 when unknown.
    pub context_window: u32,
    /// Maximum output tokens, 0 when unknown.
    pub max_output: u32,
}

/// Which list schema the client expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    OpenAi,
    Anthropic,
}

/// Anthropic clients (Claude Code, Claude Desktop) send `anthropic-version`; everyone
/// else gets the OpenAI list.
pub fn shape(headers: &HeaderMap) -> Shape {
    if headers.contains_key("anthropic-version") {
        Shape::Anthropic
    } else {
        Shape::OpenAi
    }
}

/// Render the list body for `shape`.
pub fn render(entries: &[Entry], shape: Shape) -> Bytes {
    let body = match shape {
        Shape::OpenAi => openai(entries),
        Shape::Anthropic => anthropic(entries),
    };
    Bytes::from(serde_json::to_vec(&body).expect("model list is plain JSON"))
}

fn openai(entries: &[Entry]) -> Value {
    let data: Vec<Value> = entries
        .iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "object": "model",
                "created": CREATED,
                "owned_by": entry.provider,
            })
        })
        .collect();
    json!({ "object": "list", "data": data })
}

fn anthropic(entries: &[Entry]) -> Value {
    let data: Vec<Value> = entries
        .iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "type": "model",
                "display_name": display_name(&entry.id),
                "created_at": CREATED_AT,
                // The official ModelInfo shape; null is honest when we do not know.
                "max_input_tokens": positive(entry.context_window),
                "max_tokens": positive(entry.max_output),
            })
        })
        .collect();
    json!({
        "data": data,
        "has_more": false,
        "first_id": entries.first().map(|entry| entry.id.as_str()),
        "last_id": entries.last().map(|entry| entry.id.as_str()),
    })
}

/// `provider/model` -> `model`; the namespace is noise in a picker.
fn display_name(id: &str) -> &str {
    id.split_once('/').map_or(id, |(_, model)| model)
}

fn positive(value: u32) -> Option<u32> {
    (value > 0).then_some(value)
}

/// Entry for the `auto` route, which always exists because rules can answer it.
pub fn auto_entry() -> Entry {
    Entry {
        id: "auto".to_owned(),
        provider: "auto".to_owned(),
        context_window: 0,
        max_output: 0,
    }
}

/// Entry for `combo/<name>`.
pub fn combo_entry(name: &str) -> Entry {
    Entry {
        id: format!("combo/{name}"),
        provider: "combo".to_owned(),
        context_window: 0,
        max_output: 0,
    }
}

/// Seed entries plus `auto` and the configured combos, deduplicated by id.
///
/// `auto` leads so pickers show it first; a seeded row wins over a generated one.
pub fn with_builtins(entries: impl IntoIterator<Item = Entry>, combos: &[String]) -> Vec<Entry> {
    let mut all = vec![auto_entry()];
    all.extend(entries);
    all.extend(combos.iter().map(|name| combo_entry(name)));
    let mut seen = HashSet::new();
    all.retain(|entry| seen.insert(entry.id.clone()));
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn entry(id: &str, provider: &str) -> Entry {
        Entry {
            id: id.to_owned(),
            provider: provider.to_owned(),
            context_window: 200_000,
            max_output: 8_192,
        }
    }

    fn rendered(shape: Shape) -> Value {
        let entries = with_builtins(
            [entry("openai/gpt-6-luna", "openai")],
            &["pi-subagents".to_owned()],
        );
        serde_json::from_slice(&render(&entries, shape)).unwrap()
    }

    #[test]
    fn openai_shape_is_exact() {
        let body = rendered(Shape::OpenAi);
        assert_eq!(body["object"], "list");
        let data = body["data"].as_array().unwrap();
        assert_eq!(data.len(), 3);
        assert_eq!(
            data[0],
            json!({"id": "auto", "object": "model", "created": 0, "owned_by": "auto"})
        );
        assert_eq!(
            data[1],
            json!({"id": "openai/gpt-6-luna", "object": "model", "created": 0, "owned_by": "openai"})
        );
        assert_eq!(
            data[2],
            json!({"id": "combo/pi-subagents", "object": "model", "created": 0, "owned_by": "combo"})
        );
    }

    #[test]
    fn anthropic_shape_is_exact() {
        let entries = vec![entry("openai/gpt-6-luna", "openai")];
        let body: Value = serde_json::from_slice(&render(&entries, Shape::Anthropic)).unwrap();
        assert_eq!(
            body,
            json!({
                "data": [{
                    "id": "openai/gpt-6-luna",
                    "type": "model",
                    "display_name": "gpt-6-luna",
                    "created_at": "2026-01-01T00:00:00Z",
                    "max_input_tokens": 200_000,
                    "max_tokens": 8_192,
                }],
                "has_more": false,
                "first_id": "openai/gpt-6-luna",
                "last_id": "openai/gpt-6-luna",
            })
        );
    }

    #[test]
    fn anthropic_empty_list_has_null_bounds() {
        let body: Value = serde_json::from_slice(&render(&[], Shape::Anthropic)).unwrap();
        assert_eq!(body["data"], json!([]));
        assert_eq!(body["has_more"], json!(false));
        assert!(body["first_id"].is_null());
        assert!(body["last_id"].is_null());
    }

    #[test]
    fn unknown_capacity_is_null() {
        let bare = Entry {
            id: "combo/x".to_owned(),
            provider: "combo".to_owned(),
            context_window: 0,
            max_output: 0,
        };
        let body: Value = serde_json::from_slice(&render(&[bare], Shape::Anthropic)).unwrap();
        assert!(body["data"][0]["max_input_tokens"].is_null());
        assert!(body["data"][0]["max_tokens"].is_null());
    }

    #[test]
    fn shape_comes_from_the_header() {
        let mut headers = HeaderMap::new();
        assert_eq!(shape(&headers), Shape::OpenAi);
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        assert_eq!(shape(&headers), Shape::Anthropic);
    }

    #[test]
    fn builtins_include_auto_and_combos_without_duplicates() {
        let combos = vec!["a".to_owned(), "b".to_owned()];
        let entries = with_builtins([auto_entry(), combo_entry("a")], &combos);
        let ids: Vec<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids, ["auto", "combo/a", "combo/b"]);
    }
}

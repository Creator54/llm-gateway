//! Cache-friendly request edits: markers, stable prefixes, warming.
//!
//! Every edit here is deterministic and fail-open: a body that does not parse,
//! or does not look like the expected protocol, comes back byte-identical.

use serde_json::{Map, Value};

/// Anthropic allows at most four cache breakpoints on a request.
const MAX_BREAKPOINTS: usize = 4;

/// OpenAI rejects `prompt_cache_key` values longer than this.
const MAX_CACHE_KEY: usize = 64;

const SESSION_START: &str = "<session-overview>";
const SESSION_END: &str = "</session-overview>";

/// Which wire protocol a body belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// Anthropic `/v1/messages`.
    Anthropic,
    /// OpenAI `/v1/chat/completions` or `/v1/responses`.
    OpenAi,
}

/// One switch per edit; `Options::parity()` turns all of them off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Add `cache_control` breakpoints when the client sent none.
    pub markers: bool,
    /// Repair breakpoint TTL order.
    pub ttl_order: bool,
    /// Use the 1h TTL on breakpoints this module adds.
    pub ttl_1h: bool,
    /// Strip per-turn churn from a pi `<session-overview>` block.
    pub strip_volatile: bool,
    /// Add `prompt_cache_key` to OpenAI bodies.
    pub openai_cache_key: bool,
}

impl Default for Options {
    /// Everything that is safe without extra provider support is on.
    fn default() -> Self {
        Self {
            markers: true,
            ttl_order: true,
            ttl_1h: false,
            strip_volatile: true,
            openai_cache_key: true,
        }
    }
}

impl Options {
    /// No edits at all; bodies must come back byte-identical.
    pub fn parity() -> Self {
        Self {
            markers: false,
            ttl_order: false,
            ttl_1h: false,
            strip_volatile: false,
            openai_cache_key: false,
        }
    }
}

/// A block that can carry `cache_control`, in the order Anthropic reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spot {
    /// Index into `tools`.
    Tool(usize),
    /// Index into a block-list `system`.
    System(usize),
    /// `system` sent as a plain string.
    SystemStr,
    /// Message index, then content-block index.
    Block(usize, usize),
}

/// Byte-level entry point; returns the input untouched on a parse miss or when no edit applied.
pub fn rewrite(
    body: &[u8],
    family: Family,
    opts: &Options,
    session: &str,
    supports: impl Fn(&str) -> Option<bool>,
) -> Vec<u8> {
    let Ok(original) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let edited = match family {
        // Strip first so the churn never becomes part of a prefix we then mark.
        Family::Anthropic => anthropic_messages(strip_volatile(original.clone(), opts), opts),
        Family::OpenAi => openai_cache_key(original.clone(), session, opts, supports),
    };
    if edited == original {
        return body.to_vec();
    }
    serde_json::to_vec(&edited).unwrap_or_else(|_| body.to_vec())
}

/// Add missing breakpoints and repair TTL order on an Anthropic Messages body.
pub fn anthropic_messages(mut body: Value, opts: &Options) -> Value {
    if !body.is_object() {
        return body;
    }
    if opts.markers {
        add_markers(&mut body, opts.ttl_1h);
    }
    if opts.ttl_order {
        repair_ttl_order(&mut body);
    }
    body
}

/// Remove known per-turn churn, but never before the last cache breakpoint.
pub fn strip_volatile(mut body: Value, opts: &Options) -> Value {
    if !opts.strip_volatile || !body.is_object() {
        return body;
    }
    let spots = walk(&body);
    let last = spots
        .iter()
        .rposition(|spot| control(&body, *spot).is_some());
    // No breakpoint means nothing is cached yet, so nothing is protected.
    let from = last.map_or(0, |i| i + 1);
    for spot in &spots[from..] {
        strip_churn(&mut body, *spot);
    }
    body
}

/// Set `prompt_cache_key` to `session` unless it is already there or `supports` says no.
pub fn openai_cache_key(
    mut body: Value,
    session: &str,
    opts: &Options,
    supports: impl Fn(&str) -> Option<bool>,
) -> Value {
    let session = session.trim();
    if !opts.openai_cache_key || session.is_empty() || !body.is_object() {
        return body;
    }
    if body.get("prompt_cache_key").is_some() || body.get("promptCacheKey").is_some() {
        return body;
    }
    let model = body.get("model").and_then(Value::as_str).unwrap_or("");
    if supports(model) == Some(false) {
        return body;
    }
    let key: String = session.chars().take(MAX_CACHE_KEY).collect();
    if let Some(obj) = body.as_object_mut() {
        obj.insert("prompt_cache_key".into(), Value::String(key));
    }
    body
}

/// Hash of what a provider can serve from cache: tools, system, messages up to the last breakpoint.
pub fn prefix_hash(body: &Value) -> u64 {
    let spots = walk(body);
    let mut last_bp = None;
    let mut last_msg = None;
    for (i, spot) in spots.iter().enumerate() {
        if control(body, *spot).is_none() {
            continue;
        }
        last_bp = Some(i);
        if let Spot::Block(m, _) = *spot {
            last_msg = Some(m.max(last_msg.unwrap_or(0)));
        }
    }
    if last_bp.is_none() {
        return 0;
    }
    let mut prefix = Vec::with_capacity(3);
    prefix.push(body.get("tools").cloned().unwrap_or(Value::Null));
    prefix.push(body.get("system").cloned().unwrap_or(Value::Null));
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .map(|m| match last_msg {
            Some(last) => m[..=last.min(m.len().saturating_sub(1))].to_vec(),
            None => Vec::new(),
        })
        .unwrap_or_default();
    prefix.push(Value::Array(messages));
    match serde_json::to_vec(&Value::Array(prefix)) {
        Ok(bytes) => fnv1a(&bytes),
        Err(_) => 0,
    }
}

/// Whether the cached prefix changed; `0` from either side means nothing was cached.
pub fn cache_break(prev: u64, next: u64) -> bool {
    prev != 0 && next != 0 && prev != next
}

/// All cache-breakpoint candidates in wire order: tools, then system, then messages.
fn walk(body: &Value) -> Vec<Spot> {
    let mut spots = Vec::new();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        spots.extend((0..tools.len()).map(Spot::Tool));
    }
    match body.get("system") {
        Some(Value::Array(blocks)) => spots.extend((0..blocks.len()).map(Spot::System)),
        Some(Value::String(_)) => spots.push(Spot::SystemStr),
        _ => {}
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for (m, msg) in messages.iter().enumerate() {
            if let Some(content) = msg.get("content").and_then(Value::as_array) {
                spots.extend((0..content.len()).map(|b| Spot::Block(m, b)));
            }
        }
    }
    spots
}

fn block(body: &Value, spot: Spot) -> Option<&Value> {
    match spot {
        Spot::Tool(i) => body.get("tools")?.get(i),
        Spot::System(i) => body.get("system")?.get(i),
        Spot::SystemStr => body.get("system"),
        Spot::Block(m, b) => body.get("messages")?.get(m)?.get("content")?.get(b),
    }
}

fn block_mut(body: &mut Value, spot: Spot) -> Option<&mut Value> {
    match spot {
        Spot::Tool(i) => body.get_mut("tools")?.get_mut(i),
        Spot::System(i) => body.get_mut("system")?.get_mut(i),
        Spot::SystemStr => body.get_mut("system"),
        Spot::Block(m, b) => body
            .get_mut("messages")?
            .get_mut(m)?
            .get_mut("content")?
            .get_mut(b),
    }
}

/// The `cache_control` object on a block, if it has one.
fn control(body: &Value, spot: Spot) -> Option<&Map<String, Value>> {
    block(body, spot)?.get("cache_control")?.as_object()
}

fn control_mut(body: &mut Value, spot: Spot) -> Option<&mut Map<String, Value>> {
    block_mut(body, spot)?
        .get_mut("cache_control")?
        .as_object_mut()
}

fn is_ephemeral(control: &Map<String, Value>) -> bool {
    control.get("type").and_then(Value::as_str) == Some("ephemeral")
}

fn add_markers(body: &mut Value, ttl_1h: bool) -> bool {
    let spots = walk(body);
    if spots.iter().any(|spot| control(body, *spot).is_some()) {
        return false;
    }
    let targets = marker_spots(body);
    let mut changed = false;
    for spot in targets {
        changed |= set_control(body, spot, ttl_1h);
    }
    changed
}

/// Last system block, last tool, then the last two user turns; four at most.
fn marker_spots(body: &Value) -> Vec<Spot> {
    let mut spots = Vec::new();
    if let Some(blocks) = body.get("system").and_then(Value::as_array) {
        if let Some(i) = blocks.iter().rposition(cacheable) {
            spots.push(Spot::System(i));
        }
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        if let Some(i) = tools.iter().rposition(Value::is_object) {
            spots.push(Spot::Tool(i));
        }
    }
    let mut users = 0;
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for (m, msg) in messages.iter().enumerate().rev() {
            if users == 2 || spots.len() == MAX_BREAKPOINTS {
                break;
            }
            if msg.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            let last = msg
                .get("content")
                .and_then(Value::as_array)
                .and_then(|c| c.iter().rposition(cacheable));
            if let Some(b) = last {
                spots.push(Spot::Block(m, b));
                users += 1;
            }
        }
    }
    spots
}

/// Block types Anthropic accepts `cache_control` on; anything else is left alone.
fn cacheable(block: &Value) -> bool {
    if !block.is_object() {
        return false;
    }
    match block.get("type").and_then(Value::as_str) {
        None => true,
        Some(t) => matches!(
            t,
            "text"
                | "image"
                | "document"
                | "tool_use"
                | "tool_result"
                | "server_tool_use"
                | "web_search_tool_result"
        ),
    }
}

fn set_control(body: &mut Value, spot: Spot, ttl_1h: bool) -> bool {
    let Some(obj) = block_mut(body, spot).and_then(Value::as_object_mut) else {
        return false;
    };
    let mut cc = Map::new();
    cc.insert("type".into(), Value::String("ephemeral".into()));
    if ttl_1h {
        cc.insert("ttl".into(), Value::String("1h".into()));
    }
    obj.insert("cache_control".into(), Value::Object(cc));
    true
}

/// Anthropic requires every 1h breakpoint to come before every 5m one, so any
/// short breakpoint preceding a long one is upgraded. Unknown TTLs are left alone.
fn repair_ttl_order(body: &mut Value) -> bool {
    let spots = walk(body);
    let mut long: Vec<usize> = Vec::new();
    let mut short: Vec<usize> = Vec::new();
    for (i, spot) in spots.iter().enumerate() {
        let Some(cc) = control(body, *spot) else {
            continue;
        };
        if !is_ephemeral(cc) {
            continue;
        }
        match cc.get("ttl").and_then(Value::as_str) {
            Some("1h") => long.push(i),
            None | Some("5m") => short.push(i),
            Some(_) => return false,
        }
    }
    let Some(&last_long) = long.last() else {
        return false;
    };
    let mut changed = false;
    for i in short.into_iter().filter(|i| *i < last_long) {
        if let Some(cc) = control_mut(body, spots[i]) {
            cc.insert("ttl".into(), Value::String("1h".into()));
            changed = true;
        }
    }
    changed
}

/// Rewrite one block's text if it holds a pi session-overview with churn fields.
fn strip_churn(body: &mut Value, spot: Spot) -> bool {
    let text = match spot {
        Spot::SystemStr => match body.get_mut("system") {
            Some(Value::String(s)) => s,
            _ => return false,
        },
        _ => match block_mut(body, spot).and_then(|b| b.get_mut("text")) {
            Some(Value::String(s)) => s,
            _ => return false,
        },
    };
    match strip_session_overview(text) {
        Some(cleaned) => {
            *text = cleaned;
            true
        }
        None => false,
    }
}

/// Drop RECENT COMMITS, `Working directory:` and `Line count:` from the first
/// session-overview block. Those three change every turn; the rest is stable.
fn strip_session_overview(text: &str) -> Option<String> {
    let start = text.find(SESSION_START)?;
    let inner_start = start + SESSION_START.len();
    let inner_end = inner_start + text[inner_start..].find(SESSION_END)?;
    let inner = &text[inner_start..inner_end];
    let cleaned = strip_lines(
        &strip_lines(&strip_recent_commits(inner), "Working directory:"),
        "Line count:",
    );
    if cleaned == inner {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..inner_start]);
    out.push_str(&cleaned);
    out.push_str(&text[inner_end..]);
    Some(out)
}

/// Remove the first `## RECENT COMMITS` section, up to the next `## ` heading.
fn strip_recent_commits(inner: &str) -> String {
    const HEADING: &str = "\n## RECENT COMMITS\n";
    let Some(start) = inner.find(HEADING) else {
        return inner.to_string();
    };
    let after = start + HEADING.len();
    let end = inner[after..]
        .find("\n## ")
        .map_or(inner.len(), |i| after + i);
    let mut out = String::with_capacity(inner.len());
    out.push_str(&inner[..start]);
    out.push_str(&inner[end..]);
    out
}

/// Remove every line whose text starts with `field`, keeping its newline.
fn strip_lines(text: &str, field: &str) -> String {
    let needle = format!("\n{field}");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(&needle) {
        out.push_str(&rest[..i]);
        match rest[i + needle.len()..].find('\n') {
            Some(j) => rest = &rest[i + needle.len() + j..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> Value {
        serde_json::from_value(v).unwrap()
    }

    fn breakpoints(body: &Value) -> Vec<Spot> {
        walk(body)
            .into_iter()
            .filter(|spot| control(body, *spot).is_some())
            .collect()
    }

    fn user_turn(text: &str) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": text}]})
    }

    fn sample() -> Value {
        parse(json!({
            "model": "claude",
            "system": [
                {"type": "text", "text": "sys one"},
                {"type": "text", "text": "sys two"}
            ],
            "tools": [{"name": "read"}, {"name": "bash"}],
            "messages": [
                user_turn("u1"),
                {"role": "assistant", "content": [{"type": "text", "text": "a1"}]},
                user_turn("u2"),
                user_turn("u3")
            ]
        }))
    }

    #[test]
    fn parity_leaves_bytes_identical() {
        let opts = Options::parity();
        let body = serde_json::to_vec_pretty(&sample()).unwrap();
        let out = rewrite(&body, Family::Anthropic, &opts, "sess", |_| None);
        assert_eq!(out, body);
        let out = rewrite(&body, Family::OpenAi, &opts, "sess", |_| None);
        assert_eq!(out, body);
    }

    #[test]
    fn garbage_and_unknown_shapes_are_fail_open() {
        let opts = Options::default();
        for body in [
            b"not json".to_vec(),
            b"{".to_vec(),
            b"[]".to_vec(),
            b"\"hi\"".to_vec(),
            b"".to_vec(),
        ] {
            for family in [Family::Anthropic, Family::OpenAi] {
                assert_eq!(rewrite(&body, family, &opts, "sess", |_| None), body);
            }
        }
        // An Anthropic body with no system, tools or messages gets no markers.
        let body = serde_json::to_vec(&json!({"input": "x"})).unwrap();
        assert_eq!(
            rewrite(&body, Family::Anthropic, &opts, "sess", |_| None),
            body
        );
    }

    #[test]
    fn adds_four_breakpoints_on_last_system_tool_and_two_user_turns() {
        let out = anthropic_messages(sample(), &Options::default());
        assert_eq!(
            breakpoints(&out),
            vec![
                Spot::Tool(1),
                Spot::System(1),
                Spot::Block(2, 0),
                Spot::Block(3, 0)
            ]
        );
        assert_eq!(
            block(&out, Spot::Tool(1)).unwrap()["cache_control"]["type"],
            "ephemeral"
        );
        assert!(block(&out, Spot::Tool(1)).unwrap()["cache_control"]
            .get("ttl")
            .is_none());
    }

    #[test]
    fn ttl_1h_marks_added_breakpoints() {
        let opts = Options {
            ttl_1h: true,
            ..Options::default()
        };
        let out = anthropic_messages(sample(), &opts);
        for spot in breakpoints(&out) {
            assert_eq!(block(&out, spot).unwrap()["cache_control"]["ttl"], "1h");
        }
    }

    #[test]
    fn existing_breakpoints_are_kept_and_not_extended() {
        let mut body = sample();
        body["messages"][0]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
        let out = anthropic_messages(body.clone(), &Options::default());
        assert_eq!(out, body);
        assert_eq!(breakpoints(&out), vec![Spot::Block(0, 0)]);
    }

    #[test]
    fn never_exceeds_the_anthropic_limit() {
        let mut body = sample();
        body["messages"] = json!([
            user_turn("u1"),
            user_turn("u2"),
            user_turn("u3"),
            user_turn("u4")
        ]);
        let out = anthropic_messages(body, &Options::default());
        assert!(breakpoints(&out).len() <= MAX_BREAKPOINTS);
    }

    #[test]
    fn markers_are_idempotent() {
        let opts = Options::default();
        let once = anthropic_messages(sample(), &opts);
        let twice = anthropic_messages(once.clone(), &opts);
        assert_eq!(once, twice);
    }

    #[test]
    fn skips_string_content_and_thinking_blocks() {
        let body = parse(json!({
            "system": "plain string",
            "messages": [
                {"role": "user", "content": "string content"},
                {"role": "user", "content": [{"type": "thinking", "thinking": "x"}]}
            ]
        }));
        let out = anthropic_messages(body, &Options::default());
        assert!(breakpoints(&out).is_empty());
    }

    #[test]
    fn repairs_one_hour_breakpoint_after_a_short_one() {
        let mut body = sample();
        body["tools"][0]["cache_control"] = json!({"type": "ephemeral"});
        body["messages"][0]["content"][0]["cache_control"] =
            json!({"type": "ephemeral", "ttl": "1h"});
        let out = anthropic_messages(body, &Options::default());
        assert_eq!(
            block(&out, Spot::Tool(0)).unwrap()["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
        assert_eq!(
            block(&out, Spot::Block(0, 0)).unwrap()["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
    }

    #[test]
    fn ttl_order_flag_off_leaves_order_alone() {
        let mut body = sample();
        body["tools"][0]["cache_control"] = json!({"type": "ephemeral"});
        body["messages"][0]["content"][0]["cache_control"] =
            json!({"type": "ephemeral", "ttl": "1h"});
        let opts = Options {
            ttl_order: false,
            ..Options::default()
        };
        assert_eq!(anthropic_messages(body.clone(), &opts), body);
    }

    #[test]
    fn unknown_ttl_is_not_rewritten() {
        let mut body = sample();
        body["tools"][0]["cache_control"] = json!({"type": "ephemeral", "ttl": "30m"});
        body["messages"][0]["content"][0]["cache_control"] =
            json!({"type": "ephemeral", "ttl": "1h"});
        assert_eq!(anthropic_messages(body.clone(), &Options::default()), body);
    }

    #[test]
    fn strips_churn_from_session_overview() {
        let overview = "<session-overview>\n## CURRENT TASK\nfix it\n\n## RECENT COMMITS\nabc123 one\ndef456 two\n## PACKAGES\npkg\nWorking directory: Clean\nLine count: 7 / 2000\n</session-overview>";
        let mut body = parse(json!({"system": [{"type": "text", "text": overview}]}));
        body["messages"] = json!([user_turn("hi")]);
        let out = strip_volatile(body, &Options::default());
        let text = block(&out, Spot::System(0)).unwrap()["text"]
            .as_str()
            .unwrap();
        assert_eq!(
            text,
            "<session-overview>\n## CURRENT TASK\nfix it\n\n## PACKAGES\npkg\n</session-overview>"
        );
    }

    #[test]
    fn strip_matches_the_pi_cache_optimizer_rules() {
        let cases = [
            (
                "<session-overview>\n## CURRENT TASK\nfix it\n\n## RECENT COMMITS\nabc123 one\ndef456 two\n## PACKAGES\npkg\nWorking directory: Clean\nLine count: 7 / 2000\n</session-overview>",
                "<session-overview>\n## CURRENT TASK\nfix it\n\n## PACKAGES\npkg\n</session-overview>",
            ),
            (
                "<session-overview>\n## RECENT COMMITS\nonly commit\n</session-overview>",
                "<session-overview></session-overview>",
            ),
            (
                "<session-overview>\nWorking directory: Clean\nWorking directory: 2 dirty\nLine count: 1 / 2000\nLine count: 2 / 2000\n</session-overview>",
                "<session-overview>\n</session-overview>",
            ),
            (
                "<session-overview>\nLine count: 9 / 2000</session-overview>",
                "<session-overview></session-overview>",
            ),
            (
                "<session-overview>\n## RECENT COMMITS\n\n## NEXT\nx\n</session-overview>",
                "<session-overview>\n## NEXT\nx\n</session-overview>",
            ),
            ("no tags at all\nLine count: 1 / 2000", "no tags at all\nLine count: 1 / 2000"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                strip_session_overview(input).unwrap_or_else(|| input.to_string()),
                expected,
                "{input:?}"
            );
        }
    }

    #[test]
    fn strip_is_a_noop_outside_the_block_and_when_unchanged() {
        let body = parse(json!({
            "system": [{"type": "text", "text": "Line count: 7 / 2000"}],
            "messages": [user_turn("<session-overview>\n## PACKAGES\npkg\n</session-overview>")]
        }));
        assert_eq!(strip_volatile(body.clone(), &Options::default()), body);
    }

    #[test]
    fn strip_never_touches_blocks_before_the_last_breakpoint() {
        let overview = "<session-overview>\nLine count: 7 / 2000\n</session-overview>";
        let mut body = parse(json!({
            "system": [
                {"type": "text", "text": overview, "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": overview}
            ],
            "messages": [user_turn("hi")]
        }));
        let out = strip_volatile(body.clone(), &Options::default());
        assert_eq!(block(&out, Spot::System(0)).unwrap()["text"], overview);
        assert_eq!(
            block(&out, Spot::System(1)).unwrap()["text"],
            "<session-overview>\n</session-overview>"
        );

        // A breakpoint on the churn block itself freezes everything before it.
        body["system"][1]["cache_control"] = json!({"type": "ephemeral"});
        assert_eq!(strip_volatile(body.clone(), &Options::default()), body);
    }

    #[test]
    fn strips_when_no_breakpoint_exists() {
        let mut body = parse(json!({"messages": [user_turn("x")]}));
        body["system"] = json!("<session-overview>\nLine count: 1 / 2000\n</session-overview>");
        let out = strip_volatile(body, &Options::default());
        assert_eq!(out["system"], "<session-overview>\n</session-overview>");
    }

    #[test]
    fn sets_prompt_cache_key_only_when_supported() {
        let body = parse(json!({"model": "gpt", "messages": []}));
        let opts = Options::default();
        let out = openai_cache_key(body.clone(), " session-1 ", &opts, |_| None);
        assert_eq!(out["prompt_cache_key"], "session-1");
        let out = openai_cache_key(body.clone(), "s", &opts, |_| Some(false));
        assert_eq!(out, body);
        let out = openai_cache_key(body.clone(), "s", &opts, |_| Some(true));
        assert_eq!(out["prompt_cache_key"], "s");
        assert_eq!(openai_cache_key(body.clone(), "", &opts, |_| None), body);
    }

    #[test]
    fn keeps_an_existing_prompt_cache_key_and_clamps_long_ones() {
        let opts = Options::default();
        let body = parse(json!({"prompt_cache_key": "client-key"}));
        assert_eq!(openai_cache_key(body.clone(), "s", &opts, |_| None), body);
        let long = "x".repeat(200);
        let out = openai_cache_key(parse(json!({})), &long, &opts, |_| None);
        assert_eq!(
            out["prompt_cache_key"].as_str().unwrap().len(),
            MAX_CACHE_KEY
        );
    }

    #[test]
    fn prefix_hash_covers_only_the_cached_prefix() {
        let mut body = sample();
        body["messages"][2]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
        let base = prefix_hash(&body);

        // Anything after the breakpoint is not part of the cached prefix.
        let mut after = body.clone();
        after["messages"][3]["content"][0]["text"] = json!("changed churn");
        assert_eq!(prefix_hash(&after), base);

        // Anything before it is.
        let mut before = body.clone();
        before["system"][0]["text"] = json!("sys one edited");
        assert_ne!(prefix_hash(&before), base);
        let mut tools = body.clone();
        tools["tools"][0]["name"] = json!("glob");
        assert_ne!(prefix_hash(&tools), base);

        // No breakpoint means nothing cached.
        assert_eq!(prefix_hash(&sample()), 0);
    }

    #[test]
    fn cache_break_only_fires_between_two_cached_prefixes() {
        assert!(cache_break(1, 2));
        assert!(!cache_break(1, 1));
        assert!(!cache_break(0, 2));
        assert!(!cache_break(1, 0));
        assert!(!cache_break(0, 0));
    }

    #[test]
    fn rewrite_applies_strip_then_markers() {
        let body = serde_json::to_vec(&json!({
            "system": [{"type": "text", "text":
                "<session-overview>\nLine count: 3 / 2000\n</session-overview>"}],
            "messages": [user_turn("hi")]
        }))
        .unwrap();
        let out = rewrite(&body, Family::Anthropic, &Options::default(), "", |_| None);
        let parsed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            parsed["system"][0]["text"],
            "<session-overview>\n</session-overview>"
        );
        assert_eq!(
            breakpoints(&parsed),
            vec![Spot::System(0), Spot::Block(0, 0)]
        );
        assert_ne!(out, body);
    }

    #[test]
    fn rewrite_sets_the_openai_key() {
        let body = serde_json::to_vec(&json!({"model": "gpt", "messages": []})).unwrap();
        let out = rewrite(&body, Family::OpenAi, &Options::default(), "sess", |_| {
            Some(true)
        });
        let parsed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["prompt_cache_key"], "sess");
    }
}

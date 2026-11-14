//! Tool-result cleanup after the last cache breakpoint.
//!
//! Only the mutable tail of a request is rewritten: everything up to and
//! including the last Anthropic `cache_control` marker, or the last two
//! user/tool turns of an OpenAI-style request, is left byte-identical. Edits
//! are surgical string replacements in the original bytes, so untouched
//! formatting and key order survive. Anything unparsable comes back unchanged.

use bytes::Bytes;
use std::borrow::Cow;

/// Requests above this size skip cleanup: the span tree costs roughly ten
/// times the body, and a proxy holding many 32 MiB bodies must stay small.
const MAX_BODY: usize = 8 << 20;
const MAX_DEPTH: usize = 128;

/// Which cleanups run. `parity()` disables all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Remove ANSI/VT escape sequences (colors, cursor moves).
    pub strip_ansi: bool,
    /// Keep only the final frame of `\r` progress-bar redraws per line.
    pub collapse_redraws: bool,
    /// Replace runs of identical consecutive lines with `line (repeated N times)`.
    pub collapse_repeats: bool,
    /// Drop trailing spaces and tabs on each line.
    pub trim_trailing: bool,
    /// Keep at most this many lines, head and tail, marking the gap.
    pub max_lines: Option<usize>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            strip_ansi: true,
            collapse_redraws: true,
            collapse_repeats: true,
            trim_trailing: true,
            max_lines: None,
        }
    }
}

impl Options {
    /// Every transform off, for byte-for-byte parity comparisons.
    pub fn parity() -> Self {
        Self {
            strip_ansi: false,
            collapse_redraws: false,
            collapse_repeats: false,
            trim_trailing: false,
            max_lines: None,
        }
    }

    fn enabled(&self) -> bool {
        self.strip_ansi
            || self.collapse_redraws
            || self.collapse_repeats
            || self.trim_trailing
            || self.max_lines.is_some()
    }
}

/// Clean tool output inside a request body. Returns `body` itself when the
/// request cannot be parsed, holds nothing to clean, or nothing changed.
pub fn tidy(body: Bytes, opts: &Options) -> Bytes {
    if !opts.enabled() || body.len() > MAX_BODY {
        return body;
    }
    match rewrite(&body, opts) {
        Some(cleaned) => Bytes::from(cleaned),
        None => body,
    }
}

fn rewrite(body: &[u8], opts: &Options) -> Option<Vec<u8>> {
    let root = Parser { bytes: body, at: 0 }.parse()?;
    let mut edits = Vec::new();
    if let Some(messages) = field(&root, b"messages").and_then(seq) {
        if messages.iter().any(|m| {
            content_blocks(m)
                .iter()
                .any(|b| tag_is(b, b"type", b"tool_result"))
        }) {
            anthropic(messages, opts, &mut edits);
        } else {
            chat(messages, opts, &mut edits);
        }
    } else if let Some(input) = field(&root, b"input").and_then(seq) {
        responses(input, opts, &mut edits);
    }
    apply(body, &edits)
}

/// Anthropic Messages: clean `tool_result` blocks after the last message-level
/// `cache_control`. Markers on `system` or `tools` sit before every message,
/// so they leave the whole message list mutable.
fn anthropic(messages: &[Node], opts: &Options, edits: &mut Vec<Edit>) {
    let start = messages
        .iter()
        .rposition(|m| has_key(m, b"cache_control"))
        .map_or(0, |i| i + 1);
    for message in &messages[start..] {
        for block in content_blocks(message) {
            if !tag_is(block, b"type", b"tool_result") {
                continue;
            }
            if let Some(content) = field(block, b"content") {
                collect_content(content, opts, edits);
            }
        }
    }
}

/// Chat: OpenAI caching follows the longest stable prefix, approximated here by
/// the last two user/tool turns, which are the only ones rewritten.
fn chat(messages: &[Node], opts: &Options, edits: &mut Vec<Edit>) {
    let turns: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| tag_is(m, b"role", b"user") || tag_is(m, b"role", b"tool"))
        .map(|(i, _)| i)
        .collect();
    let start = if turns.len() >= 2 {
        turns[turns.len() - 2]
    } else {
        0
    };
    for message in &messages[start..] {
        if !tag_is(message, b"role", b"tool") {
            continue;
        }
        if let Some(content) = field(message, b"content") {
            collect_content(content, opts, edits);
        }
    }
}

/// Responses: same two-turn tail, cleaning `function_call_output` items.
fn responses(input: &[Node], opts: &Options, edits: &mut Vec<Edit>) {
    let turns: Vec<usize> = input
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            tag_is(item, b"type", b"function_call_output") || tag_is(item, b"role", b"user")
        })
        .map(|(i, _)| i)
        .collect();
    let start = if turns.len() >= 2 {
        turns[turns.len() - 2]
    } else {
        0
    };
    for item in &input[start..] {
        if !tag_is(item, b"type", b"function_call_output") {
            continue;
        }
        for name in [b"output".as_slice(), b"content".as_slice()] {
            if let Some(value) = field(item, name) {
                collect_content(value, opts, edits);
            }
        }
    }
}

fn content_blocks<'a, 'n>(message: &'n Node<'a>) -> &'n [Node<'a>] {
    field(message, b"content").and_then(seq).unwrap_or(&[])
}

fn collect_content(node: &Node, opts: &Options, edits: &mut Vec<Edit>) {
    match node {
        Node::Str(_) => collect_text(node, opts, edits),
        Node::Array(parts) => {
            for part in parts {
                if let Some(text) = field(part, b"text") {
                    collect_text(text, opts, edits);
                }
            }
        }
        _ => {}
    }
}

type Edit = (usize, usize, String);

fn collect_text(node: &Node, opts: &Options, edits: &mut Vec<Edit>) {
    let Node::Str(s) = node else { return };
    let Some(text) = unescape(s.raw) else { return };
    let Some(clean) = clean(&text, opts) else {
        return;
    };
    edits.push((s.start, s.end, escape(&clean)));
}

fn apply(body: &[u8], edits: &[Edit]) -> Option<Vec<u8>> {
    if edits.is_empty() {
        return None;
    }
    let mut ordered: Vec<&Edit> = edits.iter().collect();
    ordered.sort_by_key(|(start, _, _)| *start);
    let mut out = Vec::with_capacity(body.len());
    let mut at = 0;
    for (start, end, text) in ordered {
        if *start < at || end < start {
            return None;
        }
        out.extend_from_slice(&body[at..*start]);
        out.extend_from_slice(text.as_bytes());
        at = *end;
    }
    out.extend_from_slice(&body[at..]);
    (out != body).then_some(out)
}

fn clean(text: &str, opts: &Options) -> Option<String> {
    let mut cur = Cow::Borrowed(text);
    let mut changed = false;
    if opts.strip_ansi {
        if let Some(next) = strip_ansi(&cur) {
            cur = Cow::Owned(next);
            changed = true;
        }
    }
    if opts.collapse_redraws {
        if let Some(next) = collapse_redraws(&cur) {
            cur = Cow::Owned(next);
            changed = true;
        }
    }
    if opts.trim_trailing {
        if let Some(next) = trim_trailing(&cur) {
            cur = Cow::Owned(next);
            changed = true;
        }
    }
    if opts.collapse_repeats {
        if let Some(next) = collapse_repeats(&cur) {
            cur = Cow::Owned(next);
            changed = true;
        }
    }
    if let Some(max) = opts.max_lines {
        if let Some(next) = cap_lines(&cur, max) {
            cur = Cow::Owned(next);
            changed = true;
        }
    }
    changed.then(|| cur.into_owned())
}

fn strip_ansi(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    if !bytes.contains(&0x1b) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        match bytes.get(i).copied() {
            None => {}
            Some(b'[') => {
                // CSI: parameters, then a final byte in 0x40..=0x7e.
                i += 1;
                while let Some(&b) = bytes.get(i) {
                    i += 1;
                    if (0x40..=0x7e).contains(&b) {
                        break;
                    }
                }
            }
            Some(b']' | b'P' | b'^' | b'_') => {
                // OSC/DCS/PM/APC run to BEL or ST.
                i += 1;
                while let Some(&b) = bytes.get(i) {
                    if b == 0x07 {
                        i += 1;
                        break;
                    }
                    if b == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            Some(b) if (0x20..=0x2f).contains(&b) => {
                // Two-byte escape such as ESC ( B: intermediate, then final.
                i += 1;
                if let Some(&f) = bytes.get(i) {
                    if (0x30..=0x7e).contains(&f) {
                        i += 1;
                    }
                }
            }
            Some(_) => i += 1,
        }
    }
    // Escape sequences are ASCII, so the remainder is still valid UTF-8.
    String::from_utf8(out).ok()
}

fn collapse_redraws(text: &str) -> Option<String> {
    if !text.contains('\r') {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, ending) = split_ending(line);
        let kept = match body.rfind('\r') {
            Some(i) => &body[i + 1..],
            None => body,
        };
        out.push_str(kept);
        out.push_str(ending);
    }
    (out != text).then_some(out)
}

fn trim_trailing(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, ending) = split_ending(line);
        out.push_str(body.trim_end_matches([' ', '\t']));
        out.push_str(ending);
    }
    (out != text).then_some(out)
}

fn collapse_repeats(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut changed = false;
    while i < lines.len() {
        let mut end = i + 1;
        while end < lines.len() && lines[end] == lines[i] {
            end += 1;
        }
        let run = end - i;
        if run > 1 {
            let (body, ending) = split_ending(lines[i]);
            out.push_str(body);
            out.push_str(" (repeated ");
            out.push_str(&run.to_string());
            out.push_str(" times)");
            out.push_str(ending);
            changed = true;
        } else {
            out.push_str(lines[i]);
        }
        i = end;
    }
    changed.then_some(out)
}

fn cap_lines(text: &str, max: usize) -> Option<String> {
    let max = max.max(3);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.len() <= max {
        return None;
    }
    let head = max / 2;
    let tail = max - head;
    let omitted = lines.len() - head - tail;
    let mut out = String::with_capacity(text.len());
    for line in &lines[..head] {
        out.push_str(line);
    }
    out.push_str("[... ");
    out.push_str(&omitted.to_string());
    out.push_str(" lines omitted ...]\n");
    for line in &lines[lines.len() - tail..] {
        out.push_str(line);
    }
    Some(out)
}

fn split_ending(line: &str) -> (&str, &str) {
    match line.strip_suffix('\n') {
        Some(rest) => match rest.strip_suffix('\r') {
            Some(body) => (body, "\r\n"),
            None => (rest, "\n"),
        },
        None => (line, ""),
    }
}

fn unescape(raw: &[u8]) -> Option<Cow<'_, str>> {
    let text = std::str::from_utf8(raw).ok()?;
    if !text.contains('\\') {
        return Some(Cow::Borrowed(text));
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let high = hex4(&mut chars)?;
                let code = if (0xD800..0xDC00).contains(&high) {
                    if chars.next()? != '\\' || chars.next()? != 'u' {
                        return None;
                    }
                    let low = hex4(&mut chars)?;
                    if !(0xDC00..0xE000).contains(&low) {
                        return None;
                    }
                    0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                } else {
                    high
                };
                out.push(char::from_u32(code)?);
            }
            _ => return None,
        }
    }
    Some(Cow::Owned(out))
}

fn hex4(chars: &mut std::str::Chars) -> Option<u32> {
    let mut value = 0u32;
    for _ in 0..4 {
        value = value * 16 + chars.next()?.to_digit(16)?;
    }
    Some(value)
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// A JSON value with the byte ranges of its strings, so edits can be applied
/// to the original bytes instead of a re-serialization.
enum Node<'a> {
    Scalar,
    Str(Str<'a>),
    Array(Vec<Node<'a>>),
    Object(Vec<(&'a [u8], Node<'a>)>),
}

struct Str<'a> {
    /// String body without quotes, still escaped.
    raw: &'a [u8],
    start: usize,
    end: usize,
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Parser<'a> {
    fn parse(mut self) -> Option<Node<'a>> {
        self.ws();
        let node = self.value(0)?;
        self.ws();
        (self.at == self.bytes.len()).then_some(node)
    }

    fn ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.at) {
            if !matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                break;
            }
            self.at += 1;
        }
    }

    fn value(&mut self, depth: usize) -> Option<Node<'a>> {
        if depth > MAX_DEPTH {
            return None;
        }
        match *self.bytes.get(self.at)? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(Node::Str),
            b't' => self.literal(b"true"),
            b'f' => self.literal(b"false"),
            b'n' => self.literal(b"null"),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn literal(&mut self, word: &[u8]) -> Option<Node<'a>> {
        if !self.bytes.get(self.at..)?.starts_with(word) {
            return None;
        }
        self.at += word.len();
        Some(Node::Scalar)
    }

    fn number(&mut self) -> Option<Node<'a>> {
        let start = self.at;
        while let Some(&b) = self.bytes.get(self.at) {
            if !matches!(b, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                break;
            }
            self.at += 1;
        }
        (self.at > start).then_some(Node::Scalar)
    }

    fn string(&mut self) -> Option<Str<'a>> {
        if self.bytes.get(self.at) != Some(&b'"') {
            return None;
        }
        let start = self.at + 1;
        let mut i = start;
        loop {
            match *self.bytes.get(i)? {
                b'"' => break,
                b'\\' => i += 2,
                _ => i += 1,
            }
        }
        let raw = self.bytes.get(start..i)?;
        self.at = i + 1;
        Some(Str { raw, start, end: i })
    }

    fn object(&mut self, depth: usize) -> Option<Node<'a>> {
        self.at += 1;
        let mut pairs = Vec::new();
        self.ws();
        if self.bytes.get(self.at) == Some(&b'}') {
            self.at += 1;
            return Some(Node::Object(pairs));
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            if self.bytes.get(self.at) != Some(&b':') {
                return None;
            }
            self.at += 1;
            self.ws();
            pairs.push((key.raw, self.value(depth + 1)?));
            self.ws();
            match *self.bytes.get(self.at)? {
                b',' => self.at += 1,
                b'}' => {
                    self.at += 1;
                    return Some(Node::Object(pairs));
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<Node<'a>> {
        self.at += 1;
        let mut items = Vec::new();
        self.ws();
        if self.bytes.get(self.at) == Some(&b']') {
            self.at += 1;
            return Some(Node::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value(depth + 1)?);
            self.ws();
            match *self.bytes.get(self.at)? {
                b',' => self.at += 1,
                b']' => {
                    self.at += 1;
                    return Some(Node::Array(items));
                }
                _ => return None,
            }
        }
    }
}

// Keys and short marker values are compared unescaped: an escaped spelling
// only makes cleanup miss, which is harmless.
fn field<'a, 'n>(node: &'n Node<'a>, name: &[u8]) -> Option<&'n Node<'a>> {
    match node {
        Node::Object(pairs) => pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn seq<'a, 'n>(node: &'n Node<'a>) -> Option<&'n [Node<'a>]> {
    match node {
        Node::Array(items) => Some(items),
        _ => None,
    }
}

fn tag<'a>(node: &Node<'a>, name: &[u8]) -> Option<&'a [u8]> {
    match field(node, name)? {
        Node::Str(s) => Some(s.raw),
        _ => None,
    }
}

fn tag_is(node: &Node, name: &[u8], value: &[u8]) -> bool {
    tag(node, name) == Some(value)
}

fn has_key(node: &Node<'_>, name: &[u8]) -> bool {
    match node {
        Node::Object(pairs) => pairs
            .iter()
            .any(|(key, value)| *key == name || has_key(value, name)),
        Node::Array(items) => items.iter().any(|item| has_key(item, name)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn run(body: &str, opts: &Options) -> String {
        let out = tidy(Bytes::copy_from_slice(body.as_bytes()), opts);
        String::from_utf8(out.to_vec()).unwrap()
    }

    fn parsed(body: &str, opts: &Options) -> Value {
        serde_json::from_str(&run(body, opts)).unwrap()
    }

    fn tool_result(content: Value) -> Value {
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": content}]})
    }

    #[test]
    fn clean_output_is_byte_identical() {
        let body = r#"{"model":"m","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"plain\nlines\n"}]}]}"#;
        let out = tidy(Bytes::from_static(body.as_bytes()), &Options::default());
        assert_eq!(out.as_ref(), body.as_bytes());
    }

    #[test]
    fn body_without_tool_output_is_byte_identical() {
        let body = r#"{"model" : "m", "messages": [{"role": "user", "content": "hi"}]}"#;
        let out = tidy(Bytes::from_static(body.as_bytes()), &Options::default());
        assert_eq!(out.as_ref(), body.as_bytes());
    }

    #[test]
    fn invalid_json_is_byte_identical() {
        let body = r#"{"messages":[{"role":"tool","content":"\u001b[31m""#;
        let out = tidy(Bytes::from_static(body.as_bytes()), &Options::default());
        assert_eq!(out.as_ref(), body.as_bytes());
    }

    #[test]
    fn parity_options_never_change_bytes() {
        let body = json!({
            "model": "m",
            "messages": [tool_result(json!("\u{1b}[31mred\u{1b}[0m  \nbar\nbar\n"))],
        })
        .to_string();
        let out = tidy(Bytes::copy_from_slice(body.as_bytes()), &Options::parity());
        assert_eq!(out.as_ref(), body.as_bytes());
    }

    #[test]
    fn cleans_noise_in_anthropic_tool_result() {
        let content = "\u{1b}[31mred\u{1b}[0m   \nprogress 10%\rprogress 20%\rprogress 100%\nwarn\nwarn\nwarn\nlast  ";
        let body = json!({"messages": [tool_result(json!(content))]}).to_string();
        let out = parsed(&body, &Options::default());
        assert_eq!(
            out["messages"][0]["content"][0]["content"],
            json!("red\nprogress 100%\nwarn (repeated 3 times)\nlast")
        );
    }

    #[test]
    fn cleans_text_parts_of_tool_result_array() {
        let body = json!({
            "messages": [tool_result(json!([{"type": "text", "text": "\u{1b}[32mgreen\u{1b}[0m"}]))],
        })
        .to_string();
        let out = parsed(&body, &Options::default());
        assert_eq!(
            out["messages"][0]["content"][0]["content"][0]["text"],
            json!("green")
        );
    }

    #[test]
    fn cached_prefix_stays_untouched() {
        let body = json!({
            "messages": [
                tool_result(json!("\u{1b}[31mold\u{1b}[0m")),
                {"role": "assistant", "content": [
                    {"type": "text", "text": "ok", "cache_control": {"type": "ephemeral"}}
                ]},
                tool_result(json!("\u{1b}[31mnew\u{1b}[0m")),
            ],
        })
        .to_string();
        let out = parsed(&body, &Options::default());
        assert_eq!(
            out["messages"][0]["content"][0]["content"],
            json!("\u{1b}[31mold\u{1b}[0m")
        );
        assert_eq!(out["messages"][1]["content"][0]["text"], json!("ok"));
        assert_eq!(out["messages"][2]["content"][0]["content"], json!("new"));
    }

    #[test]
    fn chat_only_cleans_last_two_turns() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "q1"},
                {"role": "tool", "content": "\u{1b}[31mold\u{1b}[0m"},
                {"role": "user", "content": "q2"},
                {"role": "tool", "content": [{"type": "text", "text": "\u{1b}[31mnew\u{1b}[0m"}]},
            ],
        })
        .to_string();
        let out = parsed(&body, &Options::default());
        assert_eq!(
            out["messages"][1]["content"],
            json!("\u{1b}[31mold\u{1b}[0m")
        );
        assert_eq!(out["messages"][3]["content"][0]["text"], json!("new"));
    }

    #[test]
    fn responses_function_call_output_is_cleaned() {
        let body = json!({
            "input": [
                {"type": "message", "role": "user", "content": "q1"},
                {"type": "function_call_output", "call_id": "c1", "output": "\u{1b}[32mgreen\u{1b}[0m"},
                {"type": "function_call_output", "call_id": "c2",
                 "output": [{"type": "input_text", "text": "\u{1b}[32mblue \u{1b}[0m"}]},
            ],
        })
        .to_string();
        let out = parsed(&body, &Options::default());
        assert_eq!(out["input"][1]["output"], json!("green"));
        assert_eq!(out["input"][2]["output"][0]["text"], json!("blue"));
    }

    #[test]
    fn caps_long_output_keeping_head_and_tail() {
        let body = json!({"messages": [tool_result(json!("a\nb\nc\nd\ne"))]}).to_string();
        let opts = Options {
            max_lines: Some(3),
            ..Options::parity()
        };
        let out = parsed(&body, &opts);
        assert_eq!(
            out["messages"][0]["content"][0]["content"],
            json!("a\n[... 2 lines omitted ...]\nd\ne")
        );
    }

    #[test]
    fn crlf_endings_survive() {
        let body = json!({"messages": [tool_result(json!("a  \r\nb\t\r\n"))]}).to_string();
        let out = parsed(&body, &Options::default());
        assert_eq!(
            out["messages"][0]["content"][0]["content"],
            json!("a\r\nb\r\n")
        );
    }

    #[test]
    fn edits_do_not_touch_surrounding_bytes() {
        let body = "{\n \"model\" : \"m\",\n \"messages\":[{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"t\",\"content\":\"\\u001b[31mx\\u001b[0m\"}]}]}";
        let out = run(body, &Options::default());
        assert!(out.starts_with("{\n \"model\" : \"m\",\n \"messages\""));
        assert!(out.contains("\"content\":\"x\""));
        assert!(serde_json::from_str::<Value>(&out).is_ok());
    }

    #[test]
    fn oversized_body_is_untouched() {
        let body = format!(
            "{{\"messages\":[{{\"role\":\"tool\",\"content\":\"\\u001b[31m{}\"}}]}}",
            "x".repeat(MAX_BODY)
        );
        let out = tidy(Bytes::copy_from_slice(body.as_bytes()), &Options::default());
        assert_eq!(out.as_ref(), body.as_bytes());
    }
}

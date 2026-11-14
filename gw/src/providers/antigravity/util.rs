//! Deterministic wire values: session ids, tool-call ids, request ids, tool names.

use http::HeaderMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256, needed for the session id and the tool-name codec.
pub fn sha256(input: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = input.to_vec();
    let bits = (input.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_be_bytes());
    for block in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn entropy() -> u64 {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(nanos);
    hasher.write_u64(COUNT.fetch_add(1, Ordering::Relaxed));
    hasher.finish()
}

/// UUIDv4 shape; only the format matters upstream.
pub fn uuid4() -> String {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&entropy().to_be_bytes());
    bytes[8..].copy_from_slice(&entropy().to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex(&bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Conversation anchor: Codex thread ids when present, else the first user text.
pub fn session_anchor(headers: &HeaderMap, chat: &serde_json::Value) -> Option<String> {
    let parent = header(headers, "x-codex-parent-thread-id");
    let own = header(headers, "thread-id");
    if let Some(parent) = parent {
        return Some(match own {
            Some(own) => format!("codex-thread:{parent}\0{own}"),
            None => format!("codex-thread:{parent}"),
        });
    }
    first_user_text(chat)
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn first_user_text(chat: &serde_json::Value) -> Option<String> {
    for message in chat.get("messages").and_then(serde_json::Value::as_array)? {
        if message.get("role").and_then(serde_json::Value::as_str) != Some("user") {
            continue;
        }
        match message.get("content") {
            Some(serde_json::Value::String(text)) => return Some(text.clone()),
            Some(serde_json::Value::Array(parts)) => {
                if let Some(text) = parts
                    .iter()
                    .find(|part| {
                        part.get("type").and_then(serde_json::Value::as_str) == Some("text")
                    })
                    .and_then(|part| part.get("text"))
                    .and_then(serde_json::Value::as_str)
                {
                    return Some(text.to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

/// `-<int64>`; deterministic from the anchor, random when the conversation has none.
pub fn session_id(anchor: Option<&str>) -> String {
    let Some(anchor) = anchor else {
        return format!("-{}", entropy() % 9_000_000_000_000_000_000);
    };
    let digest = sha256(anchor.as_bytes());
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    format!("-{}", u64::from_be_bytes(head) & 0x7fff_ffff_ffff_ffff)
}

const TOOL_NAME_CHARS: usize = 64;

fn tool_name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    name.len() <= TOOL_NAME_CHARS
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Wire name per declaration plus the inverse for response mapping.
#[derive(Debug, Default, Clone)]
pub struct ToolNames {
    pairs: Vec<(String, String)>,
}

impl ToolNames {
    pub fn new<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for name in names {
            if pairs.iter().any(|(wire, _)| wire == name) {
                continue;
            }
            let wire = if tool_name_ok(name) && !pairs.iter().any(|(wire, _)| wire == name) {
                name.to_owned()
            } else {
                let cleaned: String = name
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                let cleaned = if cleaned.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                {
                    cleaned
                } else {
                    format!("_{cleaned}")
                };
                let prefix: String = cleaned.chars().take(55).collect();
                format!("{prefix}_{}", &hex(&sha256(name.as_bytes()))[..8])
            };
            pairs.push((wire, name.to_owned()));
        }
        Self { pairs }
    }

    pub fn to_wire(&self, name: &str) -> String {
        self.pairs
            .iter()
            .find(|(_, original)| original == name)
            .map(|(wire, _)| wire.clone())
            .unwrap_or_else(|| name.to_owned())
    }

    pub fn original(&self, name: &str) -> String {
        self.pairs
            .iter()
            .find(|(wire, _)| wire == name)
            .map(|(_, original)| original.clone())
            .unwrap_or_else(|| name.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn session_id_is_stable_and_masked() {
        let id = session_id(Some("codex-thread:abc"));
        assert_eq!(id, session_id(Some("codex-thread:abc")));
        assert!(id.starts_with('-'));
        let value: u64 = id[1..].parse().unwrap();
        assert!(value <= 0x7fff_ffff_ffff_ffff);
        assert!(session_id(None).starts_with('-'));
    }

    #[test]
    fn session_anchor_prefers_thread_headers() {
        let chat = serde_json::json!({ "messages": [{ "role": "user", "content": "hello" }] });
        let mut headers = HeaderMap::new();
        assert_eq!(session_anchor(&headers, &chat).as_deref(), Some("hello"));
        headers.insert("x-codex-parent-thread-id", "p".parse().unwrap());
        assert_eq!(
            session_anchor(&headers, &chat).as_deref(),
            Some("codex-thread:p")
        );
        headers.insert("thread-id", "o".parse().unwrap());
        assert_eq!(
            session_anchor(&headers, &chat).as_deref(),
            Some("codex-thread:p\0o")
        );
    }

    #[test]
    fn tool_names_pass_through_or_get_hashed() {
        let names = ToolNames::new(["read_file", "mcp.fs/read", "update plan"]);
        assert_eq!(names.to_wire("read_file"), "read_file");
        assert_eq!(names.original("read_file"), "read_file");
        let wire = names.to_wire("mcp.fs/read");
        assert!(wire.starts_with("mcp_fs_read_"), "{wire}");
        assert_eq!(wire.len(), "mcp_fs_read_".len() + 8);
        assert_eq!(names.original(&wire), "mcp.fs/read");
        assert!(names.to_wire("update plan").starts_with("update_plan_"));
        assert_eq!(names.to_wire("unknown"), "unknown");
    }

    #[test]
    fn uuid4_has_the_right_shape() {
        let id = uuid4();
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'4');
        assert!(id.chars().filter(|c| *c == '-').count() == 4);
        assert_ne!(id, uuid4());
    }
}

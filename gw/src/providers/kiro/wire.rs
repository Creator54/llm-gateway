//! Kiro wire constants, endpoint, headers, and identifier normalization.

use super::digest::{sha256, sha256_hex};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const AMZ_TARGET: &str = "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";
/// The kiro-cli release whose traffic gw reproduces, header for header.
const CLI_VERSION: &str = "2.23.1";
const DEFAULT_REGION: &str = "us-east-1";
pub(super) const COMPLETION_TOOL: &str = "codex_kiro_final_answer";

/// `api_region` → region from the profile ARN → `sso_region` → `us-east-1`.
pub(crate) fn region(extra: &BTreeMap<String, String>) -> String {
    normalize_region(extra.get("api_region").map(String::as_str))
        .or_else(|| {
            extra
                .get("profile_arn")
                .and_then(|arn| infer_region_from_arn(arn))
        })
        .or_else(|| normalize_region(extra.get("sso_region").map(String::as_str)))
        .unwrap_or_else(|| DEFAULT_REGION.to_owned())
}

fn normalize_region(region: Option<&str>) -> Option<String> {
    let trimmed = region?.trim();
    let parts: Vec<&str> = trimmed.split('-').collect();
    let last = parts.last().copied().unwrap_or_default();
    let shape_ok = parts.len() >= 3
        && parts[0].len() == 2
        && parts[0].bytes().all(|b| b.is_ascii_lowercase())
        && parts[1..parts.len() - 1]
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase()))
        && !last.is_empty()
        && last.bytes().all(|b| b.is_ascii_digit());
    shape_ok.then(|| trimmed.to_owned())
}

fn infer_region_from_arn(arn: &str) -> Option<String> {
    normalize_region(arn.split(':').nth(3))
}

/// Runtime endpoint: a configured base URL is used verbatim unless it is the canonical
/// region-bearing host with a bare path.
pub(super) fn endpoint(base_url: Option<&str>, region: &str) -> Result<String, String> {
    let Some(base) = base_url else {
        return Ok(format!("https://runtime.{region}.kiro.dev/"));
    };
    let url = reqwest::Url::parse(base).map_err(|error| format!("invalid base_url: {error}"))?;
    if is_runtime_host(url.host_str().unwrap_or_default()) && url.path() == "/" {
        return Ok(format!("https://runtime.{region}.kiro.dev/"));
    }
    Ok(url.to_string())
}

/// The legacy canonical host, used when the runtime host is unreachable.
pub(super) fn fallback_endpoint(base_url: Option<&str>, region: &str) -> Option<String> {
    if let Some(base) = base_url {
        let url = reqwest::Url::parse(base).ok()?;
        if !is_runtime_host(url.host_str().unwrap_or_default()) || url.path() != "/" {
            return None;
        }
    }
    Some(format!("https://q.{region}.amazonaws.com/"))
}

fn is_runtime_host(host: &str) -> bool {
    let Some(rest) = host.strip_prefix("runtime.") else {
        return false;
    };
    let Some(region) = rest.strip_suffix(".kiro.dev") else {
        return false;
    };
    normalize_region(Some(region)).is_some()
}

/// The origin kiro-cli stamps on user messages and management calls.
pub(super) const ORIGIN: &str = "KIRO_CLI";

/// Ordered header list matching kiro-cli; insertion order is wire order.
pub(super) fn headers(token: &str) -> Vec<(&'static str, String)> {
    let (agent, amz_agent) = user_agents("codewhispererstreaming/0.1.17975");
    let mut headers = vec![
        ("content-type", "application/x-amz-json-1.0".to_owned()),
        ("x-amz-target", AMZ_TARGET.to_owned()),
        ("user-agent", agent),
        ("x-amz-user-agent", amz_agent),
        ("x-amzn-codewhisperer-optout", "false".to_owned()),
        ("x-kiro-attempt", "1;max=3".to_owned()),
        ("authorization", format!("Bearer {token}")),
        ("amz-sdk-request", "attempt=1; max=3".to_owned()),
        ("amz-sdk-invocation-id", uuid_v4()),
        ("accept", "*/*".to_owned()),
    ];
    if token.starts_with("ksk_") {
        headers.push(("tokentype", "API_KEY".to_owned()));
    }
    headers
}

/// Headers for kiro-cli's `GetUsageLimits` call, minus `authorization`.
pub(super) fn usage_headers() -> Vec<(&'static str, String)> {
    let (agent, amz_agent) = user_agents("codewhispererruntime/0.1.17975");
    vec![
        ("content-type", "application/x-amz-json-1.0".to_owned()),
        (
            "x-amz-target",
            "AmazonCodeWhispererService.GetUsageLimits".to_owned(),
        ),
        ("user-agent", agent),
        ("x-amz-user-agent", amz_agent),
        ("x-amzn-codewhisperer-optout", "false".to_owned()),
        ("amz-sdk-request", "attempt=1; max=3".to_owned()),
        ("amz-sdk-invocation-id", uuid_v4()),
        ("accept", "*/*".to_owned()),
    ]
}

/// kiro-cli's `envState`: the host OS and the working directory it runs in.
pub(super) fn env_state() -> serde_json::Value {
    let mut state = serde_json::json!({ "operatingSystem": platform() });
    if let Ok(dir) = std::env::current_dir() {
        state["currentWorkingDirectory"] = dir.to_string_lossy().into();
    }
    state
}

fn platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// The `user-agent` and `x-amz-user-agent` pair the CLI's AWS SDK sends for `api`.
fn user_agents(api: &str) -> (String, String) {
    let base = format!(
        "aws-sdk-rust/1.3.15 ua/2.1 api/{api} os/{} lang/rust/1.92.0",
        platform()
    );
    (
        format!("{base} md/appVersion-{CLI_VERSION} app/AmazonQ-For-CLI"),
        format!("{base} m/F app/AmazonQ-For-CLI"),
    )
}

/// UUID v4 text, from the OS CSPRNG when available.
pub(super) fn uuid_v4() -> String {
    let bytes = random_bytes();
    format_uuid(&bytes, 4)
}

/// Deterministic per-session conversation id, stable across turns.
pub(super) fn conversation_id(session: &str) -> String {
    let digest = sha256(session.as_bytes());
    format_uuid(&digest[..16], 5)
}

fn format_uuid(bytes: &[u8], version: u8) -> String {
    let mut b = [0u8; 16];
    b.copy_from_slice(&bytes[..16]);
    b[6] = (b[6] & 0x0f) | (version << 4);
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: Vec<String> = b.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..16].concat()
    )
}

fn random_bytes() -> [u8; 16] {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        if file.read_exact(&mut bytes).is_ok() {
            return bytes;
        }
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seed = sha256(
        format!(
            "{nanos}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
        .as_bytes(),
    );
    bytes.copy_from_slice(&seed[..16]);
    bytes
}

/// Registry/user model id to the CodeWhisperer model id.
pub(super) fn normalize_model_id(id: &str) -> String {
    let mut model = id.trim().to_lowercase();
    if let Some(rest) = model.strip_prefix("kiro/") {
        model = rest.to_owned();
    } else if let Some(rest) = model.strip_prefix("kiro-") {
        model = rest.to_owned();
    }
    if model == "auto" {
        return model;
    }
    model = strip_date_suffix(&model);
    if let Some(rest) = ["-low", "-medium", "-high", "-xhigh", "-max"]
        .iter()
        .find_map(|suffix| model.strip_suffix(suffix))
    {
        model = rest.to_owned();
    }
    model = dotted_version(&model);
    claude_family_first(&model)
}

fn strip_date_suffix(model: &str) -> String {
    let Some(last) = model.rsplit('-').next() else {
        return model.to_owned();
    };
    if last.len() == 8 && last.bytes().all(|b| b.is_ascii_digit()) {
        model
            .strip_suffix(&format!("-{last}"))
            .unwrap_or(model)
            .to_owned()
    } else {
        model.to_owned()
    }
}

fn dotted_version(model: &str) -> String {
    let chars: Vec<char> = model.chars().collect();
    let mut out = String::with_capacity(model.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '-'
            && i > 0
            && chars[i - 1].is_ascii_digit()
            && chars.get(i + 1).is_some_and(char::is_ascii_digit)
        {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && chars[end].is_ascii_digit() {
                end += 1;
            }
            out.push('.');
            out.extend(&chars[start..end]);
            i = end;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn claude_family_first(model: &str) -> String {
    let Some(rest) = model.strip_prefix("claude-") else {
        return model.to_owned();
    };
    let (version, family) = match rest.rsplit_once('-') {
        Some(split) => split,
        None => return model.to_owned(),
    };
    let version_ok =
        !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    if version_ok && matches!(family, "sonnet" | "opus" | "haiku") {
        format!("claude-{family}-{version}")
    } else {
        model.to_owned()
    }
}

/// Tool ids must match `^[a-zA-Z0-9_-]{1,64}$`.
pub(super) fn normalize_tool_id(id: &str) -> String {
    let mut cleaned: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    cleaned.truncate(64);
    cleaned
}

/// Per-request tool name aliases, shared by the catalog and replayed calls.
#[derive(Debug, Default, Clone)]
pub(super) struct ToolNames {
    used: BTreeSet<String>,
    wire_to_kiro: BTreeMap<String, String>,
    kiro_to_wire: BTreeMap<String, String>,
}

impl ToolNames {
    pub(super) fn new() -> Self {
        let mut used = BTreeSet::new();
        used.insert(COMPLETION_TOOL.to_owned());
        Self {
            used,
            wire_to_kiro: BTreeMap::new(),
            kiro_to_wire: BTreeMap::new(),
        }
    }

    pub(super) fn alias(&mut self, wire: &str) -> String {
        assert!(wire != COMPLETION_TOOL, "reserved kiro tool name");
        if let Some(existing) = self.wire_to_kiro.get(wire) {
            return existing.clone();
        }
        let cleaned: String = wire
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        let alias = if cleaned == wire
            && !cleaned.is_empty()
            && cleaned.len() <= 64
            && !self.used.contains(&cleaned)
        {
            cleaned
        } else {
            let base: String = cleaned.chars().take(55).collect();
            let base = if base.is_empty() {
                "tool".to_owned()
            } else {
                base
            };
            (0u32..)
                .map(|salt| {
                    let input = if salt == 0 {
                        wire.to_owned()
                    } else {
                        format!("{wire}#{salt}")
                    };
                    let suffix: String = sha256_hex(input.as_bytes()).chars().take(8).collect();
                    format!("{base}_{suffix}")
                })
                .find(|candidate| !self.used.contains(candidate))
                .expect("free alias")
        };
        self.used.insert(alias.clone());
        self.wire_to_kiro.insert(wire.to_owned(), alias.clone());
        if alias != *wire {
            self.kiro_to_wire.insert(alias.clone(), wire.to_owned());
        }
        alias
    }

    /// The client's own tool name for one Kiro coined.
    pub(super) fn restore(&self, kiro: &str) -> String {
        self.kiro_to_wire
            .get(kiro)
            .cloned()
            .unwrap_or_else(|| kiro.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extra(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn normalizes_model_ids() {
        assert_eq!(
            normalize_model_id("kiro/claude-sonnet-4-5"),
            "claude-sonnet-4.5"
        );
        assert_eq!(
            normalize_model_id("kiro/claude-3-7-sonnet"),
            "claude-sonnet-3.7"
        );
        assert_eq!(
            normalize_model_id("kiro/claude-3-7-sonnet-20250219-high"),
            "claude-3.7-sonnet-20250219"
        );
        assert_eq!(
            normalize_model_id("kiro/claude-3-7-sonnet-20250219"),
            "claude-sonnet-3.7"
        );
        assert_eq!(normalize_model_id("kiro/auto"), "auto");
        assert_eq!(normalize_model_id(" gpt-5.6-sol "), "gpt-5.6-sol");
    }

    #[test]
    fn resolves_region_in_priority_order() {
        assert_eq!(region(&extra(&[])), "us-east-1");
        assert_eq!(region(&extra(&[("sso_region", "eu-west-1")])), "eu-west-1");
        assert_eq!(
            region(&extra(&[
                ("sso_region", "eu-west-1"),
                ("profile_arn", "arn:aws:codewhisperer:us-east-2:1:profile/x")
            ])),
            "us-east-2"
        );
        assert_eq!(
            region(&extra(&[
                ("api_region", "ap-south-1"),
                ("profile_arn", "arn:aws:codewhisperer:us-east-2:1:profile/x")
            ])),
            "ap-south-1"
        );
        assert_eq!(
            region(&extra(&[("api_region", "not a region")])),
            "us-east-1"
        );
    }

    #[test]
    fn builds_runtime_endpoint() {
        assert_eq!(
            endpoint(None, "eu-west-1").unwrap(),
            "https://runtime.eu-west-1.kiro.dev/"
        );
        assert_eq!(
            endpoint(Some("https://runtime.us-east-1.kiro.dev/"), "eu-west-1").unwrap(),
            "https://runtime.eu-west-1.kiro.dev/"
        );
        assert_eq!(
            endpoint(Some("http://127.0.0.1:8080/v1/kiro"), "eu-west-1").unwrap(),
            "http://127.0.0.1:8080/v1/kiro"
        );
    }

    #[test]
    fn alias_keeps_conforming_names_and_hashes_others() {
        let mut names = ToolNames::new();
        assert_eq!(names.alias("read_file"), "read_file");
        let long = format!("mcp__workspace agents_create_agent_{}", "x".repeat(40));
        let alias = names.alias(&long);
        assert!(alias.len() <= 64, "{alias}");
        assert!(alias.ends_with(|c: char| c.is_ascii_hexdigit()));
        assert_eq!(names.alias(&long), alias);
    }

    #[test]
    fn normalizes_tool_ids() {
        assert_eq!(normalize_tool_id("call/1 x"), "call_1_x");
        assert_eq!(normalize_tool_id(&"a".repeat(80)).len(), 64);
    }
}

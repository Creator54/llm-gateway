//! Upstream adapters, one per provider. Each implements `crate::provider::Provider`.

pub mod anthropic;
pub mod antigravity;
pub mod command_code;
pub mod kiro;
pub mod openai;
pub mod opencode;

/// Provider names that have an adapter, spelled as routes and `[endpoints]` use them.
pub const NAMES: [&str; 6] = [
    "anthropic",
    "antigravity",
    "command-code",
    "kiro",
    "openai",
    "opencode",
];

/// The adapter name for `name`, folding the spellings opencodex and the CLIs use.
pub fn canonical(name: &str) -> &str {
    match name {
        "claude" => "anthropic",
        "chatgpt" | "codex" => "openai",
        "google-antigravity" => "antigravity",
        "commandcode" => "command-code",
        "opencode-cli" | "opencode-zen" | "opencode-free" => "opencode",
        other => other,
    }
}

/// Whether `name` has a native adapter.
pub fn is_provider(name: &str) -> bool {
    NAMES.contains(&name)
}

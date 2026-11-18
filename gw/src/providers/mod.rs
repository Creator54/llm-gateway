//! Upstream adapters, one per provider. Each implements `crate::provider::Provider`.

pub mod anthropic;
pub mod antigravity;
mod chat;
pub mod command_code;
pub mod kiro;
pub mod openai;
pub mod openai_api;
pub mod opencode;

/// Provider names that have an adapter, spelled as routes and `[endpoints]` use them.
pub const NAMES: [&str; 7] = [
    "anthropic",
    "antigravity",
    "command-code",
    "kiro",
    "openai",
    "openai-api",
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

/// One spelling per model for usage rows, where a provider accepts several.
pub fn usage_model(provider: &str, model: &str) -> String {
    match provider {
        "kiro" => kiro::model_id(model),
        _ => model.to_owned(),
    }
}

/// Whether `name` has a native adapter.
pub fn is_provider(name: &str) -> bool {
    NAMES.contains(&name)
}

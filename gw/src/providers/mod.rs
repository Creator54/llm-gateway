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

/// Whether `name` has a native adapter.
pub fn is_provider(name: &str) -> bool {
    NAMES.contains(&name)
}

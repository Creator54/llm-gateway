//! `auto` routing: pick a target from simple rules.
//!
//! `profiles` maps a profile name to rules tried in order. The first rule whose
//! conditions all hold wins; the last rule must have no conditions and is the
//! default. Matching is pure: no I/O, no regex, no clock.

use crate::provider::Inbound;
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeMap, fmt};

/// Profile used for a bare `auto`.
pub const DEFAULT_PROFILE: &str = "default";

/// Request facts that rule conditions match on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Features {
    pub has_images: bool,
    pub has_tools: bool,
    /// Request body size in tokens, estimated as bytes / 4.
    pub input_tokens: usize,
    pub reasoning: bool,
    /// The `model` the client sent; `client_model_prefix` matches against it.
    pub model: String,
    pub inbound: Inbound,
}

/// Extract matchable facts from a request body. Unparseable bodies match no
/// optional feature, so routing still falls back to the default rule.
pub fn features(body: &[u8], inbound: Inbound) -> Features {
    let input_tokens = body.len() / 4;
    let Ok(json) = serde_json::from_slice::<Value>(body) else {
        return Features {
            has_images: false,
            has_tools: false,
            input_tokens,
            reasoning: false,
            model: String::new(),
            inbound,
        };
    };
    Features {
        has_images: has_image_content(&json, inbound),
        has_tools: json
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty()),
        input_tokens,
        reasoning: reasoning_on(&json, inbound),
        model: json
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        inbound,
    }
}

/// Rules live under the protocol's message list: `input` for Responses,
/// `messages` for Chat and Messages.
fn has_image_content(body: &Value, inbound: Inbound) -> bool {
    let items = match inbound {
        Inbound::Responses => body.get("input"),
        Inbound::Chat | Inbound::Messages => body.get("messages"),
    };
    items.and_then(Value::as_array).is_some_and(|items| {
        items
            .iter()
            .any(|item| item.get("content").is_some_and(content_has_image))
    })
}

fn content_has_image(content: &Value) -> bool {
    match content {
        Value::Array(parts) => parts.iter().any(content_has_image),
        Value::Object(_) => {
            matches!(
                content.get("type").and_then(Value::as_str),
                Some("input_image" | "image_url" | "image")
            ) || content.get("content").is_some_and(content_has_image)
        }
        _ => false,
    }
}

fn reasoning_on(body: &Value, inbound: Inbound) -> bool {
    fn effort(value: Option<&Value>) -> bool {
        value
            .and_then(Value::as_str)
            .is_some_and(|effort| effort != "none")
    }
    match inbound {
        Inbound::Chat => {
            effort(body.get("reasoning_effort")) || effort(body.pointer("/reasoning/effort"))
        }
        Inbound::Responses => effort(body.pointer("/reasoning/effort")),
        Inbound::Messages => {
            body.pointer("/thinking/type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == "enabled")
                || body
                    .pointer("/thinking/budget_tokens")
                    .and_then(Value::as_u64)
                    .is_some_and(|budget| budget > 0)
        }
    }
}

/// `auto` routing config: named profiles of ordered rules.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Routing {
    pub profiles: BTreeMap<String, Vec<Rule>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    #[serde(default)]
    pub when: When,
    #[serde(rename = "use")]
    pub target: String,
}

/// Conditions, all optional and ANDed. A rule with none is the default.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct When {
    pub has_images: Option<bool>,
    pub has_tools: Option<bool>,
    pub min_input_tokens: Option<usize>,
    pub max_input_tokens: Option<usize>,
    pub reasoning: Option<bool>,
    pub inbound: Option<InboundName>,
    /// Prefix match on the client's `model`; no regex dependency.
    pub client_model_prefix: Option<String>,
}

/// Config spelling of `provider::Inbound`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InboundName {
    Responses,
    Chat,
    Messages,
}

impl InboundName {
    fn matches(self, inbound: Inbound) -> bool {
        matches!(
            (self, inbound),
            (Self::Responses, Inbound::Responses)
                | (Self::Chat, Inbound::Chat)
                | (Self::Messages, Inbound::Messages)
        )
    }
}

impl When {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Share of the rule vs. request; `min_input_tokens` and `max_input_tokens`
    /// are inclusive bounds.
    pub fn matches(&self, f: &Features) -> bool {
        self.has_images.is_none_or(|want| want == f.has_images)
            && self.has_tools.is_none_or(|want| want == f.has_tools)
            && self
                .min_input_tokens
                .is_none_or(|min| f.input_tokens >= min)
            && self
                .max_input_tokens
                .is_none_or(|max| f.input_tokens <= max)
            && self.reasoning.is_none_or(|want| want == f.reasoning)
            && self.inbound.is_none_or(|want| want.matches(f.inbound))
            && self
                .client_model_prefix
                .as_deref()
                .is_none_or(|prefix| f.model.starts_with(prefix))
    }
}

impl Routing {
    /// Pick the target for `profile` (`None` means the bare `auto` profile).
    pub fn select<'a>(
        &'a self,
        profile: Option<&str>,
        f: &Features,
    ) -> Result<&'a str, SelectError> {
        let name = profile.unwrap_or(DEFAULT_PROFILE);
        let rules = self
            .profiles
            .get(name)
            .ok_or_else(|| SelectError::UnknownProfile(name.to_owned()))?;
        rules
            .iter()
            .find(|rule| rule.when.matches(f))
            .map(|rule| rule.target.as_str())
            .ok_or_else(|| SelectError::NoDefaultRule(name.to_owned()))
    }

    /// Reject profiles whose fallback is missing or unreachable, and targets
    /// that would not route.
    pub fn validate(&self) -> Result<(), String> {
        for (name, rules) in &self.profiles {
            let Some((default, conditional)) = rules.split_last() else {
                return Err(format!("profile '{name}' has no rules"));
            };
            if !default.when.is_default() {
                return Err(format!(
                    "profile '{name}': last rule must have no conditions"
                ));
            }
            if let Some(index) = conditional.iter().position(|rule| rule.when.is_default()) {
                return Err(format!(
                    "profile '{name}': rule {index} has no conditions, so every rule after it is dead"
                ));
            }
            for (index, rule) in rules.iter().enumerate() {
                check_target(name, index, &rule.target)?;
            }
        }
        Ok(())
    }
}

fn check_target(profile: &str, index: usize, target: &str) -> Result<(), String> {
    match crate::route::parse(target) {
        Some(crate::route::Target::Model { .. } | crate::route::Target::Combo(_)) => Ok(()),
        Some(crate::route::Target::Auto(_)) => Err(format!(
            "profile '{profile}': rule {index} uses '{target}', which is auto again"
        )),
        None => Err(format!(
            "profile '{profile}': rule {index} target '{target}' is not provider/model or combo/name"
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    UnknownProfile(String),
    NoDefaultRule(String),
}

impl fmt::Display for SelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownProfile(name) => write!(f, "unknown auto profile '{name}'"),
            Self::NoDefaultRule(name) => write!(f, "auto profile '{name}' matches no rule"),
        }
    }
}

impl std::error::Error for SelectError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn load(toml_text: &str) -> Routing {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            routing: Routing,
        }
        let file: File = toml::from_str(toml_text).expect("toml");
        file.routing.validate().expect("valid");
        file.routing
    }

    fn feat(model: &str, inbound: Inbound) -> Features {
        Features {
            has_images: false,
            has_tools: false,
            input_tokens: 100,
            reasoning: false,
            model: model.to_owned(),
            inbound,
        }
    }

    const SAMPLE: &str = r#"
        [[routing.profiles.main]]
        when = { has_images = true }
        use = "openai/gpt-6-vision"

        [[routing.profiles.main]]
        when = { reasoning = true, min_input_tokens = 2000 }
        use = "anthropic/claude-opus"

        [[routing.profiles.main]]
        when = { client_model_prefix = "claude-" }
        use = "anthropic/claude-sonnet"

        [[routing.profiles.main]]
        when = { inbound = "messages" }
        use = "combo/anthropic-pool"

        [[routing.profiles.main]]
        use = "command-code/deepseek/deepseek-v4.1-flash"
    "#;

    #[test]
    fn first_matching_rule_wins_in_order() {
        let routing = load(SAMPLE);
        let mut f = feat("claude-sonnet-4", Inbound::Messages);
        f.has_images = true;
        f.reasoning = true;
        f.input_tokens = 5000;
        assert_eq!(routing.select(Some("main"), &f), Ok("openai/gpt-6-vision"));

        f.has_images = false;
        assert_eq!(
            routing.select(Some("main"), &f),
            Ok("anthropic/claude-opus")
        );

        f.reasoning = false;
        assert_eq!(
            routing.select(Some("main"), &f),
            Ok("anthropic/claude-sonnet")
        );

        let messages = feat("gpt-6", Inbound::Messages);
        assert_eq!(
            routing.select(Some("main"), &messages),
            Ok("combo/anthropic-pool")
        );
        let chat = feat("gpt-6", Inbound::Chat);
        assert_eq!(
            routing.select(Some("main"), &chat),
            Ok("command-code/deepseek/deepseek-v4.1-flash")
        );
    }

    #[test]
    fn falls_back_to_default_rule() {
        let routing = load(SAMPLE);
        let f = feat("gpt-6", Inbound::Chat);
        assert_eq!(
            routing.select(Some("main"), &f),
            Ok("command-code/deepseek/deepseek-v4.1-flash")
        );
        // A bare `auto` uses the "default" profile.
        let bare: Routing = load("[[routing.profiles.default]]\nuse = \"combo/cheap\"\n");
        assert_eq!(bare.select(None, &f), Ok("combo/cheap"));
    }

    #[test]
    fn reports_missing_profiles_and_fallbacks() {
        let routing = load(SAMPLE);
        let f = feat("gpt-6", Inbound::Chat);
        assert_eq!(
            routing.select(Some("typo"), &f),
            Err(SelectError::UnknownProfile("typo".into()))
        );
        let unvalidated: Routing =
            toml::from_str("[[profiles.p]]\nwhen = { has_images = true }\nuse = \"combo/x\"\n")
                .expect("toml");
        assert_eq!(
            unvalidated.select(Some("p"), &f),
            Err(SelectError::NoDefaultRule("p".into()))
        );
    }

    #[test]
    fn bounds_are_inclusive_and_all_conditions_and() {
        let routing = load(
            r#"
            [[routing.profiles.p]]
            when = { has_tools = true, min_input_tokens = 50, max_input_tokens = 150 }
            use = "combo/tools"

            [[routing.profiles.p]]
            use = "combo/plain"
            "#,
        );
        let mut f = feat("x", Inbound::Chat);
        f.has_tools = true;
        assert_eq!(routing.select(Some("p"), &f), Ok("combo/tools"));
        f.input_tokens = 50;
        assert_eq!(routing.select(Some("p"), &f), Ok("combo/tools"));
        f.input_tokens = 150;
        assert_eq!(routing.select(Some("p"), &f), Ok("combo/tools"));
        f.input_tokens = 151;
        assert_eq!(routing.select(Some("p"), &f), Ok("combo/plain"));
        f.input_tokens = 100;
        f.has_tools = false;
        assert_eq!(routing.select(Some("p"), &f), Ok("combo/plain"));
    }

    #[test]
    fn validate_rejects_bad_profiles() {
        fn err(text: &str) -> String {
            toml::from_str::<Routing>(text)
                .expect("toml")
                .validate()
                .expect_err("invalid")
        }
        assert!(err("[profiles]\np = []\n").contains("no rules"));
        assert!(
            err("[[profiles.p]]\nwhen = { has_images = true }\nuse = \"combo/x\"\n")
                .contains("last rule")
        );
        assert!(err(
            "[[profiles.p]]\nuse = \"combo/x\"\n[[profiles.p]]\nwhen = { reasoning = true }\nuse = \"combo/y\"\n[[profiles.p]]\nuse = \"combo/z\"\n"
        )
        .contains("dead"));
        assert!(err("[[profiles.p]]\nuse = \"auto\"\n").contains("auto again"));
        assert!(err("[[profiles.p]]\nuse = \"nonsense\"\n").contains("not provider/model"));
    }

    #[test]
    fn rejects_unknown_conditions() {
        let bad = toml::from_str::<Routing>(
            "[[profiles.p]]\nwhen = { model_hint = \"x\" }\nuse = \"combo/x\"\n",
        );
        assert!(bad.is_err());
    }

    #[test]
    fn extracts_responses_features() {
        let body = json!({
            "model": "auto",
            "input": [
                { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "look" },
                    { "type": "input_image", "image_url": "data:image/png;base64,AAAA" }
                ]}
            ],
            "tools": [{ "type": "function", "name": "read", "parameters": { "type": "object" } }],
            "reasoning": { "effort": "low" }
        });
        let raw = serde_json::to_vec(&body).unwrap();
        let f = features(&raw, Inbound::Responses);
        assert!(f.has_images);
        assert!(f.has_tools);
        assert!(f.reasoning);
        assert_eq!(f.model, "auto");
        assert_eq!(f.inbound, Inbound::Responses);
        assert_eq!(f.input_tokens, raw.len() / 4);

        let quiet = features(
            &serde_json::to_vec(&json!({
                "model": "auto",
                "input": "plain text",
                "tools": [],
                "reasoning": { "effort": "none" }
            }))
            .unwrap(),
            Inbound::Responses,
        );
        assert!(!quiet.has_images);
        assert!(!quiet.has_tools);
        assert!(!quiet.reasoning);
    }

    #[test]
    fn extracts_chat_features() {
        let body = json!({
            "model": "gpt-6",
            "messages": [
                { "role": "system", "content": "be terse" },
                { "role": "user", "content": [
                    { "type": "text", "text": "what is this" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }
                ]},
                { "role": "tool", "tool_call_id": "c1", "content": [
                    { "type": "image_url", "image_url": { "url": "https://example/x.png" } }
                ]}
            ],
            "tools": [{ "type": "function", "function": { "name": "read" } }],
            "reasoning_effort": "high"
        });
        let f = features(&serde_json::to_vec(&body).unwrap(), Inbound::Chat);
        assert!(f.has_images);
        assert!(f.has_tools);
        assert!(f.reasoning);
        assert_eq!(f.model, "gpt-6");
        assert_eq!(f.inbound, Inbound::Chat);

        let off = features(
            &serde_json::to_vec(&json!({
                "model": "gpt-6",
                "messages": [{ "role": "user", "content": "hi" }],
                "reasoning_effort": "none"
            }))
            .unwrap(),
            Inbound::Chat,
        );
        assert!(!off.has_images);
        assert!(!off.reasoning);
    }

    #[test]
    fn extracts_messages_features() {
        let body = json!({
            "model": "claude-sonnet-4",
            "messages": [{ "role": "user", "content": [
                { "type": "text", "text": "describe" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } }
            ]}],
            "tools": [{ "name": "read", "input_schema": { "type": "object" } }],
            "thinking": { "type": "enabled", "budget_tokens": 1024 }
        });
        let f = features(&serde_json::to_vec(&body).unwrap(), Inbound::Messages);
        assert!(f.has_images);
        assert!(f.has_tools);
        assert!(f.reasoning);
        assert_eq!(f.model, "claude-sonnet-4");
        assert_eq!(f.inbound, Inbound::Messages);

        let disabled = features(
            &serde_json::to_vec(&json!({
                "model": "claude-sonnet-4",
                "messages": [{ "role": "user", "content": [{ "type": "text", "text": "hi" }] }],
                "thinking": { "type": "disabled" }
            }))
            .unwrap(),
            Inbound::Messages,
        );
        assert!(!disabled.has_images);
        assert!(!disabled.has_tools);
        assert!(!disabled.reasoning);
    }

    #[test]
    fn unparseable_body_still_estimates_tokens() {
        let raw = b"not json at all";
        let f = features(raw, Inbound::Chat);
        assert_eq!(f.input_tokens, raw.len() / 4);
        assert_eq!(f.model, "");
        assert!(!f.has_images && !f.has_tools && !f.reasoning);
    }
}

//! What a client's `model` string asks for.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `provider/model` (accounts rotate) or `provider@account/model` (one account).
    Model {
        provider: String,
        account: Option<String>,
        model: String,
    },
    /// `combo/<name>`
    Combo(String),
    /// `auto` or `auto/<profile>`
    Auto(Option<String>),
}

/// Split at the first `/`; the rest is the provider's own model id, which may contain `/`.
pub fn parse(model: &str) -> Option<Target> {
    if model == "auto" {
        return Some(Target::Auto(None));
    }
    let (head, rest) = model.split_once('/')?;
    if rest.is_empty() || head.is_empty() {
        return None;
    }
    Some(match head {
        "combo" => Target::Combo(rest.into()),
        "auto" => Target::Auto(Some(rest.into())),
        _ => {
            let (provider, account) = match head.split_once('@') {
                Some((p, a)) if !p.is_empty() && !a.is_empty() => (p, Some(a.to_string())),
                Some(_) => return None,
                None => (head, None),
            };
            Target::Model {
                provider: provider.into(),
                account,
                model: rest.into(),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_forms() {
        assert_eq!(parse("auto"), Some(Target::Auto(None)));
        assert_eq!(
            parse("auto/cheap"),
            Some(Target::Auto(Some("cheap".into())))
        );
        assert_eq!(
            parse("combo/pi-subagents"),
            Some(Target::Combo("pi-subagents".into()))
        );
        assert_eq!(
            parse("command-code/deepseek/deepseek-v4.1-flash"),
            Some(Target::Model {
                provider: "command-code".into(),
                account: None,
                model: "deepseek/deepseek-v4.1-flash".into()
            })
        );
        assert_eq!(
            parse("openai@work/gpt-6-luna"),
            Some(Target::Model {
                provider: "openai".into(),
                account: Some("work".into()),
                model: "gpt-6-luna".into()
            })
        );
        for bad in ["gpt-6", "openai/", "/x", "@a/x", "openai@/x"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}

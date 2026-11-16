//! Subscription quota per account, read from the provider and cached in `quota.json`.
//!
//! Anthropic and ChatGPT report usage windows (5-hour, weekly, …); Antigravity reports a
//! remaining fraction per model family. Providers without such an endpoint are not listed.

use crate::config::Config;
use crate::creds::{self, Entry};
use crate::discover::{access_token, account_extra, fresh};
use crate::providers::{anthropic, antigravity, command_code, kiro};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One usage window of one account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub provider: String,
    pub account: String,
    pub name: String,
    pub used_percent: f64,
    /// Unix time at which the window resets, when the provider says.
    pub resets_at: Option<u64>,
}

/// Cached figures older than this are fetched again.
const MAX_AGE_SECS: u64 = 300;
const FILE: &str = "quota.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    fetched_at: u64,
    windows: Vec<Window>,
}

/// A failed probe: which account, and why.
pub struct Failure {
    pub provider: String,
    pub account: String,
    pub message: String,
}

/// Quota for every account: the cache while it is fresh, otherwise a new fetch.
///
/// The cache is stale after five minutes, once any window's reset time has passed, or when
/// `force` is set. An account whose probe fails keeps its cached windows, and the failure is
/// returned so the caller can say the figures are old.
pub async fn load(cfg: &Config, force: bool) -> anyhow::Result<(Vec<Window>, Vec<Failure>)> {
    let path = cfg.state_dir.join(FILE);
    let cached: Cache = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let now = crate::login::now_secs();
    let expired = cached
        .windows
        .iter()
        .any(|w| w.resets_at.is_some_and(|at| at <= now));
    let known = |store: &creds::Store| {
        PROVIDERS.iter().all(|p| {
            store.accounts(p).iter().all(|e| {
                let name = e.label.as_deref().unwrap_or(&e.id);
                cached
                    .windows
                    .iter()
                    .any(|w| w.provider == *p && w.account == name)
            })
        })
    };
    let store = creds::Store::load(&cfg.state_dir)?;
    if !force && !expired && now.saturating_sub(cached.fetched_at) < MAX_AGE_SECS && known(&store) {
        return Ok((cached.windows, Vec::new()));
    }
    let (mut windows, failures) = fetch(cfg).await?;
    for old in &cached.windows {
        let failed = failures
            .iter()
            .any(|f| f.provider == old.provider && f.account == old.account);
        if failed {
            windows.push(old.clone());
        }
    }
    let cache = Cache {
        fetched_at: now,
        windows,
    };
    // Best effort: an unwritable state dir only costs a refetch next time.
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = std::fs::write(&path, bytes);
    }
    Ok((cache.windows, failures))
}

const PROVIDERS: [&str; 5] = ["anthropic", "openai", "antigravity", "command-code", "kiro"];

/// Quota windows for every account that has a quota endpoint. An account whose probe
/// fails is reported in the second list instead of aborting the others.
async fn fetch(cfg: &Config) -> anyhow::Result<(Vec<Window>, Vec<Failure>)> {
    let store = creds::Store::load(&cfg.state_dir)?;
    let client = reqwest::Client::new();
    let now = crate::login::now_secs();
    let (mut windows, mut failures) = (Vec::new(), Vec::new());
    for provider in PROVIDERS {
        for entry in store.accounts(provider) {
            let name = entry.label.clone().unwrap_or_else(|| entry.id.clone());
            match probe(&store, &client, provider, entry).await {
                Ok(found) => {
                    windows.extend(found.into_iter().map(|(window, used, resets_in)| Window {
                        provider: provider.to_owned(),
                        account: name.clone(),
                        name: window,
                        used_percent: used,
                        resets_at: resets_in.map(|secs| now + secs),
                    }))
                }
                Err(e) => failures.push(Failure {
                    provider: provider.to_owned(),
                    account: name.clone(),
                    message: format!("{e:#}"),
                }),
            }
        }
    }
    Ok((windows, failures))
}

type Found = Vec<(String, f64, Option<u64>)>;

async fn probe(
    store: &creds::Store,
    client: &reqwest::Client,
    provider: &str,
    entry: Entry,
) -> anyhow::Result<Found> {
    anyhow::ensure!(!entry.needs_reauth, "needs login");
    let entry = fresh(store, client, provider, entry).await?;
    let token = access_token(&entry)?;
    let now = crate::login::now_secs();
    let extra = crate::provider::Account::from(&entry).extra;
    let request = match provider {
        "anthropic" => client
            .get("https://api.anthropic.com/api/oauth/usage")
            .header("anthropic-beta", anthropic::OAUTH_BETA),
        "openai" => client
            .get("https://chatgpt.com/backend-api/wham/usage")
            .header("chatgpt-account-id", account_extra(&entry, "account_id")?),
        "command-code" => client
            .get(format!(
                "{}/alpha/billing/credits",
                command_code::DEFAULT_BASE_URL
            ))
            .header("user-agent", "cli")
            .header(
                "x-command-code-version",
                extra
                    .get("command_code_version")
                    .map_or(command_code::DEFAULT_VERSION, String::as_str),
            )
            .header("x-cli-environment", "production"),
        "kiro" => {
            let arn = account_extra(&entry, "profile_arn")?;
            let host = format!("https://management.{}.kiro.dev/", kiro::region(&extra));
            client
                .post(host)
                .query(&[
                    ("origin", "AI_EDITOR"),
                    ("isEmailRequired", "true"),
                    ("profileArn", arn.as_str()),
                ])
                .header("content-type", "application/x-amz-json-1.0")
                .header("x-amz-target", "AmazonCodeWhispererService.GetUsageLimits")
                .header("x-amzn-codewhisperer-optout", "true")
                .body(
                    json!({ "origin": "AI_EDITOR", "isEmailRequired": true, "profileArn": arn })
                        .to_string(),
                )
        }
        _ => client
            .post(format!(
                "{}/v1internal:fetchAvailableModels",
                antigravity::DEFAULT_BASE_URL
            ))
            .header("user-agent", antigravity::USER_AGENT)
            .header("content-type", "application/json")
            .body(json!({ "project": account_extra(&entry, "project_id")? }).to_string()),
    };
    let response = request.bearer_auth(&token).send().await?;
    let status = response.status();
    anyhow::ensure!(status.is_success(), "HTTP {status}");
    let body: Value = serde_json::from_slice(&response.bytes().await?)?;
    Ok(match provider {
        "anthropic" => anthropic_windows(&body, now),
        "openai" => openai_windows(&body),
        "command-code" => command_code_windows(&body),
        "kiro" => kiro_windows(&body),
        _ => antigravity_windows(&body, now),
    })
}

fn anthropic_windows(body: &Value, now: u64) -> Found {
    [
        ("five_hour", "5h"),
        ("seven_day", "week"),
        ("seven_day_opus", "week opus"),
        ("seven_day_sonnet", "week sonnet"),
    ]
    .iter()
    .filter_map(|(key, name)| {
        let used = body[key]["utilization"].as_f64()?;
        let resets_in = body[key]["resets_at"]
            .as_str()
            .and_then(parse_rfc3339)
            .map(|at| at.saturating_sub(now));
        Some(((*name).to_owned(), used, resets_in))
    })
    .collect()
}

fn openai_windows(body: &Value) -> Found {
    let limit = &body["rate_limit"];
    ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|key| {
            let window = &limit[key];
            let used = window["used_percent"].as_f64()?;
            let secs = window["limit_window_seconds"].as_u64()?;
            Some((
                window_name(secs),
                used,
                window["reset_after_seconds"].as_u64(),
            ))
        })
        .collect()
}

/// Antigravity quota is per model; models of one family share it, so the most-used
/// model stands for the family.
fn antigravity_windows(body: &Value, now: u64) -> Found {
    let mut families: Vec<(&str, f64, Option<u64>)> = Vec::new();
    for (id, model) in body["models"].as_object().into_iter().flatten() {
        let family = if id.starts_with("gemini") {
            "gemini"
        } else if id.starts_with("claude") || id.starts_with("gpt-oss") {
            "claude"
        } else {
            continue;
        };
        let info = &model["quotaInfo"];
        // A missing fraction means the quota is exhausted.
        let used = (1.0 - info["remainingFraction"].as_f64().unwrap_or(0.0)) * 100.0;
        let resets_in = info["resetTime"]
            .as_str()
            .and_then(parse_rfc3339)
            .map(|at| at.saturating_sub(now));
        match families.iter_mut().find(|(f, ..)| *f == family) {
            Some(entry) if used > entry.1 => *entry = (family, used, resets_in),
            Some(_) => {}
            None => families.push((family, used, resets_in)),
        }
    }
    families
        .into_iter()
        .map(|(family, used, resets_in)| (family.to_owned(), used, resets_in))
        .collect()
}

/// Command Code reports `windowLimits.{fiveHour,weekly}` as `{used, cap, resetAt}`.
fn command_code_windows(body: &Value) -> Found {
    let body = if body["data"].is_object() {
        &body["data"]
    } else {
        body
    };
    [("fiveHour", "5h"), ("weekly", "week")]
        .iter()
        .filter_map(|(key, name)| {
            let window = &body["windowLimits"][key];
            let (used, cap) = (window["used"].as_f64()?, window["cap"].as_f64()?);
            let resets_in = window["resetAt"]
                .as_str()
                .or_else(|| window["resetTime"].as_str())
                .and_then(parse_rfc3339)
                .map(|at| at.saturating_sub(crate::login::now_secs()));
            (cap > 0.0).then(|| ((*name).to_owned(), used / cap * 100.0, resets_in))
        })
        .collect()
}

/// Kiro reports one credit pool per resource type; the agentic pool wins, as in opencodex.
fn kiro_windows(body: &Value) -> Found {
    let rows = body["usageBreakdownList"].as_array().into_iter().flatten();
    let pick = |wanted: &str| {
        rows.clone().find(|r| {
            r["resourceType"]
                .as_str()
                .is_some_and(|t| t.eq_ignore_ascii_case(wanted))
        })
    };
    let Some(row) = pick("AGENTIC_REQUEST").or_else(|| pick("CREDIT")) else {
        return Vec::new();
    };
    let number = |precise: &str, whole: &str| row[precise].as_f64().or_else(|| row[whole].as_f64());
    let (Some(used), Some(limit)) = (
        number("currentUsageWithPrecision", "currentUsage"),
        number("usageLimitWithPrecision", "usageLimit"),
    ) else {
        return Vec::new();
    };
    // `nextDateReset` is epoch seconds (sometimes with a fraction).
    let resets_in = body["nextDateReset"]
        .as_f64()
        .map(|at| (at as u64).saturating_sub(crate::login::now_secs()));
    if limit <= 0.0 {
        return Vec::new();
    }
    vec![("month".to_owned(), used / limit * 100.0, resets_in)]
}

fn window_name(seconds: u64) -> String {
    match seconds {
        18_000 => "5h".to_owned(),
        604_800 => "week".to_owned(),
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s => format!("{}h", s / 3_600),
    }
}

/// Seconds since the epoch for an RFC 3339 UTC timestamp (`2026-11-16T16:45:26Z`, or with a
/// fraction and `+00:00`). Other offsets are not produced by these providers.
fn parse_rfc3339(text: &str) -> Option<u64> {
    let (date, time) = text.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.get(..8)?.split(':').map(|p| p.parse::<i64>().ok());
    let (h, min, s) = (t.next()??, t.next()??, t.next()??);
    // Days from civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3_600 + min * 60 + s).ok()
}

/// "2h 5m", "3d 4h", "42s".
pub fn duration(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h {}m", seconds / 3_600, seconds % 3_600 / 60),
        _ => format!("{}d {}h", seconds / 86_400, seconds % 86_400 / 3_600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utc_timestamps_with_and_without_fraction() {
        assert_eq!(parse_rfc3339("1970-01-02T00:00:00Z"), Some(86_400));
        assert_eq!(parse_rfc3339("2026-11-16T16:45:26Z"), Some(1_794_847_526));
        assert_eq!(
            parse_rfc3339("2026-11-16T16:39:59.722974+00:00"),
            Some(1_794_847_199)
        );
        assert_eq!(parse_rfc3339("garbage"), None);
    }

    #[test]
    fn anthropic_windows_report_utilization_and_reset() {
        let body = json!({
            "five_hour": { "utilization": 41.0, "resets_at": "1970-01-01T01:00:00+00:00" },
            "seven_day": { "utilization": 81.0, "resets_at": null },
            "seven_day_opus": null
        });
        let got = anthropic_windows(&body, 0);
        assert_eq!(
            got,
            [
                ("5h".to_owned(), 41.0, Some(3_600)),
                ("week".to_owned(), 81.0, None)
            ]
        );
    }

    #[test]
    fn openai_windows_are_named_by_length() {
        let body = json!({ "rate_limit": {
            "primary_window": { "used_percent": 5, "limit_window_seconds": 18000, "reset_after_seconds": 60 },
            "secondary_window": { "used_percent": 7, "limit_window_seconds": 604800, "reset_after_seconds": 9 }
        }});
        let got = openai_windows(&body);
        assert_eq!(got[0], ("5h".to_owned(), 5.0, Some(60)));
        assert_eq!(got[1].0, "week");
        assert_eq!(window_name(2_592_000), "30d");
    }

    #[test]
    fn antigravity_family_uses_its_most_used_model() {
        let body = json!({ "models": {
            "gemini-a": { "quotaInfo": { "remainingFraction": 0.9 } },
            "gemini-b": { "quotaInfo": { "remainingFraction": 0.5 } },
            "claude-x": { "quotaInfo": {} },
            "tab_flash": { "quotaInfo": { "remainingFraction": 1 } }
        }});
        let got = antigravity_windows(&body, 0);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "gemini");
        assert!((got[0].1 - 50.0).abs() < 1e-9);
        assert_eq!(got[1], ("claude".to_owned(), 100.0, None));
    }

    #[test]
    fn command_code_and_kiro_windows() {
        let cc = json!({ "data": { "windowLimits": {
            "fiveHour": { "used": 1, "cap": 4 }, "weekly": { "used": 0, "cap": 0 } } } });
        assert_eq!(command_code_windows(&cc), [("5h".to_owned(), 25.0, None)]);
        let kiro = json!({ "nextDateReset": 4102444800.0, "usageBreakdownList": [
            { "resourceType": "CREDIT", "currentUsage": 9, "usageLimit": 10 },
            { "resourceType": "AGENTIC_REQUEST", "currentUsageWithPrecision": 5.0, "usageLimitWithPrecision": 20.0 }] });
        let got = kiro_windows(&kiro);
        assert_eq!((got[0].0.as_str(), got[0].1), ("month", 25.0));
        assert!(kiro_windows(&json!({})).is_empty());
    }

    #[test]
    fn durations_are_short() {
        assert_eq!(duration(42), "42s");
        assert_eq!(duration(7_500), "2h 5m");
        assert_eq!(duration(300_000), "3d 11h");
    }
}

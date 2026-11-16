//! Equivalent API cost of recorded usage, priced from the public llm-prices feed.
//!
//! Subscription providers bill nothing per token, so the figure is what the same tokens
//! would cost at list price. The feed is cached in `<state_dir>/prices.json` for a day.

use crate::store::UsageSummary;
use serde::{Deserialize, Serialize};
use std::path::Path;

const URL: &str = "https://www.llm-prices.com/current-v1.json";
const FILE: &str = "prices.json";
const MAX_AGE_SECS: u64 = 24 * 3_600;

/// Dollars per million tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Price {
    id: String,
    input: f64,
    output: f64,
    input_cached: Option<f64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Prices {
    prices: Vec<Price>,
}

impl Prices {
    /// The cached feed, refreshed when older than a day. A failed refresh falls back to the
    /// stale cache; without any cache the result is empty and no cost is shown.
    pub async fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(FILE);
        let age = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs());
        let cached = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok());
        if let (Some(prices), Some(age)) = (&cached, age) {
            if age < MAX_AGE_SECS && !prices.prices.is_empty() {
                return cached.unwrap_or_default();
            }
        }
        match fetch().await {
            Ok((prices, raw)) => {
                // Best effort: a read-only state dir only costs a refetch next time.
                let _ = std::fs::write(&path, raw);
                prices
            }
            Err(_) => cached.unwrap_or_default(),
        }
    }

    /// Equivalent list-price dollars for a usage row, or `None` for an unpriced model.
    pub fn cost(&self, row: &UsageSummary) -> Option<f64> {
        let price = self.find(&row.model)?;
        let cached = price.input_cached.unwrap_or(price.input);
        let tokens = |n: u64, per_million: f64| n as f64 * per_million / 1e6;
        Some(
            tokens(row.input, price.input)
                + tokens(row.cached_read, cached)
                + tokens(row.cache_write, price.input)
                + tokens(row.output, price.output),
        )
    }

    fn find(&self, model: &str) -> Option<&Price> {
        let wanted = normalise(model);
        self.prices.iter().find(|p| normalise(&p.id) == wanted)
    }
}

async fn fetch() -> anyhow::Result<(Prices, Vec<u8>)> {
    let response = reqwest::Client::new()
        .get(URL)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?;
    let raw = response.bytes().await?.to_vec();
    Ok((serde_json::from_slice(&raw)?, raw))
}

/// `deepseek/deepseek-v4.1-flash--fast` → `deepseek-v4-1-flash`: drop the vendor prefix and
/// the `--fast` suffix, and treat `.` and `-` alike so `claude-haiku-4.5` meets `claude-haiku-4-5`.
fn normalise(model: &str) -> String {
    let name = model.rsplit('/').next().unwrap_or(model);
    let name = name.strip_suffix("--fast").unwrap_or(name);
    name.to_ascii_lowercase().replace('.', "-")
}

/// "$1.23", "$0.004", or "-" for an unpriced model.
pub fn dollars(cost: Option<f64>) -> String {
    match cost {
        None => "-".to_owned(),
        Some(c) if c < 0.01 => format!("${c:.4}"),
        Some(c) => format!("${c:.2}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prices() -> Prices {
        serde_json::from_str(
            r#"{"prices":[
                {"id":"claude-haiku-4.5","input":1,"output":5,"input_cached":0.1},
                {"id":"gpt-x","input":2,"output":8,"input_cached":null}]}"#,
        )
        .unwrap()
    }

    fn row(model: &str) -> UsageSummary {
        UsageSummary {
            provider: "p".into(),
            model: model.into(),
            account: "a".into(),
            requests: 1,
            errors: 0,
            input: 1_000_000,
            cached_read: 1_000_000,
            cache_write: 0,
            output: 1_000_000,
            cache_read_share: 0.0,
        }
    }

    #[test]
    fn prices_input_cached_and_output_separately() {
        // 1 + 0.1 + 5
        let cost = prices().cost(&row("anthropic/claude-haiku-4-5")).unwrap();
        assert!((cost - 6.1).abs() < 1e-9);
    }

    #[test]
    fn cached_tokens_fall_back_to_the_input_price_and_fast_suffix_is_ignored() {
        // 2 + 2 + 8
        let cost = prices().cost(&row("gpt-x--fast")).unwrap();
        assert!((cost - 12.0).abs() < 1e-9);
        assert!(prices().cost(&row("unknown")).is_none());
    }

    #[test]
    fn dollars_are_short() {
        assert_eq!(dollars(None), "-");
        assert_eq!(dollars(Some(0.0042)), "$0.0042");
        assert_eq!(dollars(Some(12.345)), "$12.35");
    }
}

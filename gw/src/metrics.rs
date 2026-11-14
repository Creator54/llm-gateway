//! In-memory metrics with Prometheus text exposition.
//!
//! Scalars are atomics. Labelled families take one short mutex lock per update;
//! request rates are low enough that this is cheaper than anything sharded.
//! Every family caps its label sets so a bad provider or model name cannot grow
//! memory without bound: samples that would exceed the cap are recorded under
//! the label value `other` and counted in `gw_metrics_label_overflow_total`.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use crate::store;

/// Content type for the Prometheus text exposition format.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Histogram buckets in seconds, shared by every duration family.
pub const BUCKETS: [f64; 12] = [
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];

/// Maximum distinct label sets per family before folding into `other`.
const MAX_LABEL_SETS: usize = 2000;

/// Label value that absorbs everything beyond the cap.
const OTHER: &str = "other";

type RequestKey = (String, String, String, String);
type TokenKey = (String, String, String);
type FailoverKey = (String, String, String);
type TransformKey = (String, String);
type CoolingKey = (String, String);

/// Token counts split by direction, as providers report them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenKind {
    Input,
    Output,
    CacheRead,
    CacheWrite,
}

impl TokenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Input => "input",
            TokenKind::Output => "output",
            TokenKind::CacheRead => "cache_read",
            TokenKind::CacheWrite => "cache_write",
        }
    }
}

/// Outcome of a request or response transform.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransformResult {
    Applied,
    Unchanged,
    Failed,
}

impl TransformResult {
    pub fn as_str(self) -> &'static str {
        match self {
            TransformResult::Applied => "applied",
            TransformResult::Unchanged => "unchanged",
            TransformResult::Failed => "failed",
        }
    }
}

/// Per-bucket counts; rendered as a cumulative histogram.
#[derive(Default)]
struct Histogram {
    counts: [u64; BUCKETS.len()],
    sum: f64,
    count: u64,
}

impl Histogram {
    fn observe(&mut self, secs: f64) {
        self.count += 1;
        self.sum += secs;
        let idx = BUCKETS.partition_point(|b| *b < secs);
        if let Some(c) = self.counts.get_mut(idx) {
            *c += 1;
        }
    }
}

/// Values the renderer cannot read from [`Metrics`] itself.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Extra {
    pub journal_dropped: u64,
    pub journal_lost: u64,
}

impl Extra {
    pub fn new(journal_dropped: u64, journal_lost: u64) -> Self {
        Self {
            journal_dropped,
            journal_lost,
        }
    }
}

/// Shared metrics registry; mount one per process behind an `Arc`.
pub struct Metrics {
    started: Instant,
    requests: Mutex<BTreeMap<RequestKey, u64>>,
    tokens: Mutex<BTreeMap<TokenKey, u64>>,
    failovers: Mutex<BTreeMap<FailoverKey, u64>>,
    transforms: Mutex<BTreeMap<TransformKey, u64>>,
    bytes_saved: Mutex<BTreeMap<String, u64>>,
    duration: Mutex<BTreeMap<String, Histogram>>,
    ttfb: Mutex<BTreeMap<String, Histogram>>,
    cooling: Mutex<BTreeMap<CoolingKey, bool>>,
    inflight: AtomicI64,
    overflow: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            requests: Mutex::new(BTreeMap::new()),
            tokens: Mutex::new(BTreeMap::new()),
            failovers: Mutex::new(BTreeMap::new()),
            transforms: Mutex::new(BTreeMap::new()),
            bytes_saved: Mutex::new(BTreeMap::new()),
            duration: Mutex::new(BTreeMap::new()),
            ttfb: Mutex::new(BTreeMap::new()),
            cooling: Mutex::new(BTreeMap::new()),
            inflight: AtomicI64::new(0),
            overflow: AtomicU64::new(0),
        }
    }

    /// Count one finished request. `status` is a class: `2xx`, `4xx`, `429`,
    /// `5xx` or `error`.
    pub fn request(&self, provider: &str, model: &str, account: &str, status: &str) {
        let key = (s(provider), s(model), s(account), s(status));
        let fold = |k: &RequestKey| (other(), other(), other(), k.3.clone());
        self.bump(&self.requests, key, fold);
    }

    /// Add token counts for one request.
    pub fn tokens(&self, provider: &str, model: &str, kind: TokenKind, n: u64) {
        let key = (s(provider), s(model), s(kind.as_str()));
        let fold = |k: &TokenKey| (other(), other(), k.2.clone());
        self.add(&self.tokens, key, fold, n);
    }

    /// Count one failover between accounts.
    pub fn failover(&self, from: &str, to: &str, reason: &str) {
        let key = (s(from), s(to), s(reason));
        let fold = |k: &FailoverKey| (other(), other(), k.2.clone());
        self.bump(&self.failovers, key, fold);
    }

    /// Count one transform and the bytes it saved, if any.
    pub fn transform(&self, name: &str, result: TransformResult, bytes_saved: u64) {
        let key = (s(name), s(result.as_str()));
        let fold = |k: &TransformKey| (other(), k.1.clone());
        self.bump(&self.transforms, key, fold);
        if bytes_saved > 0 {
            self.add(&self.bytes_saved, s(name), |_| other(), bytes_saved);
        }
    }

    /// Record the full request duration in seconds.
    pub fn observe_duration(&self, provider: &str, secs: f64) {
        observe(&self.duration, &self.overflow, provider, secs);
    }

    /// Record time to first byte in seconds.
    pub fn observe_ttfb(&self, provider: &str, secs: f64) {
        observe(&self.ttfb, &self.overflow, provider, secs);
    }

    /// Count a request in flight until the returned guard is dropped.
    pub fn inflight(&self) -> InflightGuard<'_> {
        self.inflight.fetch_add(1, Ordering::Relaxed);
        InflightGuard {
            gauge: &self.inflight,
        }
    }

    /// Mark an account as cooling down (1) or recovered (0).
    pub fn set_cooling(&self, provider: &str, account: &str, cooling: bool) {
        let mut m = lock(&self.cooling);
        m.insert((s(provider), s(account)), cooling);
    }

    /// Render every family in Prometheus text format, sorted and deterministic.
    pub fn render(&self, extra: &Extra) -> String {
        let mut out = String::with_capacity(4096);

        header(
            &mut out,
            "gw_requests_total",
            "Finished requests by provider, model, account and status class.",
            "counter",
        );
        for ((provider, model, account, status), v) in lock(&self.requests).iter() {
            sample(
                &mut out,
                "gw_requests_total",
                &[
                    ("provider", provider),
                    ("model", model),
                    ("account", account),
                    ("status", status),
                ],
                *v,
            );
        }

        header(
            &mut out,
            "gw_tokens_total",
            "Tokens by provider, model and kind.",
            "counter",
        );
        for ((provider, model, kind), v) in lock(&self.tokens).iter() {
            sample(
                &mut out,
                "gw_tokens_total",
                &[("provider", provider), ("model", model), ("kind", kind)],
                *v,
            );
        }

        header(
            &mut out,
            "gw_failovers_total",
            "Failovers between accounts.",
            "counter",
        );
        for ((from, to, reason), v) in lock(&self.failovers).iter() {
            sample(
                &mut out,
                "gw_failovers_total",
                &[("from", from), ("to", to), ("reason", reason)],
                *v,
            );
        }

        header(
            &mut out,
            "gw_transforms_total",
            "Transforms by name and result.",
            "counter",
        );
        for ((name, result), v) in lock(&self.transforms).iter() {
            sample(
                &mut out,
                "gw_transforms_total",
                &[("name", name), ("result", result)],
                *v,
            );
        }

        header(
            &mut out,
            "gw_transform_bytes_saved_total",
            "Bytes saved by transforms.",
            "counter",
        );
        for (name, v) in lock(&self.bytes_saved).iter() {
            sample(
                &mut out,
                "gw_transform_bytes_saved_total",
                &[("name", name)],
                *v,
            );
        }

        render_histogram(
            &mut out,
            "gw_request_duration_seconds",
            "Full request duration in seconds.",
            &self.duration,
        );
        render_histogram(
            &mut out,
            "gw_time_to_first_byte_seconds",
            "Time to first response byte in seconds.",
            &self.ttfb,
        );

        header(
            &mut out,
            "gw_inflight_requests",
            "Requests currently being served.",
            "gauge",
        );
        sample(
            &mut out,
            "gw_inflight_requests",
            &[],
            self.inflight.load(Ordering::Relaxed),
        );

        header(
            &mut out,
            "gw_account_cooling",
            "Accounts cooling down: 1 while cooling, else 0.",
            "gauge",
        );
        for ((provider, account), cooling) in lock(&self.cooling).iter() {
            sample(
                &mut out,
                "gw_account_cooling",
                &[("provider", provider), ("account", account)],
                u8::from(*cooling),
            );
        }

        header(
            &mut out,
            "gw_journal_dropped_total",
            "Usage rows dropped because the journal buffer was full.",
            "counter",
        );
        sample(
            &mut out,
            "gw_journal_dropped_total",
            &[],
            extra.journal_dropped,
        );
        header(
            &mut out,
            "gw_journal_lost_total",
            "Usage rows lost because a journal write failed twice.",
            "counter",
        );
        sample(&mut out, "gw_journal_lost_total", &[], extra.journal_lost);

        header(
            &mut out,
            "gw_metrics_label_overflow_total",
            "Samples folded into the other label because a family was full.",
            "counter",
        );
        sample(
            &mut out,
            "gw_metrics_label_overflow_total",
            &[],
            self.overflow.load(Ordering::Relaxed),
        );

        header(
            &mut out,
            "gw_build_info",
            "Build version information.",
            "gauge",
        );
        sample(
            &mut out,
            "gw_build_info",
            &[("version", env!("CARGO_PKG_VERSION"))],
            1,
        );

        if let Some(bytes) = resident_memory_bytes() {
            header(
                &mut out,
                "gw_process_resident_memory_bytes",
                "Resident memory from /proc/self/statm.",
                "gauge",
            );
            sample(&mut out, "gw_process_resident_memory_bytes", &[], bytes);
        }

        header(
            &mut out,
            "gw_uptime_seconds",
            "Seconds since metrics were created.",
            "gauge",
        );
        sample(
            &mut out,
            "gw_uptime_seconds",
            &[],
            num(self.started.elapsed().as_secs_f64()),
        );

        out
    }

    /// Increment a counter whose label set may fold into `other` at the cap.
    fn bump<K: Ord>(&self, map: &Mutex<BTreeMap<K, u64>>, key: K, fold: impl FnOnce(&K) -> K) {
        self.add(map, key, fold, 1);
    }

    /// Add to a counter, folding when a new label set would break the cap.
    /// The existing-label path allocates nothing.
    fn add<K: Ord>(
        &self,
        map: &Mutex<BTreeMap<K, u64>>,
        key: K,
        fold: impl FnOnce(&K) -> K,
        n: u64,
    ) {
        let mut m = lock(map);
        if let Some(v) = m.get_mut(&key) {
            *v += n;
            return;
        }
        if m.len() >= MAX_LABEL_SETS {
            self.overflow.fetch_add(1, Ordering::Relaxed);
            let folded = fold(&key);
            *m.entry(folded).or_insert(0) += n;
        } else {
            m.insert(key, n);
        }
    }
}

/// Decrements `gw_inflight_requests` when dropped.
pub struct InflightGuard<'a> {
    gauge: &'a AtomicI64,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.gauge.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Headers and formatted rows for the `stats` table in [`crate::ui`].
pub fn stats_table(rows: &[store::UsageSummary]) -> (Vec<&'static str>, Vec<Vec<String>>) {
    let headers = vec![
        "provider",
        "model",
        "account",
        "requests",
        "errors",
        "input",
        "output",
        "cache read",
        "cache share",
    ];
    let body = rows
        .iter()
        .map(|r| {
            vec![
                r.provider.clone(),
                r.model.clone(),
                r.account.clone(),
                human(r.requests),
                human(r.errors),
                human(r.input),
                human(r.output),
                human(r.cached_read),
                format!("{:.0}%", r.cache_read_share * 100.0),
            ]
        })
        .collect();
    (headers, body)
}

/// Compact token counts: 999, 1.2k, 3.4M, 5.6G.
pub fn human(n: u64) -> String {
    const UNITS: [(f64, &str); 4] = [(1.0, ""), (1e3, "k"), (1e6, "M"), (1e9, "G")];
    let mut unit = match n {
        0..=999 => 0,
        1_000..=999_999 => 1,
        1_000_000..=999_999_999 => 2,
        _ => 3,
    };
    let mut text = format!("{:.1}", n as f64 / UNITS[unit].0);
    // A value rounding to 1000.0 belongs in the next unit, not as "1000k".
    while text == "1000.0" && unit + 1 < UNITS.len() {
        unit += 1;
        text = format!("{:.1}", n as f64 / UNITS[unit].0);
    }
    format!("{}{}", text.trim_end_matches(".0"), UNITS[unit].1)
}

fn observe(
    map: &Mutex<BTreeMap<String, Histogram>>,
    overflow: &AtomicU64,
    provider: &str,
    secs: f64,
) {
    let mut m = lock(map);
    if !m.contains_key(provider) && m.len() >= MAX_LABEL_SETS {
        overflow.fetch_add(1, Ordering::Relaxed);
        m.entry(s(OTHER)).or_default().observe(secs);
    } else {
        m.entry(s(provider)).or_default().observe(secs);
    }
}

fn render_histogram(
    out: &mut String,
    name: &str,
    help: &str,
    map: &Mutex<BTreeMap<String, Histogram>>,
) {
    header(out, name, help, "histogram");
    for (provider, h) in lock(map).iter() {
        let mut cumulative = 0;
        for (bucket, count) in BUCKETS.iter().zip(h.counts.iter()) {
            cumulative += count;
            let le = num(*bucket);
            out.push_str(name);
            out.push_str("_bucket");
            push_labels(out, &[("provider", provider), ("le", &le)]);
            push_value(out, cumulative);
        }
        out.push_str(name);
        out.push_str("_bucket");
        push_labels(out, &[("provider", provider), ("le", "+Inf")]);
        push_value(out, h.count);
        out.push_str(name);
        out.push_str("_sum");
        push_labels(out, &[("provider", provider)]);
        push_value(out, num(h.sum));
        out.push_str(name);
        out.push_str("_count");
        push_labels(out, &[("provider", provider)]);
        push_value(out, h.count);
    }
}

fn header(out: &mut String, name: &str, help: &str, kind: &str) {
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(help);
    out.push_str("\n# TYPE ");
    out.push_str(name);
    out.push(' ');
    out.push_str(kind);
    out.push('\n');
}

fn sample(out: &mut String, name: &str, labels: &[(&str, &str)], value: impl Display) {
    out.push_str(name);
    push_labels(out, labels);
    push_value(out, value);
}

fn push_value(out: &mut String, value: impl Display) {
    out.push(' ');
    out.push_str(&value.to_string());
    out.push('\n');
}

fn push_labels(out: &mut String, labels: &[(&str, &str)]) {
    if labels.is_empty() {
        return;
    }
    out.push('{');
    for (i, (key, value)) in labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(key);
        out.push_str("=\"");
        push_escaped(out, value);
        out.push('"');
    }
    out.push('}');
}

fn push_escaped(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

/// Shortest exact float text, so `1` never renders as `1.000000`.
fn num(v: f64) -> String {
    format!("{v}")
}

fn s(v: &str) -> String {
    v.to_string()
}

fn other() -> String {
    s(OTHER)
}

/// Never panic on a poisoned lock: metrics are lossy but must keep serving.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Resident set size from `/proc/self/statm` with a 4 KiB page size.
#[cfg(target_os = "linux")]
fn resident_memory_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = text.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

#[cfg(not(target_os = "linux"))]
fn resident_memory_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_lines(out: &str, prefix: &str) -> usize {
        out.lines().filter(|l| l.starts_with(prefix)).count()
    }

    fn value_of(out: &str, line_start: &str) -> f64 {
        let line = out
            .lines()
            .find(|l| l.starts_with(line_start))
            .unwrap_or_else(|| panic!("no line {line_start}"));
        line.rsplit(' ').next().unwrap().parse().unwrap()
    }

    #[test]
    fn render_has_help_and_type_for_every_family() {
        let m = Metrics::new();
        let out = m.render(&Extra::default());
        for family in [
            "gw_requests_total",
            "gw_tokens_total",
            "gw_failovers_total",
            "gw_transforms_total",
            "gw_transform_bytes_saved_total",
            "gw_request_duration_seconds",
            "gw_time_to_first_byte_seconds",
            "gw_inflight_requests",
            "gw_account_cooling",
            "gw_journal_dropped_total",
            "gw_journal_lost_total",
            "gw_build_info",
            "gw_uptime_seconds",
            "gw_metrics_label_overflow_total",
        ] {
            assert!(
                out.contains(&format!("# HELP {family} ")),
                "missing HELP for {family}"
            );
            assert!(
                out.contains(&format!("# TYPE {family} ")),
                "missing TYPE for {family}"
            );
        }
        assert!(out.contains(&format!(
            "gw_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(CONTENT_TYPE.starts_with("text/plain; version=0.0.4"));
    }

    #[test]
    fn writes_are_sorted_regardless_of_order() {
        let m = Metrics::new();
        m.request("zeta", "m", "a", "2xx");
        m.request("alpha", "m", "a", "2xx");
        let out = m.render(&Extra::default());
        let a = out.find("provider=\"alpha\"").unwrap();
        let z = out.find("provider=\"zeta\"").unwrap();
        assert!(a < z);
    }

    #[test]
    fn label_values_are_escaped() {
        let m = Metrics::new();
        m.request("p\"q\\r\ns", "m", "a", "2xx");
        let out = m.render(&Extra::default());
        assert!(out.contains("provider=\"p\\\"q\\\\r\\ns\""), "{out}");
        assert!(out
            .lines()
            .any(|l| l.starts_with("gw_requests_total{provider=")));
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let m = Metrics::new();
        for secs in [0.03, 0.2, 100.0] {
            m.observe_duration("p", secs);
        }
        let out = m.render(&Extra::default());
        assert_eq!(
            value_of(
                &out,
                "gw_request_duration_seconds_bucket{provider=\"p\",le=\"0.05\"}"
            ),
            1.0
        );
        assert_eq!(
            value_of(
                &out,
                "gw_request_duration_seconds_bucket{provider=\"p\",le=\"0.25\"}"
            ),
            2.0
        );
        assert_eq!(
            value_of(
                &out,
                "gw_request_duration_seconds_bucket{provider=\"p\",le=\"+Inf\"}"
            ),
            3.0
        );
        assert_eq!(
            value_of(&out, "gw_request_duration_seconds_count{provider=\"p\"}"),
            3.0
        );
        let sum = value_of(&out, "gw_request_duration_seconds_sum{provider=\"p\"}");
        assert!((sum - 100.23).abs() < 1e-9, "sum {sum}");
        // Every bucket must be present, including ones above the largest sample.
        assert_eq!(
            count_lines(&out, "gw_request_duration_seconds_bucket"),
            BUCKETS.len() + 1
        );
    }

    #[test]
    fn ttfb_is_a_separate_histogram() {
        let m = Metrics::new();
        m.observe_ttfb("p", 0.2);
        let out = m.render(&Extra::default());
        assert!(out.contains("gw_time_to_first_byte_seconds_count{provider=\"p\"} 1"));
        assert!(!out.contains("gw_request_duration_seconds_count{provider=\"p\"}"));
    }

    #[test]
    fn inflight_guard_tracks_lifetime() {
        let m = Metrics::new();
        {
            let _guard = m.inflight();
            let _second = m.inflight();
            assert!(m
                .render(&Extra::default())
                .contains("gw_inflight_requests 2"));
        }
        assert!(m
            .render(&Extra::default())
            .contains("gw_inflight_requests 0"));
    }

    #[test]
    fn cooling_gauge_follows_set_cooling() {
        let m = Metrics::new();
        m.set_cooling("p", "a", true);
        let out = m.render(&Extra::default());
        assert!(out.contains("gw_account_cooling{provider=\"p\",account=\"a\"} 1"));
        m.set_cooling("p", "a", false);
        let out = m.render(&Extra::default());
        assert!(out.contains("gw_account_cooling{provider=\"p\",account=\"a\"} 0"));
    }

    #[test]
    fn label_sets_are_capped_and_counted() {
        let m = Metrics::new();
        for i in 0..MAX_LABEL_SETS + 10 {
            m.request("p", &format!("model-{i}"), "a", "2xx");
        }
        let out = m.render(&Extra::default());
        let lines = count_lines(&out, "gw_requests_total{");
        assert!(lines <= MAX_LABEL_SETS + 1, "{lines} label sets");
        assert!(out.contains("gw_requests_total{provider=\"other\",model=\"other\",account=\"other\",status=\"2xx\"}"));
        assert!(value_of(&out, "gw_metrics_label_overflow_total") >= 1.0);
    }

    #[test]
    fn journal_counts_come_from_extra() {
        let out = Metrics::new().render(&Extra::new(3, 4));
        assert!(out.contains("gw_journal_dropped_total 3"));
        assert!(out.contains("gw_journal_lost_total 4"));
    }

    #[test]
    fn human_formats_scaled_numbers() {
        assert_eq!(human(0), "0");
        assert_eq!(human(999), "999");
        assert_eq!(human(1_200), "1.2k");
        assert_eq!(human(34_000), "34k");
        assert_eq!(human(3_400_000), "3.4M");
        assert_eq!(human(999_999), "1M");
        assert_eq!(human(5_600_000_000), "5.6G");
    }

    fn summary() -> store::UsageSummary {
        store::UsageSummary {
            provider: "openai".into(),
            model: "gpt-5".into(),
            account: "work".into(),
            requests: 12,
            errors: 2,
            input: 34_000,
            cached_read: 1_200,
            cache_write: 0,
            output: 4_500,
            cache_read_share: 0.25,
        }
    }

    #[test]
    fn stats_table_headers_and_rows() {
        let (headers, rows) = stats_table(&[summary()]);
        assert_eq!(headers.len(), 9);
        assert_eq!(headers[8], "cache share");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row[0], "openai");
        assert_eq!(row[1], "gpt-5");
        assert_eq!(row[3], "12");
        assert_eq!(row[5], "34k");
        assert_eq!(row[7], "1.2k");
        assert_eq!(row[8], "25%");
    }

    #[test]
    fn stats_table_is_empty_without_rows() {
        let (headers, rows) = stats_table(&[]);
        assert_eq!(headers.len(), 9);
        assert!(rows.is_empty());
    }
}

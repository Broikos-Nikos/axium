//! Per-turn instrumentation: token counts, cost, tool histogram, wall time.
//!
//! The Python build has had this since its first benchmark. The Rust build
//! reported nothing at a turn boundary, `sonnet.rs` collected `ApiUsage` per
//! API call and then dropped it, so there was no way to price a Rust turn, and
//! a cost comparison between the two implementations would have been fiction.
//!
//! The `Meter` is threaded through the turn and every call and tool is recorded
//! on it, so a benchmark gets cost, latency and behaviour without the agent loop
//! knowing it is being measured.
//!
//! Pricing mirrors `python/axium/pricing.py` exactly. Raw token counts are always
//! kept alongside the derived cost, so a stale price table can be corrected
//! afterwards without re-running (and re-paying for) a benchmark.

use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// USD per 1M tokens: (uncached input, output, cached input).
///
/// `cache` is `None` where the provider publishes no distinct cache-hit rate;
/// the uncached input rate is then used for cached tokens too.
///
/// Sources (fetched 2026-08-06):
///   DeepSeek:  <https://api-docs.deepseek.com/quick_start/pricing>
///   OpenAI:    <https://developers.openai.com/api/docs/pricing>
///   Anthropic: <https://platform.claude.com/docs/en/about-claude/pricing>
const PRICING: &[(&str, f64, f64, Option<f64>)] = &[
    // -- DeepSeek --
    ("deepseek-v4-flash", 0.14, 0.28, Some(0.0028)),
    ("deepseek-v4-pro", 0.435, 0.87, Some(0.003625)),
    // -- OpenAI --
    ("gpt-4.1", 2.00, 8.00, Some(0.50)),
    ("gpt-4.1-mini", 0.40, 1.60, Some(0.10)),
    ("gpt-4.1-nano", 0.10, 0.40, Some(0.025)),
    ("gpt-5.4-mini", 0.75, 4.50, None),
    // -- Anthropic --
    ("claude-haiku-4-5", 1.00, 5.00, Some(0.10)),
    ("claude-haiku-4-5-20251001", 1.00, 5.00, Some(0.10)),
    ("claude-sonnet-4-6", 3.00, 15.00, Some(0.30)),
    ("claude-opus-4-6", 15.00, 75.00, Some(1.50)),
];

/// Anthropic bills a cache WRITE at 1.25x the uncached input rate. Providers that
/// do not report writes pass 0 and the term drops out.
const CACHE_WRITE_MULTIPLIER: f64 = 1.25;

pub fn is_priced(model: &str) -> bool {
    PRICING.iter().any(|(m, ..)| *m == model)
}

/// USD cost of one call. Returns 0.0 for a model with no pricing row.
///
/// An unpriced model still runs but reports `$0.0000`, which silently corrupts
/// every cost comparison, hence `Meter::unpriced_models()`, so a caller can
/// notice rather than trust a suspiciously cheap number.
pub fn cost_usd(
    model: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
    cache_write_tokens: u64,
) -> f64 {
    let Some((_, rate_in, rate_out, rate_cache)) = PRICING.iter().find(|(m, ..)| *m == model)
    else {
        return 0.0;
    };
    let cache_rate = rate_cache.unwrap_or(*rate_in);
    let uncached = prompt_tokens.saturating_sub(cached_tokens) as f64;
    (uncached * rate_in
        + cached_tokens as f64 * cache_rate
        + cache_write_tokens as f64 * rate_in * CACHE_WRITE_MULTIPLIER
        + completion_tokens as f64 * rate_out)
        / 1_000_000.0
}

/// One LLM call.
#[derive(Debug, Clone, Serialize, Default)]
pub struct CallRecord {
    /// Which part of the loop made it: primary, continuation, classifier,
    /// compactor, planner, facts, journal, review, heartbeat, distill.
    pub role: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub latency_s: f64,
    pub cost_usd: f64,
    pub priced: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolRecord {
    pub name: String,
    pub ok: bool,
    pub duration_s: f64,
    pub output_len: usize,
}

/// Per-role totals. Which roles cost what is the evidence that cheap routing
/// pays for itself, so it is broken out rather than summed away.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct RoleTotals {
    pub calls: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

/// What a turn cost and what it did. Serialised straight into a benchmark row.
///
/// Field names and shape match `python/axium/metrics.py::Meter.totals()`
/// exactly. `bench.report` reads rows by key, and a Rust row spelled differently
/// would either crash the report or, worse, read as zero and look free.
#[derive(Debug, Clone, Serialize, Default)]
pub struct TurnMetrics {
    pub llm_calls: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Not reported separately by the providers this build talks to; kept so the
    /// row shape matches the Python one.
    pub reasoning_tokens: u64,
    pub cache_hit_rate: f64,
    pub cost_usd: f64,
    /// Models seen with no pricing row. A non-empty list means `cost_usd` is a
    /// lower bound, not a cost.
    pub unpriced_models: Vec<String>,
    pub api_latency_s: f64,
    pub wall_s: f64,
    pub tool_calls: usize,
    pub tool_errors: u64,
    pub api_errors: u64,
    pub retries: u64,
    /// Tool name → call count. `BTreeMap` so two runs of the same scenario
    /// produce byte-identical JSON and a diff shows real changes only.
    pub tool_histogram: BTreeMap<String, usize>,
    pub by_role: BTreeMap<String, RoleTotals>,
    /// Named counters: compactions, facts_learned, trivial_shortcut, ...
    pub events: BTreeMap<String, u64>,
    /// Every API error message, verbatim. Extra over the Python shape; a
    /// benchmark row that says "4 api_errors" without saying which is useless.
    pub errors: Vec<String>,
}

pub struct Meter {
    calls: Vec<CallRecord>,
    tools: Vec<ToolRecord>,
    events: BTreeMap<String, u64>,
    started: Instant,
}

impl Default for Meter {
    fn default() -> Self {
        Self::new()
    }
}

impl Meter {
    pub fn new() -> Self {
        Self {
            calls: Vec::new(),
            tools: Vec::new(),
            events: BTreeMap::new(),
            started: Instant::now(),
        }
    }

    /// Record one LLM call. `role` is what makes the cost split meaningful.
    #[allow(clippy::too_many_arguments)]
    pub fn record_call(
        &mut self,
        role: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        latency_s: f64,
        error: Option<String>,
    ) {
        if error.is_some() {
            *self.events.entry("api_errors".into()).or_insert(0) += 1;
        }
        self.calls.push(CallRecord {
            role: role.to_string(),
            model: model.to_string(),
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            latency_s,
            cost_usd: cost_usd(
                model,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
            ),
            priced: is_priced(model),
            error,
        });
    }

    pub fn record_tool(&mut self, name: &str, ok: bool, duration_s: f64, output_len: usize) {
        if !ok {
            *self.events.entry("tool_errors".into()).or_insert(0) += 1;
        }
        self.tools.push(ToolRecord {
            name: name.to_string(),
            ok,
            duration_s,
            output_len,
        });
    }

    pub fn bump(&mut self, event: &str) {
        self.bump_by(event, 1);
    }

    pub fn bump_by(&mut self, event: &str, n: u64) {
        *self.events.entry(event.to_string()).or_insert(0) += n;
    }

    // Read back the raw records. Exercised by this module's tests; a report that
    // wants per-call detail rather than the aggregate would use these.
    #[allow(dead_code)]
    pub fn calls(&self) -> &[CallRecord] {
        &self.calls
    }

    #[allow(dead_code)]
    pub fn tools(&self) -> &[ToolRecord] {
        &self.tools
    }

    pub fn snapshot(&self) -> TurnMetrics {
        let mut histogram: BTreeMap<String, usize> = BTreeMap::new();
        for t in &self.tools {
            *histogram.entry(t.name.clone()).or_insert(0) += 1;
        }
        let mut by_role: BTreeMap<String, RoleTotals> = BTreeMap::new();
        for c in &self.calls {
            let r = by_role.entry(c.role.clone()).or_default();
            r.calls += 1;
            r.input_tokens += c.input_tokens;
            r.output_tokens += c.output_tokens;
            r.cost_usd += c.cost_usd;
        }
        let input: u64 = self.calls.iter().map(|c| c.input_tokens).sum();
        let cached: u64 = self.calls.iter().map(|c| c.cache_read_tokens).sum();
        let mut unpriced: Vec<String> = self
            .calls
            .iter()
            .filter(|c| !c.priced && !c.model.is_empty())
            .map(|c| c.model.clone())
            .collect();
        unpriced.sort();
        unpriced.dedup();

        TurnMetrics {
            llm_calls: self.calls.len(),
            input_tokens: input,
            output_tokens: self.calls.iter().map(|c| c.output_tokens).sum(),
            cache_read_tokens: cached,
            cache_write_tokens: self.calls.iter().map(|c| c.cache_write_tokens).sum(),
            reasoning_tokens: 0,
            cache_hit_rate: if input > 0 {
                ((cached as f64 / input as f64) * 1000.0).round() / 1000.0
            } else {
                0.0
            },
            cost_usd: self.calls.iter().map(|c| c.cost_usd).sum(),
            unpriced_models: unpriced,
            api_latency_s: ((self.calls.iter().map(|c| c.latency_s).sum::<f64>()) * 100.0).round() / 100.0,
            wall_s: (self.started.elapsed().as_millis() as f64) / 1000.0,
            tool_calls: self.tools.len(),
            tool_errors: self.events.get("tool_errors").copied().unwrap_or(0),
            api_errors: self.events.get("api_errors").copied().unwrap_or(0),
            retries: self.events.get("retries").copied().unwrap_or(0),
            tool_histogram: histogram,
            by_role,
            events: self.events.clone(),
            errors: self.calls.iter().filter_map(|c| c.error.clone()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unpriced_model_costs_zero_and_is_reported() {
        let mut m = Meter::new();
        m.record_call("primary", "some-new-model", 1000, 500, 0, 0, 1.0, None);
        let s = m.snapshot();
        assert_eq!(s.cost_usd, 0.0);
        // The whole point: silently reporting $0.0000 would corrupt every
        // comparison, so the caller is told which model was not priced.
        assert_eq!(s.unpriced_models, vec!["some-new-model".to_string()]);
    }

    #[test]
    fn cached_tokens_bill_at_the_cache_rate() {
        // 1M input of which 1M cached, on a model whose cache rate is 100x cheaper.
        let full = cost_usd("deepseek-v4-pro", 1_000_000, 0, 0, 0);
        let cached = cost_usd("deepseek-v4-pro", 1_000_000, 0, 1_000_000, 0);
        assert!((full - 0.435).abs() < 1e-9, "{full}");
        assert!((cached - 0.003625).abs() < 1e-9, "{cached}");
        assert!(cached < full / 100.0, "caching must actually be cheaper");
    }

    #[test]
    fn a_model_without_a_cache_rate_bills_cached_tokens_as_input() {
        let a = cost_usd("gpt-5.4-mini", 1_000_000, 0, 0, 0);
        let b = cost_usd("gpt-5.4-mini", 1_000_000, 0, 1_000_000, 0);
        assert!((a - b).abs() < 1e-12, "no published cache rate means no discount");
    }

    #[test]
    fn cache_writes_bill_above_the_input_rate() {
        let plain = cost_usd("claude-sonnet-4-6", 0, 0, 0, 0);
        let written = cost_usd("claude-sonnet-4-6", 0, 0, 0, 1_000_000);
        assert_eq!(plain, 0.0);
        assert!((written - 3.00 * CACHE_WRITE_MULTIPLIER).abs() < 1e-9, "{written}");
    }

    #[test]
    fn uncached_input_never_goes_negative() {
        // A provider reporting more cached than total tokens must not produce a
        // credit.
        let c = cost_usd("deepseek-v4-pro", 100, 0, 5000, 0);
        assert!(c > 0.0, "{c}");
    }

    #[test]
    fn the_histogram_counts_tools_and_is_ordered() {
        let mut m = Meter::new();
        m.record_tool("read_file", true, 0.1, 100);
        m.record_tool("patch_file", true, 0.2, 50);
        m.record_tool("read_file", true, 0.1, 120);
        let s = m.snapshot();
        assert_eq!(s.tool_calls, 3);
        assert_eq!(s.tool_histogram["read_file"], 2);
        assert_eq!(s.tool_histogram["patch_file"], 1);
        // BTreeMap: stable order, so two runs diff cleanly.
        let keys: Vec<&String> = s.tool_histogram.keys().collect();
        assert_eq!(keys, vec!["patch_file", "read_file"]);
    }

    #[test]
    fn the_cost_split_by_role_adds_up_to_the_total() {
        let mut m = Meter::new();
        m.record_call("primary", "deepseek-v4-pro", 10_000, 1_000, 0, 0, 1.0, None);
        m.record_call("continuation", "deepseek-v4-flash", 20_000, 500, 0, 0, 0.5, None);
        m.record_call("classifier", "deepseek-v4-flash", 500, 20, 0, 0, 0.1, None);
        let s = m.snapshot();
        let sum: f64 = s.by_role.values().map(|r| r.cost_usd).sum();
        assert!((sum - s.cost_usd).abs() < 1e-12);
        // Cheap routing only pays for itself if the cheap roles are actually cheap.
        assert!(s.by_role["continuation"].cost_usd < s.by_role["primary"].cost_usd);
        assert_eq!(s.by_role["primary"].calls, 1);
        assert_eq!(s.by_role["continuation"].input_tokens, 20_000);
    }

    #[test]
    fn failures_are_counted_and_surfaced() {
        let mut m = Meter::new();
        m.record_call("primary", "deepseek-v4-pro", 0, 0, 0, 0, 0.0, Some("429".into()));
        m.record_tool("run_command", false, 0.1, 0);
        let s = m.snapshot();
        assert_eq!(s.events["api_errors"], 1);
        assert_eq!(s.events["tool_errors"], 1);
        assert_eq!(s.errors, vec!["429".to_string()]);
    }

    #[test]
    fn named_counters_accumulate() {
        let mut m = Meter::new();
        m.bump("compactions");
        m.bump("compactions");
        m.bump_by("facts_learned", 3);
        let s = m.snapshot();
        assert_eq!(s.events["compactions"], 2);
        assert_eq!(s.events["facts_learned"], 3);
    }

    #[test]
    fn an_empty_turn_serialises_cleanly() {
        let s = Meter::new().snapshot();
        assert_eq!(s.llm_calls, 0);
        assert_eq!(s.cost_usd, 0.0);
        assert!(s.unpriced_models.is_empty());
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"llm_calls\":0"), "{json}");
    }

    /// The row keys `bench.report` reads. If one goes missing the report either
    /// crashes or silently reads zero, so the shape is pinned here.
    #[test]
    fn snapshot_has_every_key_the_python_totals_have() {
        let mut m = Meter::new();
        m.record_call("primary", "deepseek-v4-pro", 1000, 100, 500, 0, 0.5, None);
        let v = serde_json::to_value(m.snapshot()).unwrap();
        for key in [
            "llm_calls", "input_tokens", "output_tokens", "cache_read_tokens",
            "cache_write_tokens", "reasoning_tokens", "cache_hit_rate", "cost_usd",
            "unpriced_models", "api_latency_s", "wall_s", "tool_calls", "tool_errors",
            "api_errors", "retries", "tool_histogram", "by_role", "events",
        ] {
            assert!(v.get(key).is_some(), "missing key: {key}");
        }
        assert_eq!(v["cache_hit_rate"], 0.5);
        assert_eq!(v["by_role"]["primary"]["calls"], 1);
    }

    #[test]
    fn pricing_table_has_no_duplicate_models() {
        // A duplicate row would make `cost_usd` silently depend on ordering.
        let mut names: Vec<&str> = PRICING.iter().map(|(m, ..)| *m).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total);
    }
}

#[cfg(test)]
mod parity_tests {
    use super::*;

    /// The two implementations must price a turn identically, or the whole
    /// point of running the same scenarios through both is lost: a cost
    /// difference would be a pricing-table difference, not an agent difference.
    ///
    /// The expected values are the output of `python/axium/pricing.py` on the
    /// same inputs, captured rather than re-derived, deriving them here from
    /// the same table this file defines would prove nothing.
    #[test]
    fn pricing_matches_the_python_implementation() {
        let cases: &[(&str, u64, u64, u64, u64, f64)] = &[
            ("deepseek-v4-pro", 1_000_000, 0, 0, 0, 0.435),
            ("deepseek-v4-pro", 1_000_000, 0, 1_000_000, 0, 0.003625),
            ("deepseek-v4-flash", 52_262, 3_693, 38_016, 0, 0.003_134_924_8),
            ("gpt-5.4-mini", 1_000_000, 0, 1_000_000, 0, 0.75),
            ("claude-sonnet-4-6", 0, 0, 0, 1_000_000, 3.75),
            ("claude-haiku-4-5", 12_345, 678, 9_000, 100, 0.007_76),
            ("deepseek-v4-pro", 100, 0, 5_000, 0, 0.000_018_125),
            ("unknown-model", 1_000, 1_000, 0, 0, 0.0),
        ];
        for &(model, inp, out, cached, written, expected) in cases {
            let got = cost_usd(model, inp, out, cached, written);
            assert!(
                (got - expected).abs() < 1e-12,
                "{model}: rust {got} vs python {expected}"
            );
        }
    }
}

/// Shared handle. The turn hands the same meter to the model client, the
/// classifier and the compactor, so one turn's cost is one number.
pub type MeterHandle = Arc<Mutex<Meter>>;

pub fn new_handle() -> MeterHandle {
    Arc::new(Mutex::new(Meter::new()))
}

/// Token counts as reported by whichever provider answered.
///
/// The three wire formats spell these differently, and a benchmark that silently
/// read zeros from one of them would report that provider as free.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

/// Pull usage out of a raw provider response, whichever shape it has.
///
/// Anthropic reports `usage.input_tokens` plus separate cache fields; OpenAI and
/// DeepSeek report `usage.prompt_tokens` / `completion_tokens` with cache hits
/// nested under `prompt_tokens_details`. Missing fields read as zero rather than
/// failing: a metrics gap must not cost the turn.
pub fn usage_from_response(v: &serde_json::Value) -> Usage {
    let u = &v["usage"];
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    Usage {
        input_tokens: if u["input_tokens"].is_u64() { n("input_tokens") } else { n("prompt_tokens") },
        output_tokens: if u["output_tokens"].is_u64() { n("output_tokens") } else { n("completion_tokens") },
        cache_read_tokens: if u["cache_read_input_tokens"].is_u64() {
            n("cache_read_input_tokens")
        } else {
            u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0)
        },
        cache_write_tokens: n("cache_creation_input_tokens"),
    }
}

/// Record one call on a handle that may not exist. Never panics on a poisoned
/// lock: losing metrics is not a reason to lose the turn.
pub fn record(
    meter: Option<&MeterHandle>,
    role: &str,
    model: &str,
    usage: Usage,
    latency_s: f64,
    error: Option<String>,
) {
    if let Some(m) = meter {
        m.lock().unwrap_or_else(|e| e.into_inner()).record_call(
            role,
            model,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            latency_s,
            error,
        );
    }
}

pub fn record_tool(meter: Option<&MeterHandle>, name: &str, ok: bool, duration_s: f64, len: usize) {
    if let Some(m) = meter {
        m.lock().unwrap_or_else(|e| e.into_inner()).record_tool(name, ok, duration_s, len);
    }
}

pub fn bump(meter: Option<&MeterHandle>, event: &str) {
    if let Some(m) = meter {
        m.lock().unwrap_or_else(|e| e.into_inner()).bump(event);
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn anthropic_shape_is_read() {
        let v = serde_json::json!({"usage": {
            "input_tokens": 100, "output_tokens": 20,
            "cache_read_input_tokens": 80, "cache_creation_input_tokens": 5
        }});
        let u = usage_from_response(&v);
        assert_eq!(u, Usage { input_tokens: 100, output_tokens: 20,
                              cache_read_tokens: 80, cache_write_tokens: 5 });
    }

    #[test]
    fn openai_and_deepseek_shape_is_read() {
        let v = serde_json::json!({"usage": {
            "prompt_tokens": 100, "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 64}
        }});
        let u = usage_from_response(&v);
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 20);
        assert_eq!(u.cache_read_tokens, 64, "a missed cache field reads as free");
    }

    #[test]
    fn a_response_with_no_usage_reads_as_zero_not_an_error() {
        assert_eq!(usage_from_response(&serde_json::json!({})), Usage::default());
    }

    #[test]
    fn recording_through_a_missing_handle_is_a_no_op() {
        record(None, "primary", "m", Usage::default(), 0.0, None);
        record_tool(None, "read_file", true, 0.0, 0);
        bump(None, "compactions");
    }

    #[test]
    fn a_poisoned_lock_still_records() {
        let h = new_handle();
        let h2 = h.clone();
        let _ = std::thread::spawn(move || {
            let _g = h2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        record(Some(&h), "primary", "deepseek-v4-pro", Usage { input_tokens: 10, ..Default::default() }, 0.0, None);
        assert_eq!(h.lock().unwrap_or_else(|e| e.into_inner()).calls().len(), 1);
    }
}

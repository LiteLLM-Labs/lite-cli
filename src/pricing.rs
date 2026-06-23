//! Model pricing from LiteLLM's `model_prices_and_context_window.json`.
//!
//! Cost is computed following litellm's `generic_cost_per_token` token path
//! (litellm/litellm_core_utils/llm_cost_calc/utils.py):
//!   - the threshold for tiered ("above_Nk_tokens") pricing is the *total* context,
//!     i.e. input + cache_read + cache_creation (matching anthropic's prompt_tokens);
//!   - when total > threshold, all four rates switch to their `_above_<N>_tokens` variants;
//!   - text (non-cached input) is billed at the input rate, cache reads at the cache-read rate,
//!     cache writes at the cache-creation rate (5m base; the 1h split needs per-token details
//!     that the streamed usage does not carry, so the base rate is used).
//!
//! Fetched once and cached to `~/.lite/model_prices.json`; refreshed when older than 24h.
//! If the network is unavailable, the cached copy (however old) is used.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

const PRICES_URL: &str = "https://raw.githubusercontent.com/BerriAI/litellm/refs/heads/litellm_internal_staging/model_prices_and_context_window.json";
const MAX_CACHE_AGE: Duration = Duration::from_secs(24 * 60 * 60);

const INPUT_KEY: &str = "input_cost_per_token";
const OUTPUT_KEY: &str = "output_cost_per_token";
const CACHE_READ_KEY: &str = "cache_read_input_token_cost";
const CACHE_CREATION_KEY: &str = "cache_creation_input_token_cost";

/// Per-model cost fields (base + any `_above_<N>_tokens` tier variants), as f64 USD/token.
type CostMap = HashMap<String, f64>;

#[derive(Default)]
pub struct Pricing {
    map: HashMap<String, CostMap>,
}

fn cache_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".lite")
        .join("model_prices.json")
}

/// Coerce a JSON value to f64 (litellm tolerates string costs like "3e-7").
fn as_f64(v: &serde_json::Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn parse(bytes: &[u8]) -> HashMap<String, CostMap> {
    let Ok(raw) = serde_json::from_slice::<HashMap<String, serde_json::Value>>(bytes) else {
        return HashMap::new();
    };
    raw.into_iter()
        .filter_map(|(model, entry)| {
            let obj = entry.as_object()?;
            let costs: CostMap = obj
                .iter()
                .filter(|(k, _)| {
                    k.starts_with("input_cost_per_token")
                        || k.starts_with("output_cost_per_token")
                        || k.starts_with("cache_read_input_token_cost")
                        || k.starts_with("cache_creation_input_token_cost")
                })
                .filter_map(|(k, v)| as_f64(v).map(|n| (k.clone(), n)))
                .collect();
            if costs.is_empty() {
                None
            } else {
                Some((model, costs))
            }
        })
        .collect()
}

/// Parse the token threshold encoded in a key like `input_cost_per_token_above_200k_tokens`.
fn parse_threshold(key: &str) -> Option<u64> {
    let after = key.split("_above_").nth(1)?;
    let num = after.split("_tokens").next()?;
    if let Some(k) = num.strip_suffix('k') {
        k.parse::<f64>().ok().map(|n| (n * 1000.0) as u64)
    } else {
        num.parse::<u64>().ok()
    }
}

impl Pricing {
    /// Load pricing: fresh cache, else fetch + cache, else stale cache, else empty.
    pub async fn load() -> Self {
        let path = cache_path();
        let fresh = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or(MAX_CACHE_AGE) < MAX_CACHE_AGE)
            .unwrap_or(false);

        if fresh {
            if let Ok(bytes) = std::fs::read(&path) {
                return Self { map: parse(&bytes) };
            }
        }

        if let Ok(resp) = reqwest::get(PRICES_URL).await {
            if let Ok(bytes) = resp.bytes().await {
                let map = parse(&bytes);
                if !map.is_empty() {
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(&path, &bytes);
                    return Self { map };
                }
            }
        }

        if let Ok(bytes) = std::fs::read(&path) {
            return Self { map: parse(&bytes) };
        }
        Self::default()
    }

    /// Look up a model's cost map, tolerating provider prefixes (`anthropic/claude-opus-4-8`).
    fn lookup(&self, model: &str) -> Option<&CostMap> {
        if let Some(c) = self.map.get(model) {
            return Some(c);
        }
        model.rsplit_once('/').and_then(|(_, rest)| self.map.get(rest))
    }

    /// Total USD cost for one request, following litellm's `generic_cost_per_token` token path.
    pub fn cost(
        &self,
        model: Option<&str>,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    ) -> f64 {
        let Some(costs) = model.and_then(|m| self.lookup(m)) else {
            return 0.0;
        };

        // Threshold basis = total context (matches anthropic's prompt_tokens).
        let total = input_tokens + cache_read_tokens + cache_creation_tokens;
        let rate = self.rate_resolver(costs, total, None);

        input_tokens as f64 * rate(INPUT_KEY)
            + output_tokens as f64 * rate(OUTPUT_KEY)
            + cache_read_tokens as f64 * rate(CACHE_READ_KEY)
            + cache_creation_tokens as f64 * rate(CACHE_CREATION_KEY)
    }

    /// Like `cost`, but using the richer transcript data: service tier and the 5m/1h
    /// cache-creation split (litellm bills 5m at the base write rate, 1h at `_above_1hr`).
    pub fn cost_detailed(
        &self,
        model: Option<&str>,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_5m: u64,
        cache_creation_1h: u64,
        service_tier: Option<&str>,
    ) -> f64 {
        let Some(costs) = model.and_then(|m| self.lookup(m)) else {
            return 0.0;
        };
        let total = input_tokens + cache_read_tokens + cache_creation_5m + cache_creation_1h;
        let rate = self.rate_resolver(costs, total, service_tier);

        input_tokens as f64 * rate(INPUT_KEY)
            + output_tokens as f64 * rate(OUTPUT_KEY)
            + cache_read_tokens as f64 * rate(CACHE_READ_KEY)
            + cache_creation_5m as f64 * rate(CACHE_CREATION_KEY)
            + cache_creation_1h as f64 * rate("cache_creation_input_token_cost_above_1hr")
    }

    /// Build a rate lookup for a request: resolves base keys against the matching long-context
    /// tier (`_above_<N>_tokens`) and service tier (`_flex`/`_priority`), most-specific first.
    fn rate_resolver<'a>(
        &self,
        costs: &'a CostMap,
        total: u64,
        service_tier: Option<&str>,
    ) -> impl Fn(&str) -> f64 + 'a {
        // Highest threshold the total exceeds; keep that key's exact `_above_..._tokens` suffix.
        let thr: Option<String> = costs
            .keys()
            .filter_map(|k| {
                k.strip_prefix(INPUT_KEY)
                    .filter(|s| s.starts_with("_above_"))
                    .and_then(|s| parse_threshold(k).map(|t| (t, s.to_string())))
            })
            .filter(|(t, _)| total > *t)
            .max_by_key(|(t, _)| *t)
            .map(|(_, suf)| suf);

        // Only flex/priority have dedicated keys; standard/unknown fall back to base.
        let tier: Option<String> = service_tier
            .filter(|t| *t == "flex" || *t == "priority")
            .map(|t| format!("_{t}"));

        move |base: &str| -> f64 {
            // Most specific → least: tier+threshold, threshold, tier, base.
            if let (Some(t), Some(s)) = (&tier, &thr) {
                if let Some(v) = costs.get(&format!("{base}{t}{s}")) {
                    return *v;
                }
            }
            if let Some(s) = &thr {
                if let Some(v) = costs.get(&format!("{base}{s}")) {
                    return *v;
                }
            }
            if let Some(t) = &tier {
                if let Some(v) = costs.get(&format!("{base}{t}")) {
                    return *v;
                }
            }
            costs.get(base).copied().unwrap_or(0.0)
        }
    }

    /// USD saved by cache reads vs paying the full input rate for those tokens.
    /// (Uses base rates — a savings estimate, not tier-adjusted.)
    pub fn cache_savings(&self, model: Option<&str>, cache_read_tokens: u64) -> f64 {
        let Some(costs) = model.and_then(|m| self.lookup(m)) else {
            return 0.0;
        };
        let input = costs.get(INPUT_KEY).copied().unwrap_or(0.0);
        let cached = costs.get(CACHE_READ_KEY).copied().unwrap_or(0.0);
        (cache_read_tokens as f64 * (input - cached)).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricing(model: &str, pairs: &[(&str, f64)]) -> Pricing {
        let costs: CostMap = pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        Pricing {
            map: HashMap::from([(model.to_string(), costs)]),
        }
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
    }

    #[test]
    fn untiered_with_cache_creation() {
        // claude-opus-4-8 (no thresholds).
        let p = pricing(
            "claude-opus-4-8",
            &[
                ("input_cost_per_token", 5e-6),
                ("output_cost_per_token", 2.5e-5),
                ("cache_read_input_token_cost", 5e-7),
                ("cache_creation_input_token_cost", 6.25e-6),
            ],
        );
        // 7839*5e-6 + 4*2.5e-5 + 31747*6.25e-6
        approx(p.cost(Some("claude-opus-4-8"), 7839, 4, 0, 31747), 0.23771375);
    }

    #[test]
    fn tiered_below_and_above_threshold() {
        let p = pricing(
            "claude-sonnet-4-5",
            &[
                ("input_cost_per_token", 3e-6),
                ("output_cost_per_token", 1.5e-5),
                ("cache_read_input_token_cost", 3e-7),
                ("cache_creation_input_token_cost", 3.75e-6),
                ("input_cost_per_token_above_200k_tokens", 6e-6),
                ("output_cost_per_token_above_200k_tokens", 2.25e-5),
                ("cache_read_input_token_cost_above_200k_tokens", 6e-7),
                ("cache_creation_input_token_cost_above_200k_tokens", 7.5e-6),
            ],
        );
        // Below: 1000*3e-6 + 500*1.5e-5
        approx(p.cost(Some("claude-sonnet-4-5"), 1000, 500, 0, 0), 0.0105);
        // Above (input alone > 200k): 210000*6e-6 + 1000*2.25e-5
        approx(p.cost(Some("claude-sonnet-4-5"), 210000, 1000, 0, 0), 1.2825);
        // Threshold basis includes cache: 10000 + 195000 = 205000 > 200k -> above rates.
        // 10000*6e-6 + 195000*6e-7
        approx(p.cost(Some("claude-sonnet-4-5"), 10000, 0, 195000, 0), 0.177);
    }

    #[test]
    fn unknown_model_is_zero() {
        let p = pricing("x", &[("input_cost_per_token", 1.0)]);
        approx(p.cost(Some("nope"), 100, 100, 0, 0), 0.0);
        approx(p.cost(None, 100, 100, 0, 0), 0.0);
    }

    #[test]
    fn provider_prefix_fallback() {
        let p = pricing("claude-opus-4-8", &[("input_cost_per_token", 5e-6)]);
        approx(p.cost(Some("anthropic/claude-opus-4-8"), 1000, 0, 0, 0), 0.005);
    }
}

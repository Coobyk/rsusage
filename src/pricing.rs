//! Model pricing: models.dev primary, LiteLLM fallback — matching ccusage.
//! Fetched at runtime, cached under XDG cache, `--offline` reads cache only.
use std::path::PathBuf;

const MODELSDEV_URL: &str = "https://models.dev/api.json";
const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const CACHE_MAX_AGE_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, Default)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_creation: f64,
    pub cache_read: f64,
}

impl Price {
    pub fn is_free(&self) -> bool {
        self.input == 0.0 && self.output == 0.0
            && self.cache_creation == 0.0 && self.cache_read == 0.0
    }
}

#[derive(Default)]
pub struct Pricing {
    /// normalized model id -> price per token
    map: std::collections::HashMap<String, Price>,
    /// memoized per-model resolution (exact + fuzzy) — misses are common
    cache: std::cell::RefCell<std::collections::HashMap<String, Option<Price>>>,
}

fn cache_dir() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::agents::home().join(".cache"))
        .join("rsusage")
}

fn fetch_cached(url: &str, file: &str, offline: bool) -> Option<String> {
    let path = cache_dir().join(file);
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|e| e.as_secs() < CACHE_MAX_AGE_SECS);
    if !offline && !fresh {
        if let Ok(text) = ureq::get(url)
            .call()
            .and_then(|mut r| r.body_mut().read_to_string())
        {
            if serde_json::from_str::<serde_json::Value>(&text).is_ok() {
                let _ = std::fs::create_dir_all(cache_dir());
                let _ = std::fs::write(&path, &text);
            }
        }
    }
    std::fs::read_to_string(&path).ok()
}

/// Normalize a model id for lookup: lowercase, strip provider prefix,
/// strip trailing -YYYYMMDD date.
fn norm(model: &str) -> String {
    let lower = model.to_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    match base.rsplit_once('-') {
        Some((b, suf)) if suf.len() == 8 && suf.chars().all(|c| c.is_ascii_digit()) => {
            b.to_string()
        }
        _ => base.to_string(),
    }
}

/// Insert into map only if absent (first writer wins).
fn put(
    map: &mut std::collections::HashMap<String, Price>,
    key: String,
    p: Price,
) {
    map.entry(key).or_insert(p);
}

impl Pricing {
    pub fn load(offline: bool) -> Self {
        let mut map = std::collections::HashMap::new();
        // models.dev on top (per-1M-token costs -> per-token).
        // Canonical providers first so their prices win over resellers
        // (bedrock/openrouter/... markup).
        const PRIORITY: &[&str] = &[
            "anthropic", "openai", "xai", "google", "z-ai", "zai-org", "zai",
            "opencode", "kilo", "gemini", "deepseek", "moonshot", "qwen",
        ];
        if let Some(text) = fetch_cached(MODELSDEV_URL, "modelsdev_prices.json", offline) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(providers) = v.as_object() {
                    let mut order: Vec<&String> = providers.keys().collect();
                    order.sort_by_key(|name| {
                        PRIORITY
                            .iter()
                            .position(|p| p == name)
                            .unwrap_or(PRIORITY.len())
                    });
                    for prov in order {
                        let pv = &providers[prov];
                        let Some(models) = pv.get("models").and_then(|m| m.as_object()) else {
                            continue;
                        };
                        let _ = prov;
                        for (name, mv) in models {
                            let Some(c) = mv.get("cost") else { continue };
                            let g = |k: &str| {
                                c.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0) / 1e6
                            };
                            let p = Price {
                                input: g("input"),
                                output: g("output"),
                                cache_creation: g("cache_write"),
                                cache_read: g("cache_read"),
                            };
                            // insert both raw and normalized keys
                            put(&mut map, name.to_lowercase(), p.clone());
                            put(&mut map, norm(name), p);
                        }
                    }
                }
            }
        }
        // LiteLLM first (fallback: only fills keys models.dev doesn't cover)
        if let Some(text) = fetch_cached(LITELLM_URL, "litellm_prices.json", offline) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(obj) = v.as_object() {
                    for (name, p) in obj {
                        let g = |k: &str| p.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
                        put(
                            &mut map,
                            norm(name),
                            Price {
                                input: g("input_cost_per_token"),
                                output: g("output_cost_per_token"),
                                cache_creation: g("cache_creation_input_token_cost"),
                                cache_read: g("cache_read_input_token_cost"),
                            },
                        );
                    }
                }
            }
        }
        Self { map, cache: std::cell::RefCell::new(std::collections::HashMap::new()) }
    }

    pub fn get(&self, model: &str) -> Option<Price> {
        let key = norm(model);
        if let Some(p) = self.cache.borrow().get(&key) {
            return p.clone();
        }
        let resolved = if let Some(p) = self.map.get(&key) {
            Some(p.clone())
        } else {
            // secondary: containment match (e.g. "claude-sonnet-4-5" inside a longer id)
            let mut best: Option<&Price> = None;
            for (k, p) in &self.map {
                if (k.contains(&key) || key.contains(k.as_str())) && best.is_none() {
                    best = Some(p);
                }
            }
            best.cloned()
        };
        self.cache.borrow_mut().insert(key, resolved.clone());
        resolved
    }

    pub fn cost(&self, model: &str, input: u64, output: u64, cc: u64, cr: u64) -> Option<f64> {
        self.get(model).map(|p| {
            input as f64 * p.input
                + output as f64 * p.output
                + cc as f64 * p.cache_creation
                + cr as f64 * p.cache_read
        })
    }

    pub fn entry_cost(&self, e: &crate::types::UsageEntry) -> Option<f64> {
        self.cost(&e.model, e.input, e.output, e.cache_creation, e.cache_read)
    }
}

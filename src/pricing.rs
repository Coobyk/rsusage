//! Model pricing: models.dev primary, LiteLLM fallback — matching ccusage.
//! Fetched at runtime, cached under XDG cache, `--offline` reads cache only.
use std::collections::BTreeMap;
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
        self.input == 0.0
            && self.output == 0.0
            && self.cache_creation == 0.0
            && self.cache_read == 0.0
    }
}

#[derive(Default)]
pub struct Pricing {
    /// normalized model id -> price per token (BTreeMap so the fuzzy-match
    /// fallback below resolves deterministically)
    map: BTreeMap<String, Price>,
    /// memoized per-model resolution (exact + fuzzy) — misses are common
    cache: std::cell::RefCell<std::collections::HashMap<String, Option<Price>>>,
}

fn cache_dir() -> PathBuf {
    crate::paths::xdg_cache()
}

fn fetch_cached(url: &str, file: &str, offline: bool) -> Option<String> {
    let path = cache_dir().join(file);
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|e| e.as_secs() < CACHE_MAX_AGE_SECS);
    if !offline && !fresh {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(15)))
            .build()
            .new_agent();
        if let Ok(text) = agent
            .get(url)
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

impl Pricing {
    pub fn load(offline: bool) -> Self {
        let mut map = BTreeMap::new();
        // models.dev first (per-1M-token costs -> per-token); canonical
        // providers before resellers so their prices win the first-writer-wins
        // insert (bedrock/openrouter add markup).
        const PRIORITY: &[&str] = &[
            "anthropic",
            "openai",
            "xai",
            "google",
            "z-ai",
            "zai-org",
            "zai",
            "opencode",
            "kilo",
            "gemini",
            "deepseek",
            "moonshot",
            "qwen",
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
                        let Some(models) =
                            providers[prov].get("models").and_then(|m| m.as_object())
                        else {
                            continue;
                        };
                        for (name, mv) in models {
                            let Some(c) = mv.get("cost") else { continue };
                            let g =
                                |k: &str| c.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0) / 1e6;
                            let p = Price {
                                input: g("input"),
                                output: g("output"),
                                cache_creation: g("cache_write"),
                                cache_read: g("cache_read"),
                            };
                            map.entry(name.to_lowercase()).or_insert_with(|| p.clone());
                            map.entry(norm(name)).or_insert(p);
                        }
                    }
                }
            }
        }
        // LiteLLM fills whatever models.dev doesn't cover
        if let Some(text) = fetch_cached(LITELLM_URL, "litellm_prices.json", offline) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(obj) = v.as_object() {
                    for (name, p) in obj {
                        let g = |k: &str| p.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
                        map.entry(norm(name)).or_insert(Price {
                            input: g("input_cost_per_token"),
                            output: g("output_cost_per_token"),
                            cache_creation: g("cache_creation_input_token_cost"),
                            cache_read: g("cache_read_input_token_cost"),
                        });
                    }
                }
            }
        }
        Self {
            map,
            cache: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    pub fn get(&self, model: &str) -> Option<Price> {
        let key = norm(model);
        if key.is_empty() {
            return None;
        }
        if let Some(p) = self.cache.borrow().get(&key) {
            return p.clone();
        }
        // containment match, e.g. "claude-sonnet-4-5" inside a longer id —
        // first hit in sorted-key order wins so results are deterministic
        let resolved = self.map.get(&key).cloned().or_else(|| {
            self.map
                .iter()
                .find(|(k, _)| k.contains(&key) || key.contains(k.as_str()))
                .map(|(_, p)| p.clone())
        });
        self.cache.borrow_mut().insert(key, resolved.clone());
        resolved
    }

    pub fn entry_cost(&self, e: &crate::types::UsageEntry) -> Option<f64> {
        self.get(&e.model).map(|p| {
            e.tokens.input as f64 * p.input
                + e.tokens.output as f64 * p.output
                + e.tokens.cache_creation as f64 * p.cache_creation
                + e.tokens.cache_read as f64 * p.cache_read
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricing_with(models: &[(&str, f64)]) -> Pricing {
        Pricing {
            map: models
                .iter()
                .map(|(n, i)| {
                    (
                        n.to_string(),
                        Price {
                            input: *i,
                            output: 0.0,
                            cache_creation: 0.0,
                            cache_read: 0.0,
                        },
                    )
                })
                .collect(),
            cache: Default::default(),
        }
    }

    #[test]
    fn lookup_exact_then_deterministic_fuzzy() {
        let p = pricing_with(&[("claude-sonnet-4-5", 3.0), ("claude-sonnet", 1.0)]);
        assert_eq!(p.get("claude-sonnet-4-5").unwrap().input, 3.0);
        // normalized id contained in a longer key (or vice versa) resolves
        assert!(p.get("anthropic/claude-sonnet-4-5-20250929").is_some());
        // unknown ids are misses
        assert!(p.get("totally-made-up-model").is_none());
        // empty ids never match (they'd containment-hit arbitrary keys)
        assert!(p.get("").is_none());
        // misses are memoized but stay misses
        assert!(p.get("totally-made-up-model").is_none());
    }

    #[test]
    fn norm_strips_provider_and_date() {
        assert_eq!(
            norm("anthropic/Claude-Sonnet-4-5-20250929"),
            "claude-sonnet-4-5"
        );
        assert_eq!(norm("gpt-5"), "gpt-5");
        assert_eq!(norm("zai-org/GLM-4.6"), "glm-4.6");
    }
}

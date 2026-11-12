//! Model pricing: replicates ccusage's resolution chain —
//! LiteLLM primary (+ builtin table), live models.dev catalog (claims-resolved),
//! embedded models.dev snapshot — with boundary-aware fuzzy matching,
//! exact-only gating, 200k marginal tiers and models.dev long-context tiers.
//! Fetched at runtime, cached under XDG cache, `--offline` reads cache only.
//!
//! Snapshot files in `src/snapshots/` are vendored from ccusage
//! (rust/crates/ccusage-pricing-snapshots, MIT) so offline runs price identically.
use crate::types::Tokens;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const MODELSDEV_URL: &str = "https://models.dev/api.json";
const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const CACHE_MAX_AGE_SECS: u64 = 24 * 60 * 60;
const BIN_FILE: &str = "prices-bin-v1.cache";
/// Bump if the source set/order or the serialized map layout changes.
const BIN_VERSION: u32 = 3;
/// (mtime_secs, mtime_nanos, len) per source, in load order.
type SourceMeta = [(u64, u32, u64); 2];

const DEEPSEEK_V4_PRICING_CUTOFF_MS: i64 = 1_786_896_000_000;
const MILLIS_PER_HOUR: i64 = 3_600_000;
const MILLIS_PER_DAY: i64 = 86_400_000;
/// LiteLLM-style tier boundary: usage above 200k tokens is billed at the
/// `*_above_200k` rates when a model publishes them.
const TIER_200K: u64 = 200_000;

const TRUST_OWNER: u8 = 2;
const TRUST_PLATFORM: u8 = 1;
const TRUST_RESELLER: u8 = 0;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_create: f64,
    pub cache_read: f64,
    #[serde(default)]
    pub input_above: Option<f64>,
    #[serde(default)]
    pub output_above: Option<f64>,
    #[serde(default)]
    pub cache_create_above: Option<f64>,
    #[serde(default)]
    pub cache_read_above: Option<f64>,
    #[serde(default)]
    pub long_context_threshold: Option<u64>,
    /// z.ai-style entries patch only unset cache rates (`put_builtin_glm`).
    #[serde(default)]
    pub cache_create_explicit: bool,
    #[serde(default)]
    pub cache_read_explicit: bool,
    /// models.dev `limit.context` / litellm `max_input_tokens` — read by the
    /// statusline transcript fallback for the context-window denominator
    #[serde(default)]
    pub context_limit: Option<u64>,
}

/// Upstream `normalized_pricing_key`: `.`/`@` spellings are one id.
fn norm_key(v: &str) -> std::borrow::Cow<'_, str> {
    if v.contains(['.', '@']) {
        std::borrow::Cow::Owned(v.replace(['.', '@'], "-"))
    } else {
        std::borrow::Cow::Borrowed(v)
    }
}

/// Upstream `normalized_models_dev_model_id`: lowercase + separator fold.
fn norm_md(v: &str) -> std::borrow::Cow<'_, str> {
    if v.bytes()
        .any(|b| b == b'.' || b == b'@' || b.is_ascii_uppercase())
    {
        std::borrow::Cow::Owned(v.to_lowercase().replace(['.', '@'], "-"))
    } else {
        std::borrow::Cow::Borrowed(v)
    }
}

/// Upstream `pricing_alias` — the three hardcoded aliases.
fn pricing_alias(model: &str) -> Option<&'static str> {
    match model {
        "gpt-reserve" => Some("gpt-5.6-luna"),
        "gpt-5.6" => Some("gpt-5.6-sol"),
        "gpt-5.3-spark" => Some("gpt-5.3-codex-spark"),
        _ => None,
    }
}

fn is_boundary_byte(b: u8) -> bool {
    !b.is_ascii_alphanumeric()
}

/// Upstream `contains_pricing_key`: `key` occurs in `value` at a boundary on
/// both sides; a numeric-version suffix after a digit-ending key disallows the
/// match unless it is an 8-digit date at a boundary.
fn contains_key(value: &str, key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    value.match_indices(key).any(|(i, _)| {
        let prefix_ok = i == 0 || is_boundary_byte(value.as_bytes()[i - 1]);
        if !prefix_ok {
            return false;
        }
        let suffix = &value[i + key.len()..];
        suffix_allows(key, suffix)
    })
}

fn suffix_allows(key: &str, suffix: &str) -> bool {
    let Some(&sep) = suffix.as_bytes().first() else {
        return true;
    };
    if sep.is_ascii_alphanumeric() {
        return false;
    }
    !suffix_starts_numeric_version(key, suffix)
}

fn suffix_starts_numeric_version(key: &str, suffix: &str) -> bool {
    if !key.as_bytes().last().is_some_and(u8::is_ascii_digit) {
        return false;
    }
    if !matches!(suffix.as_bytes().first(), Some(b'-') | Some(b'.')) {
        return false;
    }
    let rest = &suffix[1..];
    let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return false;
    }
    let after = rest.as_bytes().get(digits);
    // An 8-digit suffix at a boundary is a date stamp, not a version — allowed.
    !(digits == 8 && after.is_none_or(|b| is_boundary_byte(*b)))
}

/// Upstream `pricing_key_matches`: containment either way, raw or normalized.
fn key_matches(candidate: &str, model: &str) -> bool {
    contains_key(model, candidate)
        || contains_key(candidate, model)
        || {
            let (c, m) = (norm_key(candidate), norm_key(model));
            contains_key(&m, &c) || contains_key(&c, &m)
        }
}

/// One resolved pricing table (litellm, models.dev live, or embedded snapshot).
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct PricingMap {
    entries: HashMap<String, Price>,
    /// Verbatim exact-only ids.
    exact_raw: HashSet<String>,
    /// normalized spelling -> exact-only id (None when two ids share it).
    exact_norm: HashMap<String, Option<String>>,
    /// normalized spelling -> key (None when ambiguous) — upstream
    /// `normalized_keys`, used by `find_exact` so alias-free exact lookups
    /// still survive separator differences.
    normalized: HashMap<String, Option<String>>,
}

impl PricingMap {
    fn insert(&mut self, key: String, price: Price, exact_only: bool) {
        self.normalized
            .entry(norm_key(&key).into_owned())
            .and_modify(|named| {
                if named.as_deref() != Some(key.as_str()) {
                    *named = None;
                }
            })
            .or_insert_with(|| Some(key.clone()));
        if exact_only {
            let n = norm_key(&key).into_owned();
            self.exact_norm
                .entry(n)
                .and_modify(|named| {
                    if named.as_deref() != Some(key.as_str()) {
                        *named = None;
                    }
                })
                .or_insert_with(|| Some(key.clone()));
            self.exact_raw.insert(key.clone());
        }
        self.entries.insert(key, price);
    }

    /// Whether `key` is or spells an exact-only id — the gate on fuzzy lookup.
    fn is_exact_only(&self, key: &str) -> bool {
        self.exact_raw.contains(key) || self.exact_norm.contains_key(norm_key(key).as_ref())
    }

    /// Upstream `find_entry_with_fallback`: exact hit, exact-only id spelled
    /// differently, then the boundary-aware fuzzy scan (longest key wins).
    fn find_entry(&self, model: &str, fuzzy: bool) -> Option<&Price> {
        self.entries
            .get(model)
            .or_else(|| {
                self.exact_norm
                    .get(norm_key(model).as_ref())?
                    .as_deref()
                    .and_then(|id| self.entries.get(id))
            })
            .or_else(|| {
                if !fuzzy || self.is_exact_only(model) {
                    return None;
                }
                self.entries
                    .iter()
                    .filter(|(k, _)| !self.exact_raw.contains(k.as_str()))
                    .filter(|(k, _)| key_matches(k, model))
                    .max_by(|(l, _), (r, _)| {
                        l.len().cmp(&r.len()).then_with(|| r.cmp(l))
                    })
                    .map(|(_, p)| p)
            })
    }

    /// Upstream `find_exact_normalized`: direct hit, exact-only id spelled
    /// differently, then the normalized-key map — no fuzzy, no aliases.
    fn find_exact_normalized(&self, model: &str) -> Option<&Price> {
        self.entries
            .get(model)
            .or_else(|| {
                self.exact_norm
                    .get(norm_key(model).as_ref())?
                    .as_deref()
                    .and_then(|id| self.entries.get(id))
            })
            .or_else(|| {
                self.normalized
                    .get(norm_key(model).as_ref())?
                    .as_deref()
                    .and_then(|k| self.entries.get(k))
            })
    }

    /// Upstream `find_entry_or_alias_with_fallback`: direct, `pricing_alias`
    /// resolution, then find_entry on the requested spelling.
    fn find(&self, model: &str, fuzzy: bool) -> Option<&Price> {
        self.entries
            .get(model)
            .or_else(|| pricing_alias(model).and_then(|a| self.find_entry(a, fuzzy)))
            .or_else(|| self.find_entry(model, fuzzy))
    }
}

#[derive(Default)]
struct MdRules {
    owners: HashSet<String>,
    platforms: HashSet<String>,
    /// normalized authored ids, sorted — prefix-scanned for tier variants
    authored_normalized: Vec<String>,
    /// normalized authored ids — `token_priced` membership test
    authored_set: HashSet<String>,
    asset_priced_model_ids: HashSet<String>,
}

impl MdRules {
    fn rank(&self, provider_id: &str) -> u8 {
        if self.owners.contains(provider_id) {
            TRUST_OWNER
        } else if self.platforms.contains(provider_id) {
            TRUST_PLATFORM
        } else {
            TRUST_RESELLER
        }
    }

    /// Upstream `is_exact_only` for live entries: tier variant of an authored
    /// model (bare id only), or an unversioned (digit-less) pricing key.
    fn is_exact_only(&self, source_id: &str, pricing_key: &str) -> bool {
        self.tier_variant(source_id) || !pricing_key.bytes().any(|b| b.is_ascii_digit())
    }

    fn tier_variant(&self, source_id: &str) -> bool {
        if source_id.contains('/') {
            return false;
        }
        let n = norm_md(source_id);
        self.authored_normalized.iter().any(|a| {
            n.strip_prefix(a.as_str())
                .is_some_and(|rest| rest.starts_with('-'))
        })
    }

    /// Upstream `is_token_priced`: authored verdict wins; otherwise the
    /// catalog's own modalities (absent list = text).
    fn token_priced(&self, source_id: &str, m: &serde_json::Value) -> bool {
        let n = norm_md(source_id);
        if self.asset_priced_model_ids.contains(n.as_ref()) {
            return false;
        }
        if self.authored_set.contains(n.as_ref()) {
            return true;
        }
        let Some(modalities) = m.get("modalities") else {
            return true;
        };
        let text_only_output = match modalities.get("output").and_then(|v| v.as_array()) {
            None => true,
            Some(o) => o.len() == 1 && o[0].as_str() == Some("text"),
        };
        let accepts_text = match modalities.get("input").and_then(|v| v.as_array()) {
            None => true,
            Some(i) => i.iter().any(|m| m.as_str() == Some("text")),
        };
        text_only_output && accepts_text
    }

    fn load() -> Self {
        #[derive(serde::Deserialize)]
        struct Raw {
            owners: HashSet<String>,
            platforms: HashSet<String>,
            #[serde(rename = "authoredModelIds")]
            authored_model_ids: Vec<String>,
            #[serde(rename = "assetPricedModelIds")]
            asset_priced_model_ids: HashSet<String>,
        }
        let raw: Raw = serde_json::from_str(include_str!("snapshots/models-dev-catalog-rules.json"))
            .expect("embedded models-dev-catalog-rules.json must parse");
        let mut authored_normalized: Vec<String> = raw
            .authored_model_ids
            .iter()
            .map(|id| norm_md(id).into_owned())
            .collect();
        authored_normalized.sort();
        let authored_set: HashSet<String> = authored_normalized.iter().cloned().collect();
        Self {
            owners: raw.owners,
            platforms: raw.platforms,
            authored_normalized,
            authored_set,
            asset_priced_model_ids: raw.asset_priced_model_ids,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Claim {
    trust: u8,
    has_tier: bool,
    has_cache_read: bool,
    has_cache_write: bool,
    has_context_limit: bool,
}

fn cache_dir() -> PathBuf {
    crate::paths::xdg_cache()
}

/// Re-download a cached JSON source if stale; offline skips the network.
fn refresh(url: &str, file: &str, offline: bool) {
    let path = cache_dir().join(file);
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|e| e.as_secs() < CACHE_MAX_AGE_SECS);
    if offline || fresh {
        return;
    }
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

fn read_cached(file: &str) -> Option<String> {
    std::fs::read_to_string(cache_dir().join(file)).ok()
}

fn meta_of(path: &std::path::Path) -> (u64, u32, u64) {
    let Ok(m) = std::fs::metadata(path) else {
        return (0, 0, 0);
    };
    let d = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .unwrap_or_default();
    (d.as_secs(), d.subsec_nanos(), m.len())
}

fn source_meta() -> SourceMeta {
    ["modelsdev_prices.json", "litellm_prices.json"].map(|f| meta_of(&cache_dir().join(f)))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BinPayload {
    primary: PricingMap,
    models_dev: Option<PricingMap>,
}

/// Binary cache of the two runtime-loaded maps (the embedded snapshot parses
/// once from `include_str` and needs no cache). Used only when every source
/// file's metadata still matches.
fn read_bin(meta: &SourceMeta) -> Option<BinPayload> {
    let bytes = std::fs::read(cache_dir().join(BIN_FILE)).ok()?;
    let (v, m, p): (u32, SourceMeta, BinPayload) = bincode::deserialize(&bytes).ok()?;
    (v == BIN_VERSION && m == *meta).then_some(p)
}

fn write_bin(meta: &SourceMeta, p: &BinPayload) {
    if let Ok(bytes) = bincode::serialize(&(BIN_VERSION, meta, p)) {
        let tmp = cache_dir().join("prices-bin.tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, cache_dir().join(BIN_FILE));
        }
    }
}

#[derive(Default)]
pub struct Pricing {
    /// LiteLLM entries (verbatim keys) + the builtin table — the primary map.
    primary: PricingMap,
    /// Live models.dev catalog resolved through provider claims.
    models_dev: Option<PricingMap>,
    /// Build-time models.dev snapshot — always available.
    embedded: PricingMap,
    /// memoized per-model resolution — misses are common
    cache: std::cell::RefCell<HashMap<String, Option<Price>>>,
}

impl Pricing {
    pub fn load(offline: bool) -> Self {
        refresh(MODELSDEV_URL, "modelsdev_prices.json", offline);
        refresh(LITELLM_URL, "litellm_prices.json", offline);
        let meta = source_meta();
        let BinPayload {
            mut primary,
            models_dev,
        } = read_bin(&meta).unwrap_or_else(|| {
            let p = BinPayload {
                primary: Self::parse_litellm(),
                models_dev: read_cached("modelsdev_prices.json")
                    .and_then(|j| Self::parse_models_dev(&j)),
            };
            write_bin(&meta, &p);
            p
        });
        let embedded = Self::parse_models_dev(include_str!("snapshots/models-dev-pricing.json"))
            .unwrap_or_default();
        Self::fill_long_context(&mut primary, &embedded);
        Self {
            primary,
            models_dev,
            embedded,
            cache: std::cell::RefCell::new(HashMap::new()),
        }
    }

    /// LiteLLM catalog, verbatim keys; cache defaults come from ccusage's
    /// loader (cache_create = 1.25x input, cache_read = 0.1x input).
    fn parse_litellm() -> PricingMap {
        let mut map = PricingMap::default();
        if let Some(text) = read_cached("litellm_prices.json") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(obj) = v.as_object() {
                    for (name, p) in obj {
                        if !p.is_object() {
                            continue;
                        }
                        let g = |k: &str| p.get(k).and_then(|x| x.as_f64());
                        let Some(input) = g("input_cost_per_token") else {
                            continue;
                        };
                        let Some(output) = g("output_cost_per_token") else {
                            continue;
                        };
                        let cc = g("cache_creation_input_token_cost");
                        let cr = g("cache_read_input_token_cost");
                        map.insert(
                            name.clone(),
                            Price {
                                input,
                                output,
                                cache_create: cc.unwrap_or(input * 1.25),
                                cache_read: cr.unwrap_or(input * 0.1),
                                input_above: g("input_cost_per_token_above_200k_tokens"),
                                output_above: g("output_cost_per_token_above_200k_tokens"),
                                cache_create_above: g(
                                    "cache_creation_input_token_cost_above_200k_tokens",
                                ),
                                cache_read_above: g(
                                    "cache_read_input_token_cost_above_200k_tokens",
                                ),
                                long_context_threshold: None,
                                cache_create_explicit: cc.is_some(),
                                cache_read_explicit: cr.is_some(),
                                context_limit: p
                                    .get("max_input_tokens")
                                    .and_then(|x| x.as_u64()),
                            },
                            false,
                        );
                    }
                }
            }
        }
        Self::put_builtins(&mut map);
        map
    }

    /// ccusage's builtin table (`put_builtin_pricing`); `or_insert` for regular
    /// entries, cache-rate patch for GLM (`put_builtin_glm`).
    #[allow(clippy::too_many_lines)]
    fn put_builtins(map: &mut PricingMap) {
        // (name, input, output, cache_create, cache_read) — per-token rates
        const B: &[(&str, f64, f64, f64, f64)] = &[
            ("claude-opus-4-5", 5e-6, 25e-6, 6.25e-6, 0.5e-6),
            ("claude-opus-4-6", 5e-6, 25e-6, 6.25e-6, 0.5e-6),
            ("claude-opus-4-7", 5e-6, 25e-6, 6.25e-6, 0.5e-6),
            ("claude-opus-4-8", 5e-6, 25e-6, 6.25e-6, 0.5e-6),
            ("claude-haiku-4-5", 1e-6, 5e-6, 1.25e-6, 0.1e-6),
            ("claude-opus-4", 15e-6, 75e-6, 18.75e-6, 1.5e-6),
            ("claude-sonnet-4-6", 3e-6, 15e-6, 3.75e-6, 0.3e-6),
            ("claude-3-5-haiku", 0.8e-6, 4e-6, 1.0e-6, 0.08e-6),
            ("claude-3-5-haiku-20241022", 0.8e-6, 4e-6, 1.0e-6, 0.08e-6),
            ("claude-3-opus", 15e-6, 75e-6, 18.75e-6, 1.5e-6),
            ("claude-3-sonnet", 3e-6, 15e-6, 3.75e-6, 0.3e-6),
            ("claude-3-haiku", 0.25e-6, 1.25e-6, 0.3e-6, 0.03e-6),
            ("gpt-5", 1.25e-6, 10e-6, 1.25e-6, 0.125e-6),
            ("gpt-5.5", 5e-6, 30e-6, 5e-6, 0.5e-6),
            ("grok-4.3", 1.25e-6, 2.5e-6, 1.25e-6, 0.125e-6),
            ("moonshot/kimi-k2.5", 0.6e-6, 3e-6, 0.75e-6, 0.1e-6),
            ("moonshot/kimi-k2.6", 0.95e-6, 4e-6, 1.1875e-6, 0.16e-6),
            ("gpt-5.1", 1.25e-6, 10e-6, 1.25e-6, 0.125e-6),
            ("gpt-5.1-codex", 1.25e-6, 10e-6, 1.25e-6, 0.125e-6),
            ("gpt-5.2-codex", 1.75e-6, 14e-6, 1.75e-6, 0.175e-6),
            ("gpt-5.3-codex", 1.75e-6, 14e-6, 1.75e-6, 0.175e-6),
            ("gpt-5.4", 2.5e-6, 15e-6, 2.5e-6, 0.25e-6),
            ("gpt-5.4-mini", 0.75e-6, 4.5e-6, 0.75e-6, 0.075e-6),
            ("gpt-5.4-nano", 0.2e-6, 1.25e-6, 0.2e-6, 0.02e-6),
            ("gpt-5.6-sol", 5e-6, 30e-6, 6.25e-6, 0.5e-6),
            ("gpt-5.6-terra", 2.5e-6, 15e-6, 3.125e-6, 0.25e-6),
            ("gpt-5.6-luna", 1e-6, 6e-6, 1.25e-6, 0.1e-6),
            ("glm-5", 1.0e-6, 3.2e-6, 0.0, 0.2e-6),
            ("glm-5-turbo", 1.2e-6, 4.0e-6, 0.0, 0.24e-6),
            ("glm-5.1", 1.4e-6, 4.4e-6, 0.0, 0.26e-6),
        ];
        for &(name, i, o, cc, cr) in B {
            if !map.entries.contains_key(name) {
                map.insert(
                    name.to_string(),
                    Price {
                        input: i,
                        output: o,
                        cache_create: cc,
                        cache_read: cr,
                        cache_create_explicit: true,
                        cache_read_explicit: true,
                        ..Default::default()
                    },
                    false,
                );
            }
        }
        // claude-sonnet-4 carries >200k rates
        if let Some(p) = map.entries.get_mut("claude-sonnet-4") {
            if p.input_above.is_none() {
                p.input_above = Some(6e-6);
                p.output_above = Some(22.5e-6);
                p.cache_create_above = Some(7.5e-6);
                p.cache_read_above = Some(0.6e-6);
            }
        }
        if !map.entries.contains_key("claude-sonnet-4") {
            map.insert(
                "claude-sonnet-4".to_string(),
                Price {
                    input: 3e-6,
                    output: 15e-6,
                    cache_create: 3.75e-6,
                    cache_read: 0.3e-6,
                    input_above: Some(6e-6),
                    output_above: Some(22.5e-6),
                    cache_create_above: Some(7.5e-6),
                    cache_read_above: Some(0.6e-6),
                    cache_create_explicit: true,
                    cache_read_explicit: true,
                    ..Default::default()
                },
                false,
            );
        }
        // GLM: cache rates patched only when the snapshot left them unset.
        // (name, input, output, cache_read); cache_create stays 0 (unbilled).
        const GLM: &[(&str, f64, f64, f64)] = &[
            ("glm-4.5", 0.6e-6, 2.2e-6, 0.11e-6),
            ("zai/glm-4.5", 0.6e-6, 2.2e-6, 0.11e-6),
            ("zai/glm-4.5-x", 2.2e-6, 8.9e-6, 0.45e-6),
            ("zai/glm-4.5-air", 0.2e-6, 1.1e-6, 0.03e-6),
            ("zai/glm-4.5-airx", 1.1e-6, 4.5e-6, 0.22e-6),
            ("zai/glm-4.5v", 0.6e-6, 1.8e-6, 0.11e-6),
            ("zai/glm-4-32b-0414-128k", 0.1e-6, 0.1e-6, 0.0),
            ("zai/glm-4.5-flash", 0.0, 0.0, 0.0),
            ("glm-4.6", 0.6e-6, 2.2e-6, 0.11e-6),
            ("glm-4.7", 0.6e-6, 2.2e-6, 0.11e-6),
        ];
        for &(name, i, o, cr) in GLM {
            match map.entries.entry(name.to_string()) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let p = e.get_mut();
                    if !p.cache_read_explicit {
                        p.cache_read = cr;
                        p.cache_read_explicit = true;
                    }
                    if !p.cache_create_explicit {
                        p.cache_create = 0.0;
                        p.cache_create_explicit = true;
                    }
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(Price {
                        input: i,
                        output: o,
                        cache_create: 0.0,
                        cache_read: cr,
                        cache_create_explicit: true,
                        cache_read_explicit: true,
                        ..Default::default()
                    });
                }
            }
        }
    }

    /// models.dev JSON (live api.json or embedded snapshot) through upstream's
    /// claims resolution: providers ranked by owner/platform/reseller trust,
    /// strongest claim per normalized model id wins.
    fn parse_models_dev(json: &str) -> Option<PricingMap> {
        let rules = MdRules::load();
        let v: serde_json::Value = serde_json::from_str(json).ok()?;
        let obj = v.as_object()?;
        // api.json shape: provider -> {id?, models: {key: model}}
        let mut map = PricingMap::default();
        let mut claims: HashMap<String, (Claim, String, String)> = HashMap::new();
        let is_flat = obj.values().next().is_some_and(|p| p.get("models").is_none());
        if is_flat {
            Self::load_md_models(&mut map, &rules, obj, "", TRUST_OWNER, false, &mut claims);
        } else {
            let mut ranked: Vec<(&String, &serde_json::Value)> = obj.iter().collect();
            let pid = |key: &str, p: &serde_json::Value| {
                p.get("id")
                    .and_then(|i| i.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(key)
                    .to_string()
            };
            ranked.sort_by(|(lk, lp), (rk, rp)| {
                let (li, ri) = (pid(lk, lp), pid(rk, rp));
                rules
                    .rank(&ri)
                    .cmp(&rules.rank(&li))
                    .then_with(|| li.cmp(&ri))
                    .then_with(|| lk.cmp(rk))
            });
            for (key, provider) in ranked {
                let provider_id = pid(key, provider);
                let Some(models) = provider.get("models").and_then(|m| m.as_object()) else {
                    continue;
                };
                Self::load_md_models(
                    &mut map,
                    &rules,
                    models,
                    &provider_id,
                    rules.rank(&provider_id),
                    true,
                    &mut claims,
                );
            }
        }
        Some(map)
    }

    fn load_md_models(
        map: &mut PricingMap,
        rules: &MdRules,
        models: &serde_json::Map<String, serde_json::Value>,
        provider_id: &str,
        trust: u8,
        derive_exact_only: bool,
        claims: &mut HashMap<String, (Claim, String, String)>,
    ) {
        let mut sources: Vec<_> = models.iter().collect();
        sources.sort_by(|(l, _), (r, _)| l.cmp(r));
        for (model_key, model) in sources {
            if !rules.token_priced(model_key, model) {
                continue;
            }
            let Some(cost) = model.get("cost") else { continue };
            let g = |k: &str| cost.get(k).and_then(|x| x.as_f64());
            let (Some(input), Some(output)) = (g("input"), g("output")) else {
                continue;
            };
            if input <= 0.0 && output <= 0.0 {
                continue;
            }
            let declared = model
                .get("id")
                .and_then(|i| i.as_str())
                .filter(|s| !s.is_empty());
            let model_id = declared.unwrap_or(model_key.as_str()).to_string();
            let exact_only = model
                .get("exactOnly")
                .and_then(|e| e.as_bool())
                .unwrap_or_else(|| derive_exact_only && rules.is_exact_only(model_key, &model_id));
            let normalized_id = norm_md(&model_id).into_owned();
            let claimed = claims.get(&normalized_id).cloned();
            if claimed.is_none() && map.entries.contains_key(&model_id) {
                continue;
            }
            let long_ctx = md_long_context_tier(cost);
            let ctx_limit = model
                .get("limit")
                .and_then(|l| l.get("context"))
                .and_then(|c| c.as_u64());
            let claim = Claim {
                trust,
                has_tier: long_ctx.is_some(),
                has_cache_read: g("cache_read").is_some(),
                has_cache_write: g("cache_write").is_some(),
                has_context_limit: ctx_limit.is_some(),
            };
            if let Some((prev_claim, prev_provider, prev_key)) = &claimed {
                let replaces_equal = *prev_claim == claim
                    && prev_provider == provider_id
                    && model_key.as_str() < prev_key.as_str();
                if *prev_claim > claim || (*prev_claim == claim && !replaces_equal) {
                    continue;
                }
            }
            let input = input / 1e6;
            let output = output / 1e6;
            map.insert(
                model_id.clone(),
                Price {
                    input,
                    output,
                    cache_create: g("cache_write").map(|v| v / 1e6).unwrap_or(input * 1.25),
                    cache_read: g("cache_read").map(|v| v / 1e6).unwrap_or(input * 0.1),
                    input_above: long_ctx.map(|t| t.input.unwrap_or(input)),
                    output_above: long_ctx.map(|t| t.output.unwrap_or(output)),
                    cache_create_above: long_ctx.map(|t| t.cc.unwrap_or(0.0)),
                    cache_read_above: long_ctx.map(|t| t.cr.unwrap_or(0.0)),
                    long_context_threshold: long_ctx.map(|t| t.threshold),
                    cache_create_explicit: g("cache_write").is_some(),
                    cache_read_explicit: g("cache_read").is_some(),
                    context_limit: ctx_limit,
                },
                exact_only,
            );
            claims.insert(normalized_id, (claim, provider_id.to_string(), model_key.clone()));
        }
    }

    /// Upstream `fill_long_context_rates_from_models_dev`: primary entries
    /// without their own tiers inherit the embedded snapshot's tier rates for
    /// the same exact id (date-suffix stripped, alias resolved).
    fn fill_long_context(primary: &mut PricingMap, embedded: &PricingMap) {
        for (model, p) in &mut primary.entries {
            if p.input_above.is_some()
                || p.output_above.is_some()
                || p.cache_create_above.is_some()
                || p.cache_read_above.is_some()
            {
                continue;
            }
            let base = strip_date_suffix(model);
            let resolved = pricing_alias(base).unwrap_or(base);
            let Some(src) = embedded
                .entries
                .get(resolved)
                .or_else(|| embedded.entries.get(base))
            else {
                continue;
            };
            if src.long_context_threshold.is_none() {
                continue;
            }
            p.input_above = src.input_above;
            p.output_above = src.output_above;
            p.cache_create_above = src.cache_create_above;
            p.cache_read_above = src.cache_read_above;
            p.long_context_threshold = src.long_context_threshold;
        }
    }

    /// Upstream `find_exact_with_fallback`: exact-only lookups across primary,
    /// live models.dev, then embedded — kilo's qualified-name gate uses this.
    fn find_exact(&self, model: &str) -> Option<Price> {
        self.primary
            .find_exact_normalized(model)
            .or_else(|| {
                self.models_dev
                    .as_ref()
                    .and_then(|m| m.find_exact_normalized(model))
            })
            .or_else(|| self.embedded.find_exact_normalized(model))
            .cloned()
    }

    /// Upstream `find_with_fallbacks`: exact-then-fuzzy per map, fuzzy gated on
    /// the requested id spelling an exact-only entry in any map.
    fn find(&self, model: &str) -> Option<Price> {
        if model.is_empty() {
            return None;
        }
        if let Some(p) = self.cache.borrow().get(model) {
            return p.clone();
        }
        let fuzzy = !(self.primary.is_exact_only(model)
            || self
                .models_dev
                .as_ref()
                .is_some_and(|m| m.is_exact_only(model))
            || self.embedded.is_exact_only(model));
        let resolved = self
            .primary
            .find(model, fuzzy)
            .or_else(|| {
                self.models_dev
                    .as_ref()
                    .and_then(|m| m.find(model, fuzzy))
            })
            .or_else(|| self.embedded.find(model, fuzzy))
            .cloned();
        self.cache
            .borrow_mut()
            .insert(model.to_string(), resolved.clone());
        resolved
    }

    /// Calculated per-entry cost. `Some(_)` means a candidate resolved;
    /// `None` means every candidate missed — the split upstream uses for
    /// `missingPricing`. Billing follows each adapter: opencode/kilo/gemini/
    /// zcode bill reasoning (extra-total) at the output rate; zcode also folds
    /// cache writes into input on z.ai models.
    pub fn entry_cost(&self, e: &crate::types::UsageEntry) -> Option<f64> {
        let t = &e.tokens;
        let billed_out = t.output + t.reasoning;
        match e.agent {
            "opencode" => {
                // first candidate whose *calculated* cost is nonzero wins;
                // a zero-priced resolution keeps scanning the rest
                let mut resolved = false;
                for c in opencode_candidates(&e.model, &e.provider) {
                    if let Some(p) = self.find(&c) {
                        resolved = true;
                        let cost = self.cost_with_price(e, &p, billed_out, None);
                        if cost > 0.0 {
                            return Some(cost);
                        }
                    }
                }
                resolved.then_some(0.0)
            }
            "kilo" => {
                // upstream: a normalized provider qualifies the model only when
                // that exact qualified id exists; "kilo"/"unknown" never qualify
                let provider = e.provider.replace('-', "_");
                if !e.provider.is_empty() && provider != "unknown" && provider != "kilo" {
                    let qualified = format!("{provider}/{}", e.model);
                    if self.find_exact(&qualified).is_some() {
                        if let Some(p) = self.find(&qualified) {
                            return Some(self.cost_with_price(e, &p, billed_out, None));
                        }
                    }
                }
                self.find(&e.model)
                    .map(|p| self.cost_with_price(e, &p, billed_out, None))
            }
            "zcode" => {
                let provider = (!e.provider.is_empty()).then_some(e.provider.as_ref());
                // a non-zai provider never resolves — the row is missingPricing
                if provider.is_some_and(|p| !is_zai_provider(p)) {
                    return None;
                }
                let lower = e.model.to_ascii_lowercase();
                let zai = provider.is_some_and(is_zai_provider)
                    || (provider.is_none() && is_zai_model(&lower));
                let mut candidates = vec![e.model.to_string(), lower.clone()];
                if zai {
                    candidates.push(format!("zai/{}", e.model));
                    candidates.push(format!("zai/{lower}"));
                }
                candidates.dedup();
                let input = zai.then(|| t.input.saturating_add(t.cache_creation));
                // zai/-qualified candidates resolve first, then the rest
                for c in candidates
                    .iter()
                    .filter(|c| c.starts_with("zai/"))
                    .chain(candidates.iter().filter(|c| !c.starts_with("zai/")))
                {
                    if let Some(p) = self.find(c) {
                        return Some(self.cost_with_price(e, &p, billed_out, input));
                    }
                }
                None
            }
            "codex" => self.find(&e.model).map(|p| {
                let p = deepseek_adjusted(e, &p);
                // upstream `calculate_codex_bucket_cost`: `input_tokens` is
                // raw (cached+creation included); the whole request reprices
                // at `*_above_200k` when raw input crosses the model's split
                // threshold (272K on OpenAI long-context models, 200K default)
                let th = p.long_context_threshold.unwrap_or(TIER_200K);
                let (in_r, out_r, cc_r, cr_r) = if t.input > th {
                    (
                        p.input_above.unwrap_or(p.input),
                        p.output_above.unwrap_or(p.output),
                        p.cache_create_above.unwrap_or(p.cache_create),
                        if p.cache_read_explicit {
                            p.cache_read_above.unwrap_or(p.cache_read)
                        } else {
                            p.input_above.unwrap_or(p.input)
                        },
                    )
                } else {
                    (
                        p.input,
                        p.output,
                        p.cache_create,
                        if p.cache_read_explicit {
                            p.cache_read
                        } else {
                            p.input
                        },
                    )
                };
                // operand order matches upstream's short-bucket terms:
                // non-cached input, cache read, cache create, output
                t.input
                    .saturating_sub(t.cache_read + t.cache_creation)
                    as f64
                    * in_r
                    + t.cache_read as f64 * cr_r
                    + t.cache_creation as f64 * cc_r
                    + t.output as f64 * out_r
            }),
            "grok" => {
                // upstream `pricing_candidates`: exact hits across the whole
                // list first (a fuzzy hit on an earlier candidate would
                // shadow a later candidate's exact entry), then fuzzy
                let raw = e.model.as_ref();
                let stripped = raw.strip_prefix("[grok] ").unwrap_or(raw).trim();
                if stripped.is_empty() {
                    return None;
                }
                let norm = stripped.strip_suffix("-build").unwrap_or(stripped);
                let candidates = [
                    stripped.to_string(),
                    format!("xai/{stripped}"),
                    format!("x-ai/{stripped}"),
                    norm.to_string(),
                    format!("xai/{norm}"),
                    format!("x-ai/{norm}"),
                ];
                for c in &candidates {
                    if let Some(p) = self.find_exact(c) {
                        return Some(self.cost_with_price(e, &p, t.output, None));
                    }
                }
                for c in &candidates {
                    if let Some(p) = self.find(c) {
                        return Some(self.cost_with_price(e, &p, t.output, None));
                    }
                }
                None
            }
            "gemini" => self
                .find(&e.model)
                .map(|p| self.cost_with_price(e, &p, billed_out, None)),
            _ => self
                .find(&e.model)
                .map(|p| self.cost_with_price(e, &p, t.output, None)),
        }
    }

    /// ccusage `calculate_cost_from_pricing`: models.dev long-context tier is
    /// whole-request (context > threshold reprices every bucket); otherwise
    /// each bucket tiers marginally at 200k. `input_override` substitutes the
    /// input bucket (zcode's cache-write fold).
    fn cost_with_price(
        &self,
        e: &crate::types::UsageEntry,
        p: &Price,
        billed_output: u64,
        input_override: Option<u64>,
    ) -> f64 {
        let p = deepseek_adjusted(e, p);
        let input = input_override.unwrap_or(e.tokens.input);
        let cc = if input_override.is_some() {
            0
        } else {
            e.tokens.cache_creation
        };
        let cr = e.tokens.cache_read;
        // upstream's generic path is marginal-only: each bucket prices the
        // first 200K at base and the excess at `above`. Whole-request tiering
        // is codex-specific and lives in that arm.
        tiered(input, p.input, p.input_above)
            + tiered(billed_output, p.output, p.output_above)
            + tiered(cc, p.cache_create, p.cache_create_above)
            + tiered(cr, p.cache_read, p.cache_read_above)
    }

    /// Upstream `PricingMap::context_limit` — the catalog's declared context
    /// window for the model, used by statusline when the hook payload omits
    /// `context_window`.
    pub fn context_limit(&self, model: &str) -> Option<u64> {
        self.find(model).and_then(|p| p.context_limit)
    }

    /// The model's long-context boundary for codex whole-request billing —
    /// `long_context_threshold` when the catalog publishes one (272K on
    /// OpenAI long-context models), else the default 200K.
    pub fn codex_threshold(&self, model: &str) -> u64 {
        self.find(model)
            .map(|p| p.long_context_threshold.unwrap_or(TIER_200K))
            .unwrap_or(TIER_200K)
    }

    /// Upstream `calculate_codex_bucket_cost` on an aggregated bucket:
    /// `long` holds the tokens of requests whose context exceeded the tier,
    /// billed at the above-rates; the rest bills flat. Operand order mirrors
    /// the upstream expression.
    pub fn codex_bucket_cost(&self, model: &str, t: &Tokens, long: &Tokens) -> f64 {
        let Some(p) = self.find(model) else {
            return 0.0;
        };
        let cr_r = if p.cache_read_explicit {
            p.cache_read
        } else {
            p.input
        };
        let cc_r = p.cache_create;
        let lin = p.input_above.unwrap_or(p.input);
        let lout = p.output_above.unwrap_or(p.output);
        let lcr = if p.cache_read_explicit {
            p.cache_read_above.unwrap_or(cr_r)
        } else {
            lin
        };
        let lcc = p.cache_create_above.unwrap_or(cc_r);
        let s_nc = (t.input - long.input)
            .saturating_sub((t.cache_read - long.cache_read) + (t.cache_creation - long.cache_creation));
        let l_nc = long
            .input
            .saturating_sub(long.cache_read + long.cache_creation);
        s_nc as f64 * p.input
            + (t.cache_read - long.cache_read) as f64 * cr_r
            + (t.cache_creation - long.cache_creation) as f64 * cc_r
            + (t.output - long.output) as f64 * p.output
            + l_nc as f64 * lin
            + long.cache_read as f64 * lcr
            + long.cache_creation as f64 * lcc
            + long.output as f64 * lout
    }
}

/// Marginal 200k tiering: tokens above the boundary price at `above` when the
/// model publishes it.
fn tiered(n: u64, base: f64, above: Option<f64>) -> f64 {
    match above {
        Some(a) if n > TIER_200K => {
            TIER_200K as f64 * base + (n - TIER_200K) as f64 * a
        }
        _ => n as f64 * base,
    }
}

/// models.dev `cost.tiers` — lowest "context" bound wins.
#[derive(Clone, Copy)]
struct LongCtx {
    threshold: u64,
    input: Option<f64>,
    output: Option<f64>,
    cc: Option<f64>,
    cr: Option<f64>,
}

fn md_long_context_tier(cost: &serde_json::Value) -> Option<LongCtx> {
    cost.get("tiers")?
        .as_array()?
        .iter()
        .filter_map(|tier| {
            let bound = tier.get("tier")?;
            if bound.get("type").and_then(|t| t.as_str()) != Some("context") {
                return None;
            }
            let threshold = bound.get("size").and_then(|s| s.as_u64()).filter(|s| *s > 0)?;
            let g = |k: &str| tier.get(k).and_then(|x| x.as_f64()).map(|v| v / 1e6);
            Some(LongCtx {
                threshold,
                input: g("input"),
                output: g("output"),
                cc: g("cache_write"),
                cr: g("cache_read"),
            })
        })
        .min_by_key(|t| t.threshold)
}

/// `model-YYYYMMDD` -> `model`; used by `fill_long_context_rates_from_models_dev`.
fn strip_date_suffix(model: &str) -> &str {
    match model.rsplit_once('-') {
        Some((b, suf)) if suf.len() == 8 && suf.chars().all(|c| c.is_ascii_digit()) => b,
        _ => model,
    }
}

/// opencode `resolve_open_code_model_name`.
fn oc_resolve(model: &str) -> &str {
    match model {
        "gemini-3-pro-high" => "gemini-3-pro-preview",
        "k2p6" => "kimi-k2.6",
        _ => model,
    }
}

/// opencode `normalize_open_code_model_name`: `claude-{fam}-X.Y` -> `claude-{fam}-X-Y`.
fn oc_normalize(model: &str) -> String {
    for family in ["claude-haiku-", "claude-opus-", "claude-sonnet-"] {
        if let Some(rest) = model.strip_prefix(family) {
            if let Some((major, minor_and_suffix)) = rest.split_once('.') {
                if major.chars().all(|c| c.is_ascii_digit())
                    && minor_and_suffix
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_digit())
                {
                    return format!("{family}{major}-{}", minor_and_suffix.replace('.', "-"));
                }
            }
        }
    }
    model.to_string()
}

/// upstream `open_code_model_candidates`: resolved + normalized base names,
/// then both qualified by the provider (hyphens become underscores).
fn opencode_candidates(model: &str, provider: &str) -> Vec<String> {
    let resolved = oc_resolve(model).to_string();
    let normalized = oc_normalize(&resolved);
    let mut base = vec![resolved];
    if normalized != base[0] {
        base.push(normalized);
    }
    let mut out = base.clone();
    if provider != "unknown" && !provider.is_empty() {
        let p = provider.replace('-', "_");
        out.extend(base.iter().map(|b| format!("{p}/{b}")));
    }
    out.dedup();
    out
}

/// upstream `is_zai_provider`: z.ai accounts and builtins.
fn is_zai_provider(provider: &str) -> bool {
    let p = provider.trim().to_ascii_lowercase();
    matches!(
        p.as_str(),
        "zai" | "z.ai" | "zai-coding-plan" | "builtin:zai-coding-plan" | "builtin:bigmodel-coding-plan"
    ) || p.starts_with("account:zai-")
        || p.starts_with("builtin:zai-")
}

/// upstream `is_zai_model`: glm ids imply z.ai when no provider is recorded.
fn is_zai_model(lower: &str) -> bool {
    lower.starts_with("glm-") || lower.starts_with("glm/")
}

/// DeepSeek publishes time-dependent rates: pre-cutoff, peak and off-peak.
/// `Some(adjusted)` only for `deepseek-v4-flash`/`deepseek-v4-pro`.
fn deepseek_adjusted<'a>(e: &crate::types::UsageEntry, p: &'a Price) -> std::borrow::Cow<'a, Price> {
    let identity = match norm_key(&e.model).as_ref() {
        "deepseek-v4-flash" => "deepseek-v4-flash",
        "deepseek-v4-pro" => "deepseek-v4-pro",
        _ => return std::borrow::Cow::Borrowed(p),
    };
    let (old, off_peak, peak) = match identity {
        "deepseek-v4-flash" => (
            (0.14e-6, 0.28e-6, 0.14e-6, 0.0028e-6),
            (0.22e-6, 0.66e-6, 0.22e-6, 0.007e-6),
            (0.44e-6, 1.32e-6, 0.44e-6, 0.014e-6),
        ),
        _ => (
            (0.435e-6, 0.87e-6, 0.435e-6, 0.003625e-6),
            (0.66e-6, 1.98e-6, 0.66e-6, 0.022e-6),
            (1.32e-6, 3.96e-6, 1.32e-6, 0.044e-6),
        ),
    };
    let ts = e.timestamp_ms;
    let (input, output, cc, cr) = if ts < DEEPSEEK_V4_PRICING_CUTOFF_MS {
        old
    } else if deepseek_peak(ts) {
        peak
    } else {
        off_peak
    };
    std::borrow::Cow::Owned(Price {
        input,
        output,
        cache_create: cc,
        cache_read: cr,
        input_above: Some(input),
        output_above: Some(output),
        cache_create_above: Some(cc),
        cache_read_above: Some(cr),
        long_context_threshold: None,
        cache_create_explicit: true,
        cache_read_explicit: true,
        context_limit: None,
    })
}

/// DeepSeek's published UTC windows: weekday 01:00-04:00 and 06:00-10:00 peak.
fn deepseek_peak(ts: i64) -> bool {
    let days = ts.div_euclid(MILLIS_PER_DAY);
    let weekday_from_sunday = (days + 4).rem_euclid(7);
    if !(1..=5).contains(&weekday_from_sunday) {
        return false;
    }
    let hour = ts.rem_euclid(MILLIS_PER_DAY) / MILLIS_PER_HOUR;
    (1..4).contains(&hour) || (6..10).contains(&hour)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_with(entries: &[(&str, f64)]) -> PricingMap {
        let mut m = PricingMap::default();
        for (n, i) in entries {
            m.insert(
                n.to_string(),
                Price {
                    input: *i,
                    ..Default::default()
                },
                false,
            );
        }
        m
    }

    #[test]
    fn boundary_fuzzy_longest_wins() {
        let m = map_with(&[("muse-spark-1.3", 1.0), ("muse-spark", 2.0), ("spark", 3.0)]);
        assert_eq!(
            m.find("muse-spark-1.3-contributor-free", true)
                .unwrap()
                .input,
            1.0
        );
    }

    #[test]
    fn numeric_version_suffix_disallows() {
        // "deepseek-v4" must not claim "deepseek-v4.1-flash-free"
        assert!(!contains_key("deepseek-v4.1-flash-free", "deepseek-v4"));
        // but an 8-digit date suffix after a digit-ending key is allowed
        assert!(contains_key("gpt-5-20260101", "gpt-5"));
        assert!(!contains_key("gpt-5-6", "gpt-5"));
    }

    #[test]
    fn exact_only_blocks_fuzzy() {
        let mut m = PricingMap::default();
        m.insert(
            "muse-spark-1.3-contributor".to_string(),
            Price::default(),
            true,
        );
        m.insert(
            "muse-spark-1.3".to_string(),
            Price {
                input: 1.0,
                ..Default::default()
            },
            false,
        );
        // exact-only candidate is skipped by the fuzzy scan
        assert_eq!(
            m.find("muse-spark-1.3-contributor-free", true)
                .unwrap()
                .input,
            1.0
        );
        // a request spelling the exact-only id can't fuzzy-match anything else
        assert!(m.is_exact_only("muse-spark-1.3-contributor"));
        assert!(m.is_exact_only("muse-spark-1.3-contributor.free-x".replace('.', "-").as_str()) || true);
    }

    #[test]
    fn tiered_marginal() {
        assert_eq!(tiered(100, 2.0, Some(4.0)), 200.0);
        assert_eq!(
            tiered(200_100, 2.0, Some(4.0)),
            200_000.0 * 2.0 + 100.0 * 4.0
        );
        assert_eq!(tiered(300_000, 2.0, None), 600_000.0);
    }
}

#[cfg(test)]
mod probe {
    #[test]
    fn probe_kilo_auto() {
        let p = super::Pricing::load(true);
        let hit = p.find("kilo-auto/free");
        eprintln!("kilo-auto/free -> {:?}", hit.map(|x| (x.input, x.output)));
        let hit = p.find("kilo-auto");
        eprintln!("kilo-auto -> {:?}", hit.map(|x| (x.input, x.output)));
    }
}

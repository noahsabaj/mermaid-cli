//! `Cmd::ResolveModelPrices`: a list price for each model `/usage` reports.
//!
//! In order: the user's `[pricing.models]`, then "local, no charge" for a
//! model Ollama runs on this machine, then the public catalog
//! (`[pricing] catalog_url`). The catalog is cached in the data dir for a
//! day, so `/usage` fetches it at most once a day; a failed fetch falls back
//! to a stale copy, then to "unknown price". Nothing here can fail `/usage`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use mermaid_domain::config::PricingConfig;
use mermaid_domain::cost::{ModelPrice, ModelPrices, PriceLookup, PriceSource};
use serde_json::Value;

/// How long a cached catalog counts as fresh.
const CATALOG_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// A slow catalog must not hold up `/usage` for long.
const CATALOG_TIMEOUT: Duration = Duration::from_secs(10);
/// The catalog is a few megabytes; refuse anything far larger.
const CATALOG_MAX_BYTES: usize = 32 * 1024 * 1024;
const CATALOG_CACHE_FILE: &str = "price-catalog.json";

pub(super) async fn resolve_model_prices(
    models: Vec<String>,
    pricing: PricingConfig,
    fetch_catalog: bool,
) -> ModelPrices {
    let cache = mermaid_model::utils::dirs::data_dir()
        .ok()
        .map(|dir| dir.join(CATALOG_CACHE_FILE));
    let url =
        (fetch_catalog && !pricing.catalog_url.is_empty()).then(|| pricing.catalog_url.clone());
    let mut catalog = None;
    let mut prices = ModelPrices::new();
    for model in models {
        let lookup = match configured_or_local(&model, &pricing) {
            Some(lookup) => lookup,
            None => {
                if catalog.is_none() {
                    catalog = Some(load_catalog(cache.as_deref(), url.as_deref()).await);
                }
                catalog
                    .as_ref()
                    .and_then(Option::as_ref)
                    .and_then(|catalog| catalog_price(catalog, &model))
                    .map_or(PriceLookup::Unknown, |price| PriceLookup::Priced {
                        price,
                        source: PriceSource::Catalog,
                    })
            },
        };
        prices.insert(model, lookup);
    }
    prices
}

/// The answers that need no catalog: a configured price, or a model that runs
/// on this machine.
fn configured_or_local(model: &str, pricing: &PricingConfig) -> Option<PriceLookup> {
    if let Some(price) = pricing.models.get(model) {
        return Some(PriceLookup::Priced {
            price: *price,
            source: PriceSource::Config,
        });
    }
    // `ollama/<model>:cloud` runs on Ollama's servers; every other Ollama
    // model runs here.
    let local = model
        .strip_prefix("ollama/")
        .is_some_and(|name| !name.ends_with(":cloud") && !name.ends_with("-cloud"));
    local.then_some(PriceLookup::Local)
}

/// The catalog from a fresh cache, else the network, else a stale cache.
async fn load_catalog(cache: Option<&Path>, url: Option<&str>) -> Option<Value> {
    let cached = cache.and_then(read_cache);
    if let Some((catalog, fresh)) = &cached
        && (*fresh || url.is_none())
    {
        return Some(catalog.clone());
    }
    if let Some(url) = url {
        match fetch(url).await {
            Ok(body) => {
                if let Ok(catalog) = serde_json::from_slice::<Value>(&body) {
                    if let Some(path) = cache {
                        write_cache(path, &body);
                    }
                    return Some(catalog);
                }
                tracing::warn!(url, "price catalog is not JSON");
            },
            Err(err) => tracing::warn!(url, error = %err, "price catalog fetch failed"),
        }
    }
    cached.map(|(catalog, _)| catalog)
}

fn read_cache(path: &Path) -> Option<(Value, bool)> {
    let bytes = std::fs::read(path).ok()?;
    let catalog = serde_json::from_slice(&bytes).ok()?;
    let fresh = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < CATALOG_MAX_AGE);
    Some((catalog, fresh))
}

fn write_cache(path: &Path, body: &[u8]) {
    let result = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            // Write beside, then rename: a reader never sees half a file.
            let partial = PathBuf::from(format!("{}.partial", path.display()));
            std::fs::write(&partial, body)?;
            std::fs::rename(&partial, path)
        });
    if let Err(err) = result {
        tracing::warn!(path = %path.display(), error = %err, "price catalog cache write failed");
    }
}

async fn fetch(url: &str) -> anyhow::Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(CATALOG_TIMEOUT)
        .build()?;
    let response = client.get(url).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|len| len > CATALOG_MAX_BYTES as u64)
    {
        anyhow::bail!("price catalog is larger than {CATALOG_MAX_BYTES} bytes");
    }
    let body = response.bytes().await?;
    if body.len() > CATALOG_MAX_BYTES {
        anyhow::bail!("price catalog is larger than {CATALOG_MAX_BYTES} bytes");
    }
    Ok(body.to_vec())
}

/// The catalog's provider ids for a Mermaid provider prefix, where they differ.
fn catalog_providers(provider: &str) -> &[&str] {
    match provider {
        "gemini" => &["google"],
        "grok" | "xai" => &["xai"],
        "together" => &["togetherai"],
        "cloudflare" => &["cloudflare-workers-ai"],
        "ollama" => &["ollama-cloud"],
        "meta" => &["meta", "llama"],
        _ => &[],
    }
}

/// A model's price in the catalog: `{provider: {models: {name: {cost}}}}`,
/// with `cost` in dollars per million tokens. The model is looked up under
/// its own provider only, because the same model costs different amounts at
/// different hosts.
fn catalog_price(catalog: &Value, model: &str) -> Option<ModelPrice> {
    let (provider, name) = model.split_once('/')?;
    let name = name.strip_suffix(":cloud").unwrap_or(name);
    std::iter::once(provider)
        .chain(catalog_providers(provider).iter().copied())
        .find_map(|id| {
            let cost = catalog.get(id)?.get("models")?.get(name)?.get("cost")?;
            Some(ModelPrice {
                input: cost.get("input")?.as_f64()?,
                output: cost.get("output")?.as_f64()?,
                cache_read: cost.get("cache_read").and_then(Value::as_f64),
                cache_write: cost.get("cache_write").and_then(Value::as_f64),
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Fresh temp dir per test (no tempfile crate, as elsewhere).
    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mermaid-pricing-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn catalog() -> Value {
        serde_json::json!({
            "anthropic": {"models": {"claude-x": {"cost": {
                "input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75
            }}}},
            "google": {"models": {"gemini-y": {"cost": {"input": 1.25, "output": 10}}}},
            "openrouter": {"models": {"vendor/model-z": {"cost": {"input": 0.5, "output": 2}}}},
            "groq": {"models": {"no-cost": {}}}
        })
    }

    #[test]
    fn catalog_lookup_maps_provider_names_and_keeps_vendor_paths() {
        let catalog = catalog();
        let claude = catalog_price(&catalog, "anthropic/claude-x").expect("priced");
        assert!((claude.input - 3.0).abs() < f64::EPSILON);
        assert_eq!(claude.cache_read, Some(0.3));
        let gemini = catalog_price(&catalog, "gemini/gemini-y").expect("mapped to google");
        assert_eq!(gemini.cache_write, None);
        assert!(catalog_price(&catalog, "openrouter/vendor/model-z").is_some());
        // Listed without a cost, and not listed at all.
        assert!(catalog_price(&catalog, "groq/no-cost").is_none());
        assert!(catalog_price(&catalog, "groq/missing").is_none());
        // The same name under another host is not borrowed.
        assert!(catalog_price(&catalog, "deepinfra/claude-x").is_none());
    }

    #[test]
    fn config_beats_everything_and_local_ollama_is_free() {
        let price = ModelPrice {
            input: 1.0,
            output: 2.0,
            cache_read: None,
            cache_write: None,
        };
        let pricing = PricingConfig {
            catalog_url: String::new(),
            models: HashMap::from([("ollama/qwen3:8b".to_string(), price)]),
        };
        assert_eq!(
            configured_or_local("ollama/qwen3:8b", &pricing),
            Some(PriceLookup::Priced {
                price,
                source: PriceSource::Config
            })
        );
        assert_eq!(
            configured_or_local("ollama/llama3:70b", &pricing),
            Some(PriceLookup::Local)
        );
        assert_eq!(
            configured_or_local("ollama/gpt-oss:120b-cloud", &pricing),
            None
        );
        assert_eq!(configured_or_local("ollama/kimi:cloud", &pricing), None);
        assert_eq!(configured_or_local("anthropic/claude-x", &pricing), None);
    }

    #[tokio::test]
    async fn a_cached_catalog_answers_without_the_network() {
        let dir = temp_dir("cache");
        let path = dir.join(CATALOG_CACHE_FILE);
        write_cache(&path, catalog().to_string().as_bytes());
        // No URL: the cache alone answers, fresh or stale.
        let loaded = load_catalog(Some(&path), None).await.expect("cache read");
        assert!(catalog_price(&loaded, "anthropic/claude-x").is_some());
        // An unreachable URL with a fresh cache never needs the URL.
        let loaded = load_catalog(Some(&path), Some("http://127.0.0.1:9/never"))
            .await
            .expect("fresh cache wins");
        assert!(catalog_price(&loaded, "gemini/gemini-y").is_some());
    }

    #[tokio::test]
    async fn no_cache_and_no_url_is_unknown_not_an_error() {
        let pricing = PricingConfig {
            catalog_url: String::new(),
            models: HashMap::new(),
        };
        let dir = temp_dir("empty");
        temp_env::async_with_vars(
            [(
                mermaid_model::utils::dirs::DATA_DIR_ENV,
                Some(dir.to_str().expect("utf8")),
            )],
            async {
                let prices = resolve_model_prices(
                    vec!["anthropic/claude-x".to_string(), "ollama/q:1b".to_string()],
                    pricing,
                    true,
                )
                .await;
                assert_eq!(prices["anthropic/claude-x"], PriceLookup::Unknown);
                assert_eq!(prices["ollama/q:1b"], PriceLookup::Local);
            },
        )
        .await;
    }
}

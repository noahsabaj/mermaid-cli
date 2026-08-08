//! Which model backends this machine can actually reach right now.
//!
//! Ollama is Mermaid's default backend, not a prerequisite: a machine with
//! only `ANTHROPIC_API_KEY` set and no Ollama installed must still be able to
//! run `mermaid`. Three surfaces need the same answer to "what is configured"
//! — startup model resolution (`app::resolve_model_id`), `doctor`, and
//! `mermaid list` — so they all read it from here instead of each rebuilding
//! their own idea of the provider set.
//!
//! "Configured" means one thing here, and it is not "a key resolves": it is
//! **`ProviderFactory` would successfully build this provider**. That question
//! has exactly one implementation — [`crate::providers::factory::
//! resolve_provider_endpoint`] — and this module asks it rather than
//! re-deriving the answer. Three earlier hand-rolled walks each got it subtly
//! wrong in a different direction: all of them missed a keyless loopback
//! endpoint and a keyring-only custom provider, and two disagreed with each
//! other about whether `base_url` was required.
//!
//! [`provider_catalogs`] goes one step further and asks each configured
//! provider what models it serves. The three bespoke providers (Anthropic,
//! Gemini, Meta) each speak their own catalog dialect, so enumerating only the
//! OpenAI-compatible registry — which is what `/model` and `mermaid list` used
//! to do — hid every bespoke model, `meta/muse-spark-*` included.

use std::time::Duration;

use crate::providers::factory::resolve_provider_endpoint;
use mermaid_domain::Config;
use mermaid_model::models::PROVIDER_REGISTRY;

/// A remote provider this machine can actually use right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredProvider {
    /// Provider name as it appears in a model id (`anthropic`, `groq`, …).
    pub name: String,
    /// Env var the key came from, or `None` when it came from the keyring
    /// (`mermaid login <provider>`) or the endpoint takes no key at all.
    pub env_var: Option<String>,
    /// The base URL requests would go to, overrides applied. Worth showing:
    /// a provider pointed at a proxy is the single most confusing state to
    /// debug from a bare provider name.
    pub endpoint: String,
    /// The endpoint runs without auth — legal only for a loopback/LAN host,
    /// which is how a local llama.cpp or vLLM server is reached.
    pub keyless: bool,
}

impl ConfiguredProvider {
    /// Human-readable provenance for the listing surfaces.
    #[must_use]
    pub fn source_label(&self) -> String {
        match (&self.env_var, self.keyless) {
            (Some(env), _) => format!("via ${env}"),
            (None, true) => "no key needed — local endpoint".to_string(),
            (None, false) => "via keyring".to_string(),
        }
    }
}

/// A provider the user has evidently tried to configure, and the reason it
/// cannot be used. Carries the factory's own error, so the text is the same one
/// a real request would produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProblem {
    pub name: String,
    pub reason: String,
}

/// Remote providers with a bespoke (non-OpenAI-compatible) adapter, and so
/// absent from `PROVIDER_REGISTRY`. Listing them here is what stops an
/// Anthropic-only machine from being reported as having no remote provider at
/// all.
fn bespoke_providers() -> [&'static str; 3] {
    ["anthropic", "gemini", "meta"]
}

/// Every provider name Mermaid could build, before asking whether this machine
/// has the credentials: the bespoke three, the registry, and anything the user
/// declared in `[providers.<name>]`. Sorted and deduped.
fn candidate_providers(config: &Config) -> Vec<String> {
    let mut names: Vec<String> = bespoke_providers()
        .iter()
        .map(|name| name.to_string())
        .chain(PROVIDER_REGISTRY.iter().map(|p| p.name.to_string()))
        .chain(config.providers.keys().cloned())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Every remote provider this machine can use right now, sorted by name.
///
/// Empty means only a local Ollama is reachable — the one case where a missing
/// Ollama is genuinely a dead end.
#[must_use]
pub fn configured_remote_providers(config: &Config) -> Vec<ConfiguredProvider> {
    candidate_providers(config)
        .into_iter()
        .filter_map(|name| {
            let endpoint = resolve_provider_endpoint(config, &name).ok()?;
            Some(ConfiguredProvider {
                name,
                env_var: endpoint.key_env,
                keyless: endpoint.api_key.is_none(),
                endpoint: endpoint.base_url,
            })
        })
        .collect()
}

/// Just the names from [`configured_remote_providers`].
#[must_use]
pub fn configured_remote_provider_names(config: &Config) -> Vec<String> {
    configured_remote_providers(config)
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

/// Providers the user has started configuring that still cannot be used.
///
/// "Started configuring" is deliberately narrow — a `[providers.<name>]` block
/// exists, or a key resolves — because every provider in the registry is
/// unusable on a machine with no keys, and reporting fifteen of those as
/// problems would bury the one that matters. The classic hit is Cloudflare with
/// a token but no `CLOUDFLARE_ACCOUNT_ID`.
#[must_use]
pub fn provider_problems(config: &Config) -> Vec<ProviderProblem> {
    candidate_providers(config)
        .into_iter()
        .filter_map(|name| {
            let Err(error) = resolve_provider_endpoint(config, &name) else {
                return None;
            };
            let attempted = config.providers.contains_key(&name) || any_key_resolves(config, &name);
            attempted.then(|| ProviderProblem {
                name,
                reason: error.to_string(),
            })
        })
        .collect()
}

/// Whether a key for `name` resolves from any of its accepted sources. Used
/// only to decide if the user *meant* to configure a provider that then failed
/// for some other reason — never as the definition of "configured".
fn any_key_resolves(config: &Config, name: &str) -> bool {
    let override_env = config
        .providers
        .get(name)
        .and_then(|provider| provider.api_key_env.as_deref());
    if name == "gemini" {
        return mermaid_model::utils::resolve_provider_key_with_fallback(
            name,
            crate::providers::model::gemini::DEFAULT_API_KEY_ENV,
            crate::providers::model::gemini::LEGACY_API_KEY_ENV,
            override_env,
        )
        .is_some();
    }
    let Some(default_env) = default_env_for(config, name) else {
        return false;
    };
    mermaid_model::utils::resolve_provider_key(name, &default_env, override_env).is_some()
}

/// How long one provider's catalog request may take. The `/model` picker opens
/// immediately and fills in, so a slow provider costs a late row, not a stalled
/// UI — but the request must not hang the discovery task forever either.
pub const CATALOG_TIMEOUT: Duration = Duration::from_secs(6);

/// What one configured provider serves, as reported by its catalog endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCatalog {
    /// The provider and where its key came from.
    pub provider: ConfiguredProvider,
    /// Bare model ids (no `provider/` prefix), sorted. `None` when the catalog
    /// could not be read — a network failure, a rejected key, or a provider
    /// with no listing endpoint. Distinct from `Some(vec![])`, which means the
    /// provider answered and serves nothing.
    pub models: Option<Vec<String>>,
}

/// The env var a provider's key lives in by default — before any per-provider
/// `api_key_env` override, which the callers apply themselves.
fn default_env_for(config: &Config, name: &str) -> Option<String> {
    match name {
        "anthropic" => return Some(crate::providers::model::anthropic::DEFAULT_API_KEY_ENV.into()),
        "gemini" => return Some(crate::providers::model::gemini::DEFAULT_API_KEY_ENV.into()),
        "meta" => return Some(crate::providers::model::meta::DEFAULT_API_KEY_ENV.into()),
        _ => {},
    }
    if let Some(profile) = mermaid_model::models::lookup_provider(name) {
        return Some(profile.api_key_env.to_string());
    }
    // A user-defined `[providers.<name>]`: its `api_key_env` IS the default.
    config
        .providers
        .get(name)
        .and_then(|provider| provider.api_key_env.clone())
}

/// Anthropic pins its wire format by date header, same as the chat adapter.
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// One page big enough to hold every provider's catalog. Anthropic and Gemini
/// both paginate with small defaults (20 and 50), and both cap the page at
/// 1000 — so one page is the whole list in practice.
const CATALOG_PAGE_SIZE: usize = 1000;

/// Every configured provider's model catalog, fetched concurrently.
///
/// Best-effort throughout: a provider that cannot be reached yields
/// `models: None` rather than failing the batch, because the caller's job is to
/// show the user what IS available.
pub async fn provider_catalogs(config: &Config) -> Vec<ProviderCatalog> {
    let providers = configured_remote_providers(config);
    let client = match reqwest::Client::builder().timeout(CATALOG_TIMEOUT).build() {
        Ok(client) => client,
        // No HTTP client means no catalog anywhere — still report the providers
        // themselves, which is what the key resolution already proved.
        Err(_) => {
            return providers
                .into_iter()
                .map(|provider| ProviderCatalog {
                    provider,
                    models: None,
                })
                .collect();
        },
    };
    futures::future::join_all(providers.into_iter().map(|provider| {
        let client = client.clone();
        async move {
            let models = fetch_catalog(&client, config, &provider).await;
            ProviderCatalog { provider, models }
        }
    }))
    .await
}

/// One provider's bare model ids, or `None` if the catalog could not be read.
async fn fetch_catalog(
    client: &reqwest::Client,
    config: &Config,
    provider: &ConfiguredProvider,
) -> Option<Vec<String>> {
    let name = provider.name.as_str();
    // The endpoint is already on `provider`; the key is not (it is a secret and
    // has no business sitting in a struct the CLI prints). Re-resolve it here,
    // through the same function that decided the provider was usable at all.
    let api_key = resolve_provider_endpoint(config, name).ok()?.api_key?;
    let base = provider.endpoint.trim_end_matches('/');
    // Gemini reports far more than chat models (embedders, tuned copies), so it
    // needs both a bigger page and its own row filter below.
    let mut request = match name {
        "gemini" => client
            .get(format!("{base}/models?pageSize={CATALOG_PAGE_SIZE}"))
            .header("x-goog-api-key", &api_key),
        "anthropic" => client
            .get(format!("{base}/models?limit={CATALOG_PAGE_SIZE}"))
            .header("x-api-key", &api_key)
            .header("anthropic-version", ANTHROPIC_VERSION),
        _ => client.get(format!("{base}/models")).bearer_auth(&api_key),
    };
    // Registry providers can require analytics headers (OpenRouter) — the same
    // ones the chat adapter sends.
    if let Some(profile) = mermaid_model::models::lookup_provider(name) {
        for (header, value) in profile.extra_headers {
            request = request.header(*header, *value);
        }
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.json::<serde_json::Value>().await.ok()?;
    let mut models = match name {
        "gemini" => gemini_model_ids(&body),
        // Anthropic, Meta, and every registry provider answer in the
        // OpenAI-compatible `{ "data": [{ "id": … }] }` shape.
        _ => openai_model_ids(&body),
    };
    models.sort();
    models.dedup();
    Some(models)
}

/// Ids from an OpenAI-compatible `{ "data": [{ "id": … }] }` listing.
fn openai_model_ids(body: &serde_json::Value) -> Vec<String> {
    body.get("data")
        .and_then(|data| data.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("id").and_then(|id| id.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Ids from Gemini's `{ "models": [{ "name": "models/…" }] }` listing, keeping
/// only what `/model` could actually select: the `generateContent` models.
/// Gemini also serves embedders and tuned copies through the same endpoint.
fn gemini_model_ids(body: &serde_json::Value) -> Vec<String> {
    body.get("models")
        .and_then(|models| models.as_array())
        .map(|rows| {
            rows.iter()
                .filter(|row| {
                    row.get("supportedGenerationMethods")
                        .and_then(|methods| methods.as_array())
                        // Absent field: keep the row rather than silently drop
                        // a model on a response-shape change.
                        .is_none_or(|methods| {
                            methods
                                .iter()
                                .any(|method| method.as_str() == Some("generateContent"))
                        })
                })
                .filter_map(|row| row.get("name").and_then(|name| name.as_str()))
                .map(|name| name.trim_start_matches("models/").to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_domain::UserProviderConfig;

    /// The bespoke providers were the whole point: before this module they
    /// were absent from the "configured remote providers" set, so a machine
    /// with only `ANTHROPIC_API_KEY` reported none.
    #[test]
    fn anthropic_key_alone_counts_as_a_configured_provider() {
        temp_env::with_vars([("ANTHROPIC_API_KEY", Some("sk-test"))], || {
            let names = configured_remote_provider_names(&Config::default());
            assert!(names.contains(&"anthropic".to_string()), "got {names:?}");
        });
    }

    #[test]
    fn gemini_accepts_the_legacy_env_var() {
        temp_env::with_vars(
            [
                ("GOOGLE_API_KEY", None),
                ("GEMINI_API_KEY", Some("sk-legacy")),
            ],
            || {
                let found = configured_remote_providers(&Config::default());
                let gemini = found
                    .iter()
                    .find(|entry| entry.name == "gemini")
                    .expect("legacy GEMINI_API_KEY still configures gemini");
                assert_eq!(
                    gemini.env_var.as_deref(),
                    Some(crate::providers::model::gemini::LEGACY_API_KEY_ENV)
                );
            },
        );
    }

    #[test]
    fn empty_environment_configures_nothing() {
        temp_env::with_vars(cleared_provider_env(), || {
            // The keyring is the machine's, not the test's, so only assert on
            // the env-var half: nothing here may come from an env var.
            let from_env: Vec<_> = configured_remote_providers(&Config::default())
                .into_iter()
                .filter(|entry| entry.env_var.is_some())
                .collect();
            assert!(from_env.is_empty(), "got {from_env:?}");
        });
    }

    /// Every provider env var unset, so a test asserts on its own config rather
    /// than on whatever the developer's shell happens to export.
    fn cleared_provider_env() -> Vec<(&'static str, Option<&'static str>)> {
        [
            crate::providers::model::anthropic::DEFAULT_API_KEY_ENV,
            crate::providers::model::gemini::DEFAULT_API_KEY_ENV,
            crate::providers::model::gemini::LEGACY_API_KEY_ENV,
            crate::providers::model::meta::DEFAULT_API_KEY_ENV,
            "CLOUDFLARE_ACCOUNT_ID",
        ]
        .iter()
        .map(|env| (*env, None))
        .chain(PROVIDER_REGISTRY.iter().map(|p| (p.api_key_env, None)))
        .collect()
    }

    /// A per-provider `api_key_env` override is authoritative — the default
    /// env var must not resolve the key behind its back.
    #[test]
    fn api_key_env_override_is_authoritative() {
        let mut config = Config::default();
        config.providers.insert(
            "anthropic".to_string(),
            UserProviderConfig {
                api_key_env: Some("MY_ANTHROPIC_KEY".to_string()),
                ..Default::default()
            },
        );
        temp_env::with_vars(
            [
                ("ANTHROPIC_API_KEY", Some("sk-default")),
                ("MY_ANTHROPIC_KEY", None),
            ],
            || {
                let names = configured_remote_provider_names(&config);
                assert!(!names.contains(&"anthropic".to_string()), "got {names:?}");
            },
        );
        temp_env::with_vars(
            [
                ("ANTHROPIC_API_KEY", None),
                ("MY_ANTHROPIC_KEY", Some("sk-override")),
            ],
            || {
                let found = configured_remote_providers(&config);
                let anthropic = found
                    .iter()
                    .find(|entry| entry.name == "anthropic")
                    .expect("the override env resolves the key");
                assert_eq!(anthropic.env_var.as_deref(), Some("MY_ANTHROPIC_KEY"));
                assert_eq!(anthropic.source_label(), "via $MY_ANTHROPIC_KEY");
            },
        );
    }

    /// Registry providers keep working exactly as before this module existed.
    #[test]
    fn registry_providers_are_still_detected() {
        temp_env::with_vars([("GROQ_API_KEY", Some("gsk-test"))], || {
            let names = configured_remote_provider_names(&Config::default());
            assert!(names.contains(&"groq".to_string()), "got {names:?}");
        });
    }

    /// The bug this module's catalog half exists to fix: `/model` and
    /// `mermaid list` enumerated only `PROVIDER_REGISTRY`, so every bespoke
    /// provider's models were invisible. All three must be candidates, and all
    /// three must resolve an endpoint once their key is present.
    #[test]
    fn bespoke_providers_are_candidates_and_resolve_an_endpoint() {
        let config = Config::default();
        let candidates = candidate_providers(&config);
        for name in bespoke_providers() {
            assert!(
                candidates.contains(&name.to_string()),
                "{name} is not a candidate provider"
            );
            assert!(
                default_env_for(&config, name).is_some(),
                "{name} lost its default env var"
            );
        }
        temp_env::with_vars(
            [
                ("ANTHROPIC_API_KEY", Some("sk-a")),
                ("GOOGLE_API_KEY", Some("sk-g")),
                ("MODEL_API_KEY", Some("sk-m")),
            ],
            || {
                let found = configured_remote_providers(&config);
                for name in bespoke_providers() {
                    let entry = found
                        .iter()
                        .find(|entry| entry.name == name)
                        .unwrap_or_else(|| panic!("{name} did not resolve"));
                    assert!(!entry.endpoint.is_empty(), "{name} has no endpoint");
                    assert!(!entry.keyless, "{name} has no keyless mode");
                }
            },
        );
    }

    #[test]
    fn base_url_override_wins_and_is_reported() {
        let mut config = Config::default();
        config.providers.insert(
            "meta".to_string(),
            UserProviderConfig {
                base_url: Some("https://gw.example/v1".to_string()),
                ..Default::default()
            },
        );
        temp_env::with_vars(
            [
                ("MODEL_API_KEY", Some("sk-m")),
                ("GROQ_API_KEY", Some("gsk-g")),
            ],
            || {
                let found = configured_remote_providers(&config);
                let endpoint = |name: &str| {
                    found
                        .iter()
                        .find(|entry| entry.name == name)
                        .map(|entry| entry.endpoint.clone())
                };
                assert_eq!(endpoint("meta").as_deref(), Some("https://gw.example/v1"));
                assert_eq!(
                    endpoint("groq").as_deref(),
                    Some("https://api.groq.com/openai/v1")
                );
            },
        );
    }

    /// The whole point of routing through the factory: a keyless loopback
    /// endpoint is a real, buildable provider, and every hand-rolled walk that
    /// preceded this module dropped it on the floor because no key resolved.
    #[test]
    fn a_keyless_local_endpoint_counts_as_configured() {
        let mut config = Config::default();
        config.providers.insert(
            "llamacpp".to_string(),
            UserProviderConfig {
                base_url: Some("http://127.0.0.1:8080/v1".to_string()),
                ..Default::default()
            },
        );
        temp_env::with_vars(cleared_provider_env(), || {
            let found = configured_remote_providers(&config);
            let local = found
                .iter()
                .find(|entry| entry.name == "llamacpp")
                .expect("a keyless loopback provider is usable");
            assert!(local.keyless);
            assert_eq!(local.env_var, None);
            assert_eq!(local.source_label(), "no key needed — local endpoint");
        });
    }

    /// The mirror case: a custom provider with a key but no `base_url` cannot
    /// be built, so it is NOT configured — and it earns a problem row naming
    /// the missing field, because the user clearly meant to set it up.
    #[test]
    fn a_custom_provider_without_a_base_url_is_a_problem_not_a_provider() {
        let mut config = Config::default();
        config.providers.insert(
            "acme".to_string(),
            UserProviderConfig {
                api_key_env: Some("ACME_KEY".to_string()),
                ..Default::default()
            },
        );
        temp_env::with_vars(
            cleared_provider_env()
                .into_iter()
                .chain([("ACME_KEY", Some("sk-acme"))])
                .collect::<Vec<_>>(),
            || {
                let names = configured_remote_provider_names(&config);
                assert!(!names.contains(&"acme".to_string()), "got {names:?}");
                let problems = provider_problems(&config);
                let acme = problems
                    .iter()
                    .find(|problem| problem.name == "acme")
                    .expect("a half-configured provider is reported");
                assert!(acme.reason.contains("base_url"), "got {}", acme.reason);
            },
        );
    }

    /// A provider nobody configured must stay silent. Fifteen registry entries
    /// with no key are not fifteen problems.
    #[test]
    fn untouched_providers_are_not_reported_as_problems() {
        temp_env::with_vars(cleared_provider_env(), || {
            let problems = provider_problems(&Config::default());
            assert!(problems.is_empty(), "got {problems:?}");
        });
    }

    /// Cloudflare's endpoint embeds an account id. A token without one is the
    /// canonical half-configured provider, and the reason must name the var.
    #[test]
    fn cloudflare_without_an_account_id_is_a_problem() {
        temp_env::with_vars(
            cleared_provider_env()
                .into_iter()
                .chain([("CLOUDFLARE_API_TOKEN", Some("cf-token"))])
                .collect::<Vec<_>>(),
            || {
                let config = Config::default();
                assert!(
                    !configured_remote_provider_names(&config).contains(&"cloudflare".to_string())
                );
                let problem = provider_problems(&config)
                    .into_iter()
                    .find(|problem| problem.name == "cloudflare")
                    .expect("a token without an account id is reported");
                assert!(
                    problem.reason.contains("CLOUDFLARE_ACCOUNT_ID"),
                    "got {}",
                    problem.reason
                );
            },
        );
    }

    /// The invariant the whole refactor exists to hold: discovery lists a
    /// provider if and only if the factory can build one. Asserted across the
    /// configs that used to split the two apart.
    #[test]
    fn listing_agrees_with_what_the_factory_can_build() {
        let mut config = Config::default();
        config.providers.insert(
            "llamacpp".to_string(),
            UserProviderConfig {
                base_url: Some("http://127.0.0.1:8080/v1".to_string()),
                ..Default::default()
            },
        );
        config.providers.insert(
            "acme".to_string(),
            UserProviderConfig {
                api_key_env: Some("ACME_KEY".to_string()),
                ..Default::default()
            },
        );
        temp_env::with_vars(
            cleared_provider_env()
                .into_iter()
                .chain([
                    ("ACME_KEY", Some("sk-acme")),
                    ("ANTHROPIC_API_KEY", Some("sk-a")),
                    ("CLOUDFLARE_API_TOKEN", Some("cf-token")),
                ])
                .collect::<Vec<_>>(),
            || {
                let listed = configured_remote_provider_names(&config);
                for name in candidate_providers(&config) {
                    let buildable = resolve_provider_endpoint(&config, &name).is_ok();
                    assert_eq!(
                        listed.contains(&name),
                        buildable,
                        "{name}: listed={} buildable={buildable}",
                        listed.contains(&name),
                    );
                }
            },
        );
    }

    #[test]
    fn openai_shape_yields_ids_and_survives_junk() {
        let body = serde_json::json!({
            "object": "list",
            "data": [
                {"id": "muse-spark-1.2-contributor", "object": "model"},
                {"object": "model"},
                {"id": "muse-spark-1.1"},
            ],
        });
        assert_eq!(
            openai_model_ids(&body),
            vec!["muse-spark-1.2-contributor", "muse-spark-1.1"]
        );
        assert!(openai_model_ids(&serde_json::json!({"error": "nope"})).is_empty());
    }

    #[test]
    fn gemini_shape_strips_the_prefix_and_drops_non_chat_models() {
        let body = serde_json::json!({
            "models": [
                {
                    "name": "models/gemini-3-pro",
                    "supportedGenerationMethods": ["generateContent", "countTokens"],
                },
                {
                    "name": "models/text-embedding-004",
                    "supportedGenerationMethods": ["embedContent"],
                },
                // No methods field: kept, so a response-shape change can't
                // silently empty the list.
                {"name": "models/gemini-future"},
            ],
        });
        assert_eq!(
            gemini_model_ids(&body),
            vec!["gemini-3-pro", "gemini-future"]
        );
    }

    #[test]
    fn results_are_sorted_and_deduped() {
        temp_env::with_vars(
            [
                ("GROQ_API_KEY", Some("gsk-test")),
                ("ANTHROPIC_API_KEY", Some("sk-test")),
            ],
            || {
                let names = configured_remote_provider_names(&Config::default());
                let mut sorted = names.clone();
                sorted.sort();
                sorted.dedup();
                assert_eq!(names, sorted);
            },
        );
    }
}

//! Runtime provider construction.
//!
//! `ProviderFactory` turns `(Config, model_id)` into the right
//! `Arc<dyn ModelProvider>`. The effect runner holds one of these
//! and asks it to build a provider the first time a new model is
//! referenced; subsequent lookups hit the cache.

use std::sync::Arc;

use tokio::sync::Mutex;

use mermaid_domain::Config;
use mermaid_model::models::{ModelError, Result, lookup_provider};
use mermaid_model::utils::{
    resolve_api_key, resolve_provider_key, resolve_provider_key_with_fallback,
};

use super::model::{anthropic, gemini, meta};

/// Resolve an API key (env first, then the OS keyring via
/// `mermaid login <provider>`) or return a clear `ModelError`. A
/// per-provider `override_env` is authoritative — no keyring fallback.
fn require_key(provider: &str, default_env: &str, override_env: Option<&str>) -> Result<String> {
    resolve_provider_key(provider, default_env, override_env).ok_or_else(|| {
        let env = override_env.unwrap_or(default_env);
        ModelError::Authentication(format!(
            "{provider} requires env var {env} (or `mermaid login {provider}`)"
        ))
    })
}

fn require_key_with_fallback(
    provider: &str,
    env_var: &str,
    fallback_env_var: &str,
) -> Result<String> {
    resolve_provider_key_with_fallback(provider, env_var, fallback_env_var, None).ok_or_else(|| {
        ModelError::Authentication(format!(
            "{provider} requires env var {env_var} (or legacy {fallback_env_var}, or \
                 `mermaid login {provider}`)"
        ))
    })
}

/// Which env var supplied a key, or `None` when the keyring did.
///
/// Mirrors [`resolve_provider_key`]'s precedence exactly — it must, because the
/// two are asked the same question by different callers and a disagreement
/// would show up as `status` naming a var the factory never read.
fn key_env_source(default_env: &str, override_env: Option<&str>) -> Option<String> {
    // Only ever called after a key resolved, so "not in this env var" means the
    // keyring supplied it. An explicit override is authoritative and never
    // reaches the keyring, so for that case this always matches.
    let env = override_env.unwrap_or(default_env);
    resolve_api_key(env, None).map(|_| env.to_string())
}

/// [`key_env_source`] for the one legacy-fallback provider (Gemini).
fn key_env_source_with_fallback(default_env: &str, fallback_env: &str) -> Option<String> {
    if resolve_api_key(default_env, None).is_some() {
        return Some(default_env.to_string());
    }
    if resolve_api_key(fallback_env, None).is_some() {
        return Some(fallback_env.to_string());
    }
    None
}

/// What `build_provider` resolves before it constructs anything: the endpoint it
/// would dial and the credential it would send.
///
/// This exists so that "which providers can this machine actually talk to" has
/// exactly ONE definition. It used to have four — `build_provider`'s dispatch
/// chain plus three hand-rolled walks in `doctor`, `status`, and startup model
/// resolution — and all four disagreed about custom providers. Everything that
/// wants the answer now reads it through `providers::discovery`, which calls
/// this and treats `Err` as "not usable".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderEndpoint {
    /// The base URL the adapter would be built with, overrides applied.
    pub(crate) base_url: String,
    /// `None` only where a keyless endpoint is legal: a loopback/LAN host
    /// running an unauthenticated OpenAI-compatible server.
    pub(crate) api_key: Option<String>,
    /// Env var the key came from. `None` means the keyring supplied it, or the
    /// endpoint is keyless.
    pub(crate) key_env: Option<String>,
}

impl ProviderEndpoint {
    /// The key, for the bespoke adapters that cannot run keyless.
    ///
    /// Total in practice: [`resolve_provider_endpoint`] has already failed with
    /// the provider's own actionable message if a required key was missing.
    /// This exists so those branches need no `expect`.
    fn require_key(&self, provider: &str) -> Result<String> {
        self.api_key
            .clone()
            .ok_or_else(|| ModelError::Authentication(format!("{provider} requires an API key")))
    }
}

/// True when Mermaid knows how to build `provider` at all — before asking
/// whether this machine has the credentials for it.
fn is_known_provider(config: &Config, provider_lc: &str) -> bool {
    matches!(provider_lc, "anthropic" | "gemini" | "meta")
        || lookup_provider(provider_lc).is_some()
        || config.providers.contains_key(provider_lc)
}

/// Resolve `provider`'s endpoint and credential exactly as `build_provider`
/// would, without constructing an adapter or touching the network.
///
/// `Err` is the honest answer to "can this machine use this provider": a missing
/// key, a Cloudflare account id that was never set, a custom provider with no
/// `base_url`. The error text is the same one the user would hit on a real
/// request, so `status` and `doctor` can print it verbatim.
pub(crate) fn resolve_provider_endpoint(
    config: &Config,
    provider: &str,
) -> Result<ProviderEndpoint> {
    let provider_lc = provider.to_lowercase();
    let user_cfg = config.providers.get(&provider_lc);
    let override_env = user_cfg.and_then(|c| c.api_key_env.as_deref());
    let override_url = user_cfg.and_then(|c| c.base_url.clone());

    // Bespoke adapters (Anthropic, Gemini, Meta). All three are public
    // endpoints with no keyless deployment story, so the key is required.
    let bespoke = match provider_lc.as_str() {
        "anthropic" => Some((
            anthropic::DEFAULT_API_KEY_ENV,
            None,
            anthropic::DEFAULT_BASE_URL,
        )),
        "gemini" => Some((
            gemini::DEFAULT_API_KEY_ENV,
            Some(gemini::LEGACY_API_KEY_ENV),
            gemini::DEFAULT_BASE_URL,
        )),
        "meta" => Some((meta::DEFAULT_API_KEY_ENV, None, meta::DEFAULT_BASE_URL)),
        _ => None,
    };
    if let Some((default_env, legacy_env, default_url)) = bespoke {
        let base_url = resolve_overridable_base_url(&provider_lc, override_url, default_url)?;
        let (api_key, key_env) = match legacy_env {
            // The legacy var is consulted only when the user has NOT pointed at
            // a specific one (matches `require_key_with_fallback`).
            Some(legacy) if override_env.is_none() => (
                require_key_with_fallback(&provider_lc, default_env, legacy)?,
                key_env_source_with_fallback(default_env, legacy),
            ),
            _ => (
                require_key(&provider_lc, default_env, override_env)?,
                key_env_source(default_env, override_env),
            ),
        };
        return Ok(ProviderEndpoint {
            base_url,
            api_key: Some(api_key),
            key_env,
        });
    }

    // Cloudflare Workers AI — OpenAI-compatible, but the endpoint URL embeds a
    // per-account id, so the base_url is synthesized at runtime from
    // CLOUDFLARE_ACCOUNT_ID (or a full [providers.cloudflare].base_url override,
    // e.g. AI Gateway). Must precede the generic registry branch, which would
    // otherwise route it through the placeholder profile.base_url.
    if provider_lc == "cloudflare" {
        return resolve_cloudflare_endpoint(&provider_lc, override_env, override_url);
    }

    // The OpenAI-compatible registry.
    if let Some(profile) = lookup_provider(&provider_lc) {
        let base_url = resolve_overridable_base_url(&provider_lc, override_url, profile.base_url)?;
        // Keyless when the key is unset and the endpoint is local (a registry
        // provider pointed at a loopback/LAN base_url); otherwise a clear,
        // hint-carrying auth error.
        let api_key = resolve_optional_key(
            &provider_lc,
            profile.api_key_env,
            override_env,
            &base_url,
            profile.key_hint,
        )?;
        let key_env = api_key
            .is_some()
            .then(|| key_env_source(profile.api_key_env, override_env))
            .flatten();
        return Ok(ProviderEndpoint {
            base_url,
            api_key,
            key_env,
        });
    }

    // User-custom: no registry entry, but the user has [providers.<name>] in
    // config. `base_url` is mandatory here — there is no default to fall back on.
    if user_cfg.is_some() {
        return resolve_custom_endpoint(&provider_lc, override_env, override_url);
    }

    Err(ModelError::InvalidRequest(format!(
        "Unknown provider '{provider}'"
    )))
}

/// Cloudflare Workers AI — OpenAI-compatible, but the endpoint URL embeds a
/// per-account id, so the base_url is synthesized at runtime from
/// CLOUDFLARE_ACCOUNT_ID (or a full [providers.cloudflare].base_url override,
/// e.g. AI Gateway).
fn resolve_cloudflare_endpoint(
    provider_lc: &str,
    override_env: Option<&str>,
    override_url: Option<String>,
) -> Result<ProviderEndpoint> {
    let profile = lookup_provider("cloudflare").expect("cloudflare is in the registry");
    let api_key_env = override_env.unwrap_or(profile.api_key_env);
    let base_url = match override_url {
        // Override present (AI Gateway / proxy): validate + warn like any
        // built-in override. The account id isn't needed in this case.
        Some(url) => {
            validate_provider_base_url(&url)?;
            warn_overridden_provider_host("cloudflare", &url);
            url
        },
        // Standard path: synthesize the account-scoped endpoint from env. A
        // fresh setup missing the token as well gets one error naming both
        // vars, not two fix-and-retry round-trips.
        None => match require_cloudflare_account_id() {
            Ok(id) => cloudflare_base_url(&id),
            Err(_)
                if resolve_provider_key("cloudflare", profile.api_key_env, override_env)
                    .is_none() =>
            {
                return Err(ModelError::Authentication(format!(
                    "cloudflare requires env vars {api_key_env} and CLOUDFLARE_ACCOUNT_ID — \
                     create a token at https://dash.cloudflare.com/profile/api-tokens; the \
                     account id is on your Cloudflare dashboard (or set \
                     [providers.cloudflare].base_url)"
                )));
            },
            Err(e) => return Err(e),
        },
    };
    let api_key = resolve_optional_key(
        provider_lc,
        profile.api_key_env,
        override_env,
        &base_url,
        profile.key_hint,
    )?;
    let key_env = api_key
        .is_some()
        .then(|| key_env_source(profile.api_key_env, override_env))
        .flatten();
    Ok(ProviderEndpoint {
        base_url,
        api_key,
        key_env,
    })
}

/// User-custom: no registry entry, but the user has [providers.<name>] in
/// config. `base_url` is mandatory here — there is no default to fall back on.
fn resolve_custom_endpoint(
    provider_lc: &str,
    override_env: Option<&str>,
    override_url: Option<String>,
) -> Result<ProviderEndpoint> {
    let base_url = override_url.ok_or_else(|| {
        ModelError::InvalidRequest(format!(
            "custom provider '{provider_lc}' requires base_url in config"
        ))
    })?;
    // api_key_env is optional: a local (loopback/LAN) endpoint may run
    // keyless. When a key IS used, harden the URL so it can't be sent in
    // cleartext; a keyless endpoint must be local (no secret to leak).
    // For a CUSTOM provider its api_key_env is the default (not an override
    // of a registry default), so the keyring may fill the gap; a stored key
    // alone also works with no api_key_env at all.
    let resolved = match override_env {
        Some(env) => resolve_provider_key(provider_lc, env, None),
        None => mermaid_model::utils::default_store().get(provider_lc),
    };
    let (api_key, key_env) = match resolved {
        Some(key) => {
            validate_provider_base_url(&base_url)?;
            let env = override_env.and_then(|env| key_env_source(env, None));
            (Some(key), env)
        },
        None if base_url_is_local(&base_url) => (None, None),
        None => {
            let reason = match override_env {
                Some(env) => format!(
                    "requires env var {env} (or `mermaid login {provider_lc}`, or a \
                     loopback/LAN base_url)"
                ),
                None => "requires api_key_env, or a loopback/LAN base_url".to_string(),
            };
            return Err(ModelError::Authentication(format!(
                "custom provider '{provider_lc}' {reason}"
            )));
        },
    };
    Ok(ProviderEndpoint {
        base_url,
        api_key,
        key_env,
    })
}

/// Resolve an API key, or `None` when the key is absent AND the endpoint is a
/// loopback/LAN host — local OpenAI-compatible servers (llama.cpp, vLLM, LM
/// Studio) legitimately run without auth. A missing key for a public endpoint
/// is a hard error carrying the provider's `hint` so the message is actionable.
fn resolve_optional_key(
    provider: &str,
    default_env: &str,
    override_env: Option<&str>,
    base_url: &str,
    hint: Option<&str>,
) -> Result<Option<String>> {
    if let Some(key) = resolve_provider_key(provider, default_env, override_env) {
        return Ok(Some(key));
    }
    if base_url_is_local(base_url) {
        return Ok(None);
    }
    let env = override_env.unwrap_or(default_env);
    let mut msg = format!("{provider} requires env var {env} (or `mermaid login {provider}`)");
    if let Some(h) = hint {
        msg.push_str(" — ");
        msg.push_str(h);
    }
    Err(ModelError::Authentication(msg))
}

/// True when `base_url`'s host is a loopback or private/LAN address — where a
/// local model server legitimately runs without auth.
fn base_url_is_local(base_url: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| mermaid_model::utils::classify_host(h).is_internal())
        })
        .unwrap_or(false)
}

/// The header set sent on every request: the profile's static `extra_headers`
/// first, then user `extra_headers` overrides, then env-sourced `env_headers`
/// (header name -> env var, resolved now since env is process-static). This
/// restores the profile's static headers (e.g. OpenRouter analytics) that the
/// prior registry path dropped.
fn merged_headers(
    profile: &mermaid_model::models::ProviderProfile,
    user_cfg: Option<&mermaid_domain::UserProviderConfig>,
) -> std::collections::HashMap<String, String> {
    let mut headers: std::collections::HashMap<String, String> = profile
        .extra_headers
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    if let Some(cfg) = user_cfg {
        for (k, v) in &cfg.extra_headers {
            headers.insert(k.clone(), v.clone());
        }
        for (header, env_var) in &cfg.env_headers {
            if let Ok(val) = std::env::var(env_var) {
                headers.insert(header.clone(), val);
            }
        }
    }
    headers
}

use super::model::{
    AnthropicProvider, GeminiProvider, MetaProvider, ModelProvider, OllamaProvider,
    OpenAICompatProvider,
};

/// A lazily-built, shared provider. `OnceCell` gives single-flight construction
/// (#84); the outer `Arc` lets `resolve` clone the cell out from under the cache
/// lock and initialize it without holding the lock across the build.
type ProviderCell = Arc<tokio::sync::OnceCell<Arc<dyn ModelProvider>>>;

/// Per-process provider cache. Providers are expensive to construct
/// (HTTP client, connection pool, capability lookup) so the effect
/// runner asks for them lazily and reuses across turns.
pub struct ProviderFactory {
    config: Arc<Config>,
    /// Per-key cache of built providers, keyed by normalized model id (#83). The
    /// `Mutex` is only held to get-or-insert a cell, never across the build.
    cache: Mutex<std::collections::HashMap<String, ProviderCell>>,
}

impl ProviderFactory {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            cache: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// A factory with providers already resolved for the given model ids.
    ///
    /// `resolve` hands these back instead of building anything, so a caller
    /// can substitute a provider it constructed itself. This is how tests
    /// drive the real agent loop — reducer, effect runner, tool registry,
    /// subagents — against a scripted model instead of a network: the seam
    /// is the provider, so everything below it is the production path.
    ///
    /// Seeding is additive. A model id that was not seeded still builds
    /// normally, which lets a test stub one model and leave the rest alone.
    pub fn with_seeded_providers(
        config: Config,
        seeds: impl IntoIterator<Item = (String, Arc<dyn ModelProvider>)>,
    ) -> Self {
        let cache = seeds
            .into_iter()
            .map(|(model_id, provider)| {
                let cell = tokio::sync::OnceCell::new_with(Some(provider));
                (normalize_cache_key(&model_id), Arc::new(cell))
            })
            .collect();
        Self {
            config: Arc::new(config),
            cache: Mutex::new(cache),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Resolve (or lazily construct) a provider for the given model
    /// ID. Hits the cache on the second and subsequent calls for the
    /// same ID.
    ///
    /// # Errors
    ///
    /// Whatever building the provider fails with: an unknown provider prefix,
    /// a provider whose API key does not resolve, or the adapter's own client
    /// build. A failed build is deliberately not cached, so a transient error
    /// is retried on the next call rather than pinned for the session.
    pub async fn resolve(&self, model_id: &str) -> Result<Arc<dyn ModelProvider>> {
        let key = normalize_cache_key(model_id);
        // Get-or-insert the per-key cell under a brief lock, then initialize it
        // exactly once outside the lock (#84). A failed build isn't cached, so a
        // transient error can be retried on the next call.
        let cell = {
            let mut cache = self.cache.lock().await;
            Arc::clone(
                cache
                    .entry(key)
                    .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new())),
            )
        };
        let provider = cell
            .get_or_try_init(|| async {
                let p = build_provider(&self.config, model_id).await?;
                Ok::<Arc<dyn ModelProvider>, ModelError>(Arc::from(p))
            })
            .await?;
        Ok(Arc::clone(provider))
    }
}

/// Build a provider for the given `model_id`:
///   1. `ollama/<model>` → `OllamaProvider`.
///   2. `anthropic/<model>` → `AnthropicProvider`.
///   3. `gemini/<model>` → `GeminiProvider`.
///   4. `meta/<model>` → `MetaProvider` using the Responses API.
///   5. Other builtin providers (openai, openrouter, groq, …) → `OpenAICompatProvider`.
///   6. User-defined `[providers.<name>]` → custom `OpenAICompatProvider`.
///   7. Bare model name → `OllamaProvider`.
async fn build_provider(config: &Config, model_id: &str) -> Result<Box<dyn ModelProvider>> {
    let (provider, model_name) = parse_model_id(model_id);
    let provider_lc = provider.to_lowercase();

    // 1. Ollama (and bare names). F11: pass Arc<Config> so the wrapper
    // can forward Ollama hardware options to the adapter.
    if provider_lc == "ollama" {
        let backend = crate::ollama::backend_config(config);
        let p = OllamaProvider::with_app_config(
            model_name,
            Arc::new(backend),
            Arc::new(config.clone()),
        )
        .await?;
        return Ok(Box::new(p));
    }

    // Everything past Ollama is a remote provider, and ONE function decides
    // where it lives and what credential it takes — the same one `discovery`
    // asks on behalf of `status`, `doctor`, `list`, and `/model`. The branches
    // below only turn that answer into the right adapter.
    if !is_known_provider(config, &provider_lc) {
        return Err(ModelError::InvalidRequest(format!(
            "Unknown provider '{provider}' (model_id: {model_id})"
        )));
    }
    let endpoint = resolve_provider_endpoint(config, &provider_lc)?;
    let user_cfg = config.providers.get(&provider_lc);

    // 2. Anthropic — bespoke API shape.
    if provider_lc == "anthropic" {
        let p = AnthropicProvider::new(
            endpoint.require_key(&provider_lc)?,
            model_name.to_string(),
            endpoint.base_url,
        )?;
        return Ok(Box::new(p));
    }

    // 3. Gemini — GCP AI Studio shape.
    if provider_lc == "gemini" {
        let p = GeminiProvider::new(
            endpoint.require_key(&provider_lc)?,
            model_name.to_string(),
            endpoint.base_url,
        )?;
        return Ok(Box::new(p));
    }

    // 4. Meta — Responses is required for encrypted reasoning continuity across
    // Mermaid's tool loop even though Meta also exposes Chat Completions.
    if provider_lc == "meta" {
        let api_key = endpoint.require_key(&provider_lc)?;
        let mut extra_headers = std::collections::HashMap::new();
        if let Some(cfg) = user_cfg {
            extra_headers.extend(cfg.extra_headers.clone());
            for (header, env_var) in &cfg.env_headers {
                if let Ok(value) = std::env::var(env_var) {
                    extra_headers.insert(header.clone(), value);
                }
            }
        }
        let p = MetaProvider::new(
            api_key,
            model_name.to_string(),
            endpoint.base_url,
            extra_headers,
        )?;
        return Ok(Box::new(p));
    }

    // 5 + 6. Everything else is OpenAI-compatible: the built-in registry
    // (Cloudflare's account-scoped URL included — `resolve_provider_endpoint`
    // synthesized it) or a user-defined `[providers.<name>]`.
    let profile = match lookup_provider(&provider_lc) {
        Some(profile) => profile,
        // `is_known_provider` already proved the config entry exists, and
        // `user_profile_to_static` is total, so this cannot fall through.
        None => user_cfg
            .and_then(|cfg| user_profile_to_static(&provider_lc, cfg))
            .ok_or_else(|| {
                ModelError::InvalidRequest(format!(
                    "Unknown provider '{provider}' (model_id: {model_id})"
                ))
            })?,
    };
    let extra_headers = merged_headers(profile, user_cfg);
    let p = OpenAICompatProvider::new(
        profile,
        endpoint.base_url,
        endpoint.api_key,
        model_name.to_string(),
        extra_headers,
    )?;
    Ok(Box::new(p))
}

/// Cache key for a model id: the provider segment lowercased (matching
/// `build_provider`'s own normalization) so `Anthropic/x` and `anthropic/x`
/// resolve to one cached provider instead of building two identical ones (#83).
fn normalize_cache_key(model_id: &str) -> String {
    let (provider, model) = parse_model_id(model_id);
    format!("{}/{}", provider.to_lowercase(), model)
}

/// Parse `provider/model` → `(provider, model)`. Bare strings are
/// Ollama by convention.
fn parse_model_id(model_id: &str) -> (String, &str) {
    match model_id.split_once('/') {
        Some((p, m)) => (p.to_string(), m),
        None => ("ollama".to_string(), model_id),
    }
}

/// Whether `model_id`'s provider could be built on this machine right now.
///
/// Ollama ids (bare or `ollama/…`) always pass — the local backend needs no
/// credential. Remote ids pass iff [`resolve_provider_endpoint`] does, the
/// same single definition of "buildable" that discovery and `doctor` use.
///
/// This gates persisting `last_used_model`: a `--model` whose provider
/// provably cannot be built here — a keyless test invocation, a typo'd
/// provider name — must not become the startup default that every later
/// session resolves first. (Test binaries writing `anthropic/pty-*-test`
/// into the developer's real config were exactly this hole.)
#[must_use]
pub fn model_provider_resolves(config: &Config, model_id: &str) -> bool {
    let (provider, _) = parse_model_id(model_id);
    provider.eq_ignore_ascii_case("ollama") || resolve_provider_endpoint(config, &provider).is_ok()
}

/// Process-wide memo of leaked custom-provider profiles, keyed by the
/// profile-determining inputs (see [`user_profile_to_static`]). Guarantees each
/// distinct custom provider leaks its `&'static ProviderProfile` at most once.
static PROFILE_CACHE: std::sync::LazyLock<
    std::sync::Mutex<
        std::collections::HashMap<String, &'static mermaid_model::models::ProviderProfile>,
    >,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Convert a user-defined `[providers.<name>]` entry into a `&'static
/// ProviderProfile`. `ProviderProfile`'s fields are `&'static` (tied to the
/// registry constants), so a custom provider needs a leaked, owned copy to
/// participate without redesigning the profile type.
///
/// The leak is memoized (F67). `build_provider` runs once per distinct *model
/// id*, so without a cache this leaked a fresh profile for every custom
/// `provider/model` pair — a permanent, per-distinct-model_id growth, not the
/// "0-3" the old comment claimed. The profile's content depends only on
/// (provider name, `base_url`, `api_key_env`, compat) and NOT on the model, so we
/// key the cache on exactly those and leak at most once per distinct
/// combination; repeated resolves (including different models of the same custom
/// provider) reuse the same `&'static`.
fn user_profile_to_static(
    name: &str,
    user_cfg: &mermaid_domain::UserProviderConfig,
) -> Option<&'static mermaid_model::models::ProviderProfile> {
    use mermaid_model::models::{ProviderProfile, ReasoningExtraction, ReasoningStrategy};

    let compat = user_cfg.compat.as_deref().unwrap_or("openai");
    let base_url = user_cfg.base_url.clone().unwrap_or_default();
    let api_key_env = user_cfg.api_key_env.clone().unwrap_or_default();

    // Stable key over exactly the fields baked into the profile below. NUL
    // separates components so distinct inputs can't collide into one key.
    let cache_key = format!("{name}\u{0}{base_url}\u{0}{api_key_env}\u{0}{compat}");

    let mut cache = PROFILE_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(profile) = cache.get(cache_key.as_str()) {
        // `&'static ProviderProfile` is Copy, so this hands back the same leaked
        // allocation as the first resolve — no new leak.
        return Some(*profile);
    }

    let strategy = match compat {
        "openai" => ReasoningStrategy::None,
        "openai-effort" => ReasoningStrategy::Effort,
        "openrouter" => ReasoningStrategy::OpenRouterShape,
        _ => ReasoningStrategy::None,
    };

    let profile = Box::new(ProviderProfile {
        name: Box::leak(name.to_string().into_boxed_str()),
        base_url: Box::leak(base_url.into_boxed_str()),
        api_key_env: Box::leak(api_key_env.into_boxed_str()),
        key_hint: None,
        extra_headers: &[],
        reasoning_strategy: strategy,
        reasoning_extraction: ReasoningExtraction::None,
        max_tokens_param: mermaid_model::models::MaxTokensParam::MaxTokens,
        disable_parallel_tool_calls_for: &[],
    });
    let leaked: &'static ProviderProfile = Box::leak(profile);
    cache.insert(cache_key, leaked);
    Some(leaked)
}

/// Build Cloudflare Workers AI's account-scoped OpenAI-compatible base URL. The
/// adapter appends `/chat/completions`, so this ends at `/ai/v1`. Kept pure and
/// separate so it's directly unit-testable (a built provider exposes no `base_url`
/// getter).
fn cloudflare_base_url(account_id: &str) -> String {
    format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/ai/v1",
        account_id.trim()
    )
}

/// Best-effort `base_url` for *discovery* surfaces (`doctor`'s provider list,
/// the `/models` probe) — chat requests never use this; `build_provider` has its
/// own resolution. A user override wins for any provider; cloudflare synthesizes
/// its account-scoped URL from `CLOUDFLARE_ACCOUNT_ID` and yields `None` when
/// the var is unset (there is no real endpoint then — probing the registry
/// placeholder would just guarantee a 404); everything else uses the profile's
/// static default.
pub(crate) fn discovery_base_url(
    profile: &mermaid_model::models::ProviderProfile,
    override_url: Option<String>,
) -> Option<String> {
    if override_url.is_some() {
        return override_url;
    }
    if profile.name == "cloudflare" {
        return require_cloudflare_account_id()
            .ok()
            .map(|id| cloudflare_base_url(&id));
    }
    Some(profile.base_url.to_string())
}

/// Resolve the Cloudflare account id from `CLOUDFLARE_ACCOUNT_ID` (trimmed,
/// non-empty), or a clear actionable error. It isn't a secret, but it's required
/// to construct the account-scoped Workers AI endpoint; `resolve_api_key` is
/// reused only for its empty→None handling.
fn require_cloudflare_account_id() -> Result<String> {
    resolve_api_key("CLOUDFLARE_ACCOUNT_ID", None)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ModelError::Authentication(
                "cloudflare requires env var CLOUDFLARE_ACCOUNT_ID (your Cloudflare account id) — \
                 find it on your Cloudflare dashboard, or set [providers.cloudflare].base_url"
                    .to_string(),
            )
        })
}

/// Validate a provider `base_url` before it's handed an API key. Requires
/// http/https, and **requires https for any non-loopback, non-private host** —
/// a typo'd or hostile `http://` endpoint would otherwise receive the bearer
/// key in cleartext. Plain http stays allowed for loopback / RFC-1918 hosts so
/// local model servers (Ollama, vLLM) keep working.
fn validate_provider_base_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|e| {
        ModelError::InvalidRequest(format!("invalid provider base_url '{url}': {e}"))
    })?;
    match parsed.scheme() {
        "https" => Ok(()),
        // Plaintext http is only safe to LOOPBACK: a key sent over http to any
        // other host — including a LAN/private one — crosses the wire in
        // cleartext. Every caller here is a key-bearing provider; keyless local
        // Ollama uses a separate path that doesn't validate.
        "http"
            if mermaid_model::utils::classify_host(parsed.host_str().unwrap_or_default())
                .is_loopback() =>
        {
            Ok(())
        },
        "http" => Err(ModelError::InvalidRequest(format!(
            "provider base_url '{url}' uses http:// to a non-loopback host — refusing to send the \
             API key in cleartext. Use https, or http://localhost for a local server."
        ))),
        other => Err(ModelError::InvalidRequest(format!(
            "provider base_url '{url}' has unsupported scheme '{other}' (use http or https)"
        ))),
    }
}

/// Resolve a *built-in* provider's `base_url`, honoring a user override but
/// hardening it (F66). Built-in providers (anthropic, gemini, and the
/// registry-backed OpenAI-compatible ones) ship a trusted default endpoint; a
/// `[providers.<name>] base_url` override redirects that provider's API key to a
/// host the user chose. The override lives in the user's own config, so we allow
/// it — but we also:
///   * validate the scheme via [`validate_provider_base_url`], which already
///     requires https for any non-loopback host, so an `http://` override can't
///     leak the key in cleartext (http stays allowed only for
///     `localhost/127.0.0.1/::1` local dev), and
///   * emit a one-time warning naming the host the key will be sent to, so a
///     redirect to an unexpected host (e.g. `https://attacker.example`) is
///     visible rather than silent.
///
/// With no override the trusted default is returned unchanged (no warning).
fn resolve_overridable_base_url(
    provider: &str,
    override_url: Option<String>,
    default_url: &str,
) -> Result<String> {
    match override_url {
        Some(url) => {
            validate_provider_base_url(&url)?;
            warn_overridden_provider_host(provider, &url);
            Ok(url)
        },
        None => Ok(default_url.to_string()),
    }
}

/// Hosts already warned about (per provider) for a built-in `base_url` override,
/// so the F66 warning fires once per process rather than on every `resolve`.
/// Keyed by `"<provider>@<host>"`.
static WARNED_OVERRIDE_HOSTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// Emit a one-time `tracing::warn!` (deduped per provider+host) naming the host a
/// built-in provider's API key will be sent to because its trusted default
/// `base_url` was overridden in config (F66).
fn warn_overridden_provider_host(provider: &str, base_url: &str) {
    let host = provider_host(base_url);
    if should_warn_once(&format!("{provider}@{host}")) {
        tracing::warn!(
            "built-in provider '{}' base_url overridden in config: the {} API key will be sent to \
             host '{}' instead of the trusted default endpoint",
            provider,
            provider,
            host
        );
    }
}

/// Host portion of a `base_url` for the override warning, or `"<unknown>"` if it
/// can't be parsed (the caller already validated it, so this is belt-and-braces).
fn provider_host(base_url: &str) -> String {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// Record `key` in the process-wide warned-set, returning `true` the first time
/// (i.e. "warn now") and `false` thereafter. Poison-tolerant.
fn should_warn_once(key: &str) -> bool {
    let mut warned = WARNED_OVERRIDE_HOSTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    warned.insert(key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_is_local_classifies_hosts() {
        assert!(base_url_is_local("http://127.0.0.1:8000/v1"));
        assert!(base_url_is_local("http://localhost:1234/v1"));
        assert!(base_url_is_local("http://192.168.1.5:8000/v1"));
        assert!(!base_url_is_local("https://api.openai.com/v1"));
        assert!(!base_url_is_local("not a url"));
    }

    #[test]
    fn merged_headers_keeps_static_profile_headers_and_user_overrides() {
        let profile = mermaid_model::models::lookup_provider("openrouter").unwrap();
        // No user config: the static profile headers survive (the latent-bug fix).
        let base = merged_headers(profile, None);
        assert_eq!(
            base.get("X-OpenRouter-Title").map(String::as_str),
            Some("Mermaid")
        );
        assert!(base.contains_key("HTTP-Referer"));
        // User extra_headers merge on top and can override a static one.
        let mut cfg = mermaid_domain::UserProviderConfig::default();
        cfg.extra_headers.insert("X-Custom".into(), "v".into());
        cfg.extra_headers
            .insert("X-OpenRouter-Title".into(), "Override".into());
        let merged = merged_headers(profile, Some(&cfg));
        assert_eq!(merged.get("X-Custom").map(String::as_str), Some("v"));
        assert_eq!(
            merged.get("X-OpenRouter-Title").map(String::as_str),
            Some("Override")
        );
        assert!(merged.contains_key("HTTP-Referer"));
    }

    #[test]
    fn merged_headers_resolves_env_headers_and_skips_missing() {
        let profile = mermaid_model::models::lookup_provider("openai").unwrap();
        let mut cfg = mermaid_domain::UserProviderConfig::default();
        cfg.env_headers
            .insert("X-Gateway-Token".into(), "MERMAID_TEST_GW_TOKEN".into());
        temp_env::with_var("MERMAID_TEST_GW_TOKEN", Some("secret123"), || {
            let merged = merged_headers(profile, Some(&cfg));
            assert_eq!(
                merged.get("X-Gateway-Token").map(String::as_str),
                Some("secret123")
            );
        });
        temp_env::with_var("MERMAID_TEST_GW_TOKEN", None::<&str>, || {
            assert!(!merged_headers(profile, Some(&cfg)).contains_key("X-Gateway-Token"));
        });
    }

    #[test]
    fn base_url_requires_https_for_remote_hosts() {
        // Remote http is refused (would leak the bearer key in cleartext).
        assert!(validate_provider_base_url("http://api.example.com/v1").is_err());
        assert!(validate_provider_base_url("ftp://example.com").is_err());
        // https anywhere and http to LOOPBACK are fine.
        assert!(validate_provider_base_url("https://api.example.com/v1").is_ok());
        assert!(validate_provider_base_url("http://localhost:11434/v1").is_ok());
        assert!(validate_provider_base_url("http://127.0.0.1:8000").is_ok());
        assert!(validate_provider_base_url("http://[::1]:8000").is_ok());
        // #26: http to a non-loopback host (even a private LAN one) is refused —
        // the API key would otherwise cross the wire in cleartext.
        assert!(validate_provider_base_url("http://192.168.1.5:8080").is_err());
        assert!(validate_provider_base_url("http://169.254.169.254").is_err());
    }

    #[test]
    fn cloudflare_base_url_synthesizes_account_scoped_endpoint() {
        assert_eq!(
            cloudflare_base_url("acct123"),
            "https://api.cloudflare.com/client/v4/accounts/acct123/ai/v1"
        );
        // Trims surrounding whitespace (e.g. a trailing newline from `export`).
        assert_eq!(
            cloudflare_base_url("  acct123\n"),
            "https://api.cloudflare.com/client/v4/accounts/acct123/ai/v1"
        );
    }

    #[test]
    fn cloudflare_account_id_required_and_non_blank() {
        // Unset → clear, actionable error naming the env var.
        temp_env::with_var("CLOUDFLARE_ACCOUNT_ID", None::<&str>, || {
            let err = require_cloudflare_account_id().expect_err("must error when unset");
            assert!(format!("{err}").contains("CLOUDFLARE_ACCOUNT_ID"));
        });
        // Whitespace-only is treated as unset.
        temp_env::with_var("CLOUDFLARE_ACCOUNT_ID", Some("   "), || {
            assert!(require_cloudflare_account_id().is_err());
        });
        // A real value resolves, trimmed.
        temp_env::with_var("CLOUDFLARE_ACCOUNT_ID", Some(" acct123 "), || {
            assert_eq!(require_cloudflare_account_id().unwrap(), "acct123");
        });
    }

    #[test]
    fn discovery_base_url_resolves_per_provider() {
        let cf = lookup_provider("cloudflare").expect("cloudflare is in the registry");
        let openai = lookup_provider("openai").expect("openai is in the registry");
        // A user override always wins, for any provider.
        assert_eq!(
            discovery_base_url(cf, Some("https://gw.example/v1".to_string())),
            Some("https://gw.example/v1".to_string())
        );
        // Non-cloudflare: the static registry default.
        assert_eq!(
            discovery_base_url(openai, None),
            Some(openai.base_url.to_string())
        );
        // Cloudflare synthesizes the account-scoped URL from env...
        temp_env::with_var("CLOUDFLARE_ACCOUNT_ID", Some("acct123"), || {
            assert_eq!(
                discovery_base_url(cf, None),
                Some("https://api.cloudflare.com/client/v4/accounts/acct123/ai/v1".to_string())
            );
        });
        // ...and yields None when it's unset — nothing real to probe.
        temp_env::with_var("CLOUDFLARE_ACCOUNT_ID", None::<&str>, || {
            assert_eq!(discovery_base_url(cf, None), None);
        });
    }

    #[tokio::test]
    async fn cloudflare_missing_both_env_vars_is_one_combined_error() {
        temp_env::async_with_vars(
            [
                ("CLOUDFLARE_ACCOUNT_ID", None::<&str>),
                ("CLOUDFLARE_API_TOKEN", None),
            ],
            async {
                let f = ProviderFactory::new(Config::default());
                let err = match f.resolve("cloudflare/@cf/zai-org/glm-5.2").await {
                    Ok(_) => panic!("must fail with neither env var set"),
                    Err(e) => e,
                };
                let msg = format!("{err}");
                assert!(
                    msg.contains("CLOUDFLARE_API_TOKEN") && msg.contains("CLOUDFLARE_ACCOUNT_ID"),
                    "one error must name both missing vars, got: {msg}"
                );
            },
        )
        .await;
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    fn unique_env(prefix: &str) -> String {
        static N: AtomicUsize = AtomicUsize::new(0);
        format!(
            "{}_{}_{}",
            prefix,
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        )
    }

    #[test]
    fn parse_bare_name_defaults_to_ollama() {
        let (p, m) = parse_model_id("qwen3-coder:30b");
        assert_eq!(p, "ollama");
        assert_eq!(m, "qwen3-coder:30b");
    }

    #[test]
    fn parse_prefixed() {
        let (p, m) = parse_model_id("anthropic/claude-opus-4-7");
        assert_eq!(p, "anthropic");
        assert_eq!(m, "claude-opus-4-7");
    }

    #[tokio::test]
    async fn meta_requires_its_documented_api_key_env() {
        temp_env::async_with_vars(
            [(
                crate::providers::model::meta::DEFAULT_API_KEY_ENV,
                None::<&str>,
            )],
            async {
                let factory = ProviderFactory::new(Config::default());
                let error = match factory.resolve("meta/muse-spark-1.1").await {
                    Ok(_) => panic!("Meta must require an API key"),
                    Err(error) => error,
                };
                assert!(
                    error
                        .to_string()
                        .contains(crate::providers::model::meta::DEFAULT_API_KEY_ENV)
                );
            },
        )
        .await;
    }

    #[tokio::test]
    async fn meta_routes_to_responses_provider_with_muse_capabilities() {
        temp_env::async_with_vars(
            [(
                crate::providers::model::meta::DEFAULT_API_KEY_ENV,
                Some("test-key"),
            )],
            async {
                let factory = ProviderFactory::new(Config::default());
                let provider = factory.resolve("meta/muse-spark-1.1").await.unwrap();
                let capabilities = provider.capabilities();
                assert!(capabilities.supports_tools);
                assert!(capabilities.supports_vision);
                assert!(capabilities.emits_provider_continuation);
                assert_eq!(
                    capabilities.max_context_tokens,
                    Some(mermaid_model::constants::META_MUSE_SPARK_CONTEXT_WINDOW)
                );
                assert_eq!(
                    capabilities.max_output_tokens,
                    Some(mermaid_model::constants::META_MUSE_SPARK_MAX_OUTPUT_TOKENS)
                );
            },
        )
        .await;
    }

    #[test]
    fn gemini_key_resolution_accepts_legacy_fallback() {
        let primary = unique_env("MERMAID_FACTORY_GEMINI_PRIMARY");
        let legacy = unique_env("MERMAID_FACTORY_GEMINI_LEGACY");
        temp_env::with_vars(
            [(primary.as_str(), None), (legacy.as_str(), Some("legacy"))],
            || {
                let resolved = require_key_with_fallback("gemini", &primary, &legacy)
                    .expect("legacy fallback should resolve");
                assert_eq!(resolved, "legacy");
            },
        );
    }

    #[test]
    fn gemini_key_resolution_prefers_google_primary() {
        let primary = unique_env("MERMAID_FACTORY_GEMINI_PRIMARY2");
        let legacy = unique_env("MERMAID_FACTORY_GEMINI_LEGACY2");
        temp_env::with_vars(
            [
                (primary.as_str(), Some("google")),
                (legacy.as_str(), Some("legacy")),
            ],
            || {
                let resolved = require_key_with_fallback("gemini", &primary, &legacy)
                    .expect("primary should resolve");
                assert_eq!(resolved, "google");
            },
        );
    }

    #[tokio::test]
    async fn factory_reports_unknown_provider_clearly() {
        let cfg = Config::default();
        let f = ProviderFactory::new(cfg);
        match f.resolve("totally-made-up/model").await {
            Ok(_) => panic!("expected error"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("totally-made-up") || msg.contains("Unknown provider"),
                    "error message: {msg}"
                );
            },
        }
    }

    #[test]
    fn normalize_cache_key_lowercases_provider_only() {
        // #83: provider segment is lowercased; the model segment is preserved.
        assert_eq!(
            normalize_cache_key("Anthropic/Claude-X"),
            "anthropic/Claude-X"
        );
        assert_eq!(
            normalize_cache_key("anthropic/Claude-X"),
            "anthropic/Claude-X"
        );
        // Bare names default to the ollama provider.
        assert_eq!(normalize_cache_key("qwen3:30b"), "ollama/qwen3:30b");
    }

    #[tokio::test]
    async fn resolve_is_single_flight_and_cached() {
        // Ollama is keyless and builds no network connection, so this resolves
        // offline. Two concurrent resolves with different provider casing must
        // return the same cached instance (#83 normalization + #84 single-flight).
        let f = ProviderFactory::new(Config::default());
        let (a, b) = tokio::join!(
            f.resolve("ollama/test-model"),
            f.resolve("Ollama/test-model"),
        );
        let a = a.expect("resolve a");
        let b = b.expect("resolve b");
        assert!(
            Arc::ptr_eq(&a, &b),
            "expected one cached provider for casing variants + concurrent resolve"
        );
    }

    // F66: a built-in provider's base_url override is hardened — validated for
    // scheme and otherwise honored (the warning is a side effect we don't assert
    // here, but the dedup helper is tested separately below).
    #[test]
    fn builtin_base_url_override_validated_and_resolved() {
        // No override → trusted default, unchanged, no validation needed.
        assert_eq!(
            resolve_overridable_base_url("anthropic", None, "https://api.anthropic.com/v1")
                .unwrap(),
            "https://api.anthropic.com/v1"
        );
        // https override is honored (the key would go there — warned, not blocked).
        assert_eq!(
            resolve_overridable_base_url(
                "anthropic",
                Some("https://proxy.internal/v1".to_string()),
                "https://api.anthropic.com/v1",
            )
            .unwrap(),
            "https://proxy.internal/v1"
        );
        // http override to a NON-loopback host is refused (would leak the key in
        // cleartext) — the F66 https requirement.
        assert!(
            resolve_overridable_base_url(
                "anthropic",
                Some("http://attacker.example/v1".to_string()),
                "https://api.anthropic.com/v1",
            )
            .is_err()
        );
        // http override to loopback stays allowed for local dev (don't break it).
        assert!(
            resolve_overridable_base_url(
                "openai",
                Some("http://localhost:8080/v1".to_string()),
                "https://api.openai.com/v1",
            )
            .is_ok()
        );
    }

    #[test]
    fn provider_host_extracts_host_or_unknown() {
        assert_eq!(
            provider_host("https://attacker.example/v1"),
            "attacker.example"
        );
        assert_eq!(provider_host("http://127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(provider_host("not a url"), "<unknown>");
    }

    #[test]
    fn override_host_warning_is_deduped() {
        // The F66 warning must be one-time per key: first call fires, the rest
        // are suppressed. Use a process-unique key so this test doesn't race the
        // shared warned-set with any other test.
        let key = unique_env("MERMAID_FACTORY_WARN_KEY");
        assert!(should_warn_once(&key), "first warn for a key must fire");
        assert!(
            !should_warn_once(&key),
            "subsequent warns for the same key must be suppressed"
        );
    }

    // F67: identical custom-provider inputs must reuse one leaked &'static
    // profile, and distinct inputs must each leak exactly one — no per-model_id
    // growth.
    #[test]
    fn custom_profile_is_memoized_per_key() {
        use mermaid_domain::UserProviderConfig;
        let cfg = UserProviderConfig {
            base_url: Some("https://api.custom.test/v1".to_string()),
            api_key_env: Some("CUSTOM_KEY".to_string()),
            compat: Some("openai".to_string()),
            ..Default::default()
        };
        // Two resolves with identical inputs (as happens for two different models
        // of the same custom provider) return the SAME leaked pointer.
        let a = user_profile_to_static("mermaid_test_customx", &cfg).unwrap();
        let b = user_profile_to_static("mermaid_test_customx", &cfg).unwrap();
        assert!(
            std::ptr::eq(a, b),
            "identical custom-provider inputs must reuse one leaked &'static profile"
        );
        assert_eq!(a.base_url, "https://api.custom.test/v1");
        assert_eq!(a.api_key_env, "CUSTOM_KEY");

        // A different base_url is a different key → a distinct leaked profile.
        let cfg2 = UserProviderConfig {
            base_url: Some("https://api.custom.test/v2".to_string()),
            ..cfg.clone()
        };
        let c = user_profile_to_static("mermaid_test_customx", &cfg2).unwrap();
        assert!(
            !std::ptr::eq(a, c),
            "a different base_url must leak a distinct profile"
        );
    }
}

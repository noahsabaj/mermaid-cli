//! OAuth sign-in for remote (Streamable HTTP) MCP servers, following the MCP
//! authorization spec (2026-07-28).
//!
//! `mermaid mcp login <server>` runs the browser flow once: discovery
//! (RFC 9728 resource metadata, then RFC 8414 / OpenID Connect metadata), a
//! client (pre-registered, Mermaid's Client ID Metadata Document, or Dynamic
//! Client Registration as the fallback), PKCE S256, the `resource` indicator
//! (RFC 8707), and the `iss` check (RFC 9207). The tokens go to the OS
//! keyring. After that, [`OAuthSession`] puts the access token on every
//! request of the server's transport and refreshes it when it expires or the
//! server answers 401.

mod discovery;
mod flow;
mod store;

use anyhow::{Result, anyhow};
use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderValue};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

use mermaid_domain::McpServerConfig;
use mermaid_model::utils::CredentialStore;

use self::discovery::Challenge;
use self::flow::{Client, Registration};
pub(crate) use self::store::StoredTokens;
use self::store::{ClientAuth, load, save};

/// How long the browser step may take.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
/// Refresh this long before the access token expires.
const EXPIRY_MARGIN_SECS: u64 = 60;
/// Budget for the unauthenticated probe that fetches the challenge.
const PROBE_TIMEOUT_SECS: u64 = 30;

/// The server wants a (new) sign-in. Shown as the reason a server did not
/// start, and recognised by `mermaid add --url` to run the sign-in there.
#[derive(Debug)]
pub struct AuthRequired {
    server: String,
    /// Scopes an `insufficient_scope` challenge asked for.
    more_scope: Option<String>,
}

impl std::fmt::Display for AuthRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.more_scope {
            Some(scope) => write!(
                f,
                "the server needs more access ({scope}); run `mermaid mcp login {}`",
                self.server
            ),
            None => write!(
                f,
                "sign-in required; run `mermaid mcp login {}`",
                self.server
            ),
        }
    }
}

impl std::error::Error for AuthRequired {}

/// True when `err` (anywhere in its chain) is an [`AuthRequired`].
#[must_use]
pub fn is_auth_required(err: &anyhow::Error) -> bool {
    err.downcast_ref::<AuthRequired>().is_some()
}

/// The process-wide keyring as a shareable store.
fn default_store() -> Arc<dyn CredentialStore> {
    struct Static(&'static dyn CredentialStore);
    impl CredentialStore for Static {
        fn get(&self, account: &str) -> Option<String> {
            self.0.get(account)
        }
        fn set(&self, account: &str, value: &str) -> Result<()> {
            self.0.set(account, value)
        }
        fn delete(&self, account: &str) -> Result<bool> {
            self.0.delete(account)
        }
        fn label(&self) -> &'static str {
            self.0.label()
        }
    }
    Arc::new(Static(mermaid_model::utils::default_store()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Whether the config already sends its own `Authorization` header, in which
/// case Mermaid's OAuth stays out of the way.
fn has_own_authorization(config: &McpServerConfig) -> bool {
    config
        .headers
        .keys()
        .chain(config.env_headers.keys())
        .any(|k| k.eq_ignore_ascii_case("authorization"))
}

fn server_url(config: &McpServerConfig) -> Result<Url> {
    let url = config
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("sign-in applies only to remote servers (with a `url`)"))?;
    Url::parse(url).map_err(|e| anyhow!("invalid MCP server url '{url}': {e}"))
}

/// Stored tokens for `server` that were issued for its current `url`.
fn load_for(
    store: &dyn CredentialStore,
    server: &str,
    config: &McpServerConfig,
) -> Option<StoredTokens> {
    load(store, server).filter(|t| config.url.as_deref() == Some(t.server_url.as_str()))
}

/// True when the keyring holds tokens for `server`'s current `url`.
#[must_use]
pub fn is_signed_in(server: &str, config: &McpServerConfig) -> bool {
    load_for(default_store().as_ref(), server, config).is_some()
}

/// Delete `server`'s stored tokens; `Ok(false)` when there were none.
///
/// # Errors
///
/// The keyring refusing the delete.
pub fn logout(server: &str) -> Result<bool> {
    store::delete(default_store().as_ref(), server)
}

/// Opens the authorization URL for the user. A seam, so tests can play the
/// browser.
pub(crate) type Browser = Box<dyn Fn(&Url) + Send + Sync>;

/// The real browser: print the URL (a remote shell cannot open one), then
/// ask the OS to open it.
fn system_browser() -> Browser {
    Box::new(|url: &Url| {
        println!("\nOpen this URL to sign in (Mermaid is trying to open it for you):\n\n  {url}\n");
        let url = url.to_string();
        tokio::spawn(async move {
            if let Err(e) = crate::providers::tool::exec::open_browser_url(&url).await {
                tracing::debug!("MCP OAuth: could not open a browser: {e}");
            }
        });
    })
}

/// Lines the user pastes, for a browser on another machine whose redirect
/// cannot reach this one. Only on a terminal. A plain thread, not
/// `spawn_blocking`: a read still blocked at exit must not hold the runtime.
fn pasted_lines() -> Option<mpsc::UnboundedReceiver<String>> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return None;
    }
    println!(
        "If the browser runs on another machine, sign in there and paste the address it \
         ends on (http://127.0.0.1:...) here."
    );
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
            if tx.send(std::mem::take(&mut line)).is_err() {
                break;
            }
        }
    });
    Some(rx)
}

/// Sign in to remote MCP server `server` in the browser and store the tokens
/// in the OS keyring.
///
/// # Errors
///
/// A config without a `url`, discovery or registration failing, the user
/// refusing or not finishing in time, the token request failing, and the
/// keyring refusing the write.
pub async fn login(server: &str, config: &McpServerConfig) -> Result<()> {
    let store = default_store();
    let outcome = login_with(
        server,
        config,
        store.as_ref(),
        system_browser(),
        pasted_lines(),
    )
    .await?;
    println!(
        "Signed in to '{server}' ({}). The tokens are in {}.",
        outcome,
        store.label()
    );
    Ok(())
}

/// [`login`] with the store, browser and paste channel injected.
pub(crate) async fn login_with(
    server: &str,
    config: &McpServerConfig,
    store: &dyn CredentialStore,
    browser: Browser,
    pasted: Option<mpsc::UnboundedReceiver<String>>,
) -> Result<String> {
    let url = server_url(config)?;
    let allow_private = config.allow_private_network;
    let http = super::transport_http::vetted_client(allow_private)?;
    let oauth = config.oauth.as_ref();

    let challenge = probe(&http, &url, allow_private).await?;
    let resource_meta =
        discovery::fetch_resource_metadata(&http, &url, &challenge, allow_private).await?;
    let resource = discovery::resource_indicator(&url, &resource_meta)?;
    // RFC 9728 section 7.6 leaves the choice to the client: the first.
    let issuer = resource_meta.authorization_servers[0].clone();
    let meta = discovery::fetch_auth_server_metadata(&http, &issuer, allow_private).await?;

    let previous = load(store, server);
    if let (Some(id), Some(prev)) = (oauth.and_then(|o| o.client_id.as_deref()), &previous)
        && prev.client_id == id
        && prev.issuer != meta.issuer
    {
        // Client ids belong to the server that issued them (spec: client
        // credentials are bound to their issuer).
        return Err(anyhow!(
            "'{server}' now signs in at {}, but client_id {id} was registered with {}. \
             Register a client with the new server and update [mcp_servers.{server}.oauth]",
            meta.issuer,
            prev.issuer
        ));
    }

    let callback = flow::bind_callback(oauth.and_then(|o| o.callback_port)).await?;
    let redirect_uri = callback.redirect_uri.clone();
    let (client, registration) =
        flow::choose_client(&http, &meta, oauth, &redirect_uri, server).await?;
    let scope = flow::select_scope(
        oauth,
        challenge.scope.as_deref(),
        resource_meta.scopes_supported.as_deref(),
        previous
            .as_ref()
            .filter(|p| p.issuer == meta.issuer)
            .and_then(|p| p.scope.as_deref()),
        meta.scopes_supported.as_deref(),
    );
    let pkce = flow::Pkce::new()?;
    let state = flow::new_state()?;
    let auth_url = flow::authorization_url(&flow::AuthorizationRequest {
        endpoint: &meta.authorization_endpoint,
        client_id: &client.id,
        redirect_uri: &redirect_uri,
        code_challenge: &pkce.challenge,
        state: &state,
        resource: &resource,
        scope: scope.as_deref(),
    })?;

    browser(&auth_url);
    let params = flow::wait_for_callback(callback, &state, pasted, SIGN_IN_TIMEOUT).await?;
    let code = flow::check_authorization_response(
        &params,
        &meta.issuer,
        meta.authorization_response_iss_parameter_supported,
    )?;
    let tokens = flow::exchange_code(
        &http,
        &meta.token_endpoint,
        &client,
        &code,
        &pkce.verifier,
        &redirect_uri,
        &resource,
    )
    .await?;

    let record = StoredTokens {
        // Bound to the url as configured, the string the transport compares.
        server_url: config.url.clone().unwrap_or_default(),
        resource,
        issuer: meta.issuer.clone(),
        token_endpoint: meta.token_endpoint.clone(),
        client_id: client.id.clone(),
        // A pre-registered client's secret stays in its env var.
        client_secret: match registration {
            Registration::Dynamic => client.secret.clone(),
            Registration::PreRegistered | Registration::MetadataDocument => None,
        },
        client_auth: client.auth,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at: tokens.expires_in.map(|s| now_secs() + s),
        scope: tokens.scope.or(scope),
    };
    save(store, server, &record)?;
    Ok(match registration {
        Registration::PreRegistered => "configured OAuth client",
        Registration::MetadataDocument => "Mermaid client metadata document",
        Registration::Dynamic => "dynamically registered client",
    }
    .to_string())
}

/// POST an unauthenticated `server/discover` (a 2026-07-28 request; a legacy
/// server rejects it the same way) to read the server's challenge. A server
/// that answers without 401/403 gives no challenge; discovery then falls back
/// to the well-known URLs.
async fn probe(http: &reqwest::Client, url: &Url, allow_private: bool) -> Result<Challenge> {
    use super::client::{MODERN_VERSION, with_meta};
    super::transport_http::check_ip_literal(url, allow_private)?;
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "server/discover",
        "params": with_meta(serde_json::json!({})),
    });
    let response = http
        .post(url.clone())
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .header("MCP-Protocol-Version", MODERN_VERSION)
        .header("Mcp-Method", "server/discover")
        .json(&body)
        .timeout(Duration::from_secs(PROBE_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| anyhow!("cannot reach {url}: {e}"))?;
    Ok(challenge_of(response.headers()))
}

fn challenge_of(headers: &HeaderMap) -> Challenge {
    discovery::parse_challenge(
        headers
            .get_all(reqwest::header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|v| v.to_str().ok()),
    )
}

/// Token state as the transport sees it.
#[derive(Default)]
struct Slot {
    /// The keyring has been read (lazily, on the first request).
    read: bool,
    tokens: Option<StoredTokens>,
}

impl Slot {
    fn read(tokens: Option<StoredTokens>) -> Self {
        Self { read: true, tokens }
    }
}

/// OAuth for one HTTP transport: puts `Authorization: Bearer` on requests
/// and keeps the token fresh. Absent when the config sends its own
/// `Authorization` header.
pub(crate) struct OAuthSession {
    server: String,
    config_url: String,
    client_secret_env: Option<String>,
    store: Arc<dyn CredentialStore>,
    http: reqwest::Client,
    tokens: tokio::sync::Mutex<Slot>,
}

impl OAuthSession {
    /// The session for `server`, or `None` when OAuth does not apply.
    pub(crate) fn for_server(
        server: &str,
        config: &McpServerConfig,
        store: Option<Arc<dyn CredentialStore>>,
        http: reqwest::Client,
    ) -> Option<Self> {
        if has_own_authorization(config) {
            return None;
        }
        Some(Self {
            server: server.to_string(),
            config_url: config.url.clone()?,
            client_secret_env: config
                .oauth
                .as_ref()
                .and_then(|o| o.client_secret_env.clone()),
            store: store.unwrap_or_else(default_store),
            http,
            tokens: tokio::sync::Mutex::new(Slot::default()),
        })
    }

    fn read_store(&self) -> Option<StoredTokens> {
        load(self.store.as_ref(), &self.server).filter(|t| t.server_url == self.config_url)
    }

    /// The `Authorization` value for the next request, refreshing first when
    /// the token is about to expire. `None` = not signed in.
    pub(crate) async fn bearer(&self) -> Option<HeaderValue> {
        let mut slot = self.tokens.lock().await;
        if !slot.read {
            *slot = Slot::read(self.read_store());
        }
        let tokens = &mut slot.tokens;
        let expiring = tokens.as_ref().is_some_and(|t| {
            t.refresh_token.is_some()
                && t.expires_at
                    .is_some_and(|at| at <= now_secs() + EXPIRY_MARGIN_SECS)
        });
        if expiring && let Some(fresh) = self.refresh(tokens.as_ref()?).await {
            *tokens = Some(fresh);
        }
        tokens.as_ref().and_then(|t| header_for(&t.access_token))
    }

    /// The server answered 401 to a request that carried `sent`. Returns a
    /// token worth one retry: one another process already refreshed into the
    /// keyring, or a fresh refresh. `None` = the user must sign in again.
    pub(crate) async fn after_unauthorized(
        &self,
        sent: Option<&HeaderValue>,
    ) -> Option<HeaderValue> {
        let mut slot = self.tokens.lock().await;
        let stored = self.read_store();
        if let Some(t) = &stored
            && let Some(h) = header_for(&t.access_token)
            && Some(&h) != sent
        {
            *slot = Slot::read(stored);
            return Some(h);
        }
        let fresh = match &stored {
            Some(t) => self.refresh(t).await,
            None => None,
        };
        let header = fresh.as_ref().and_then(|t| header_for(&t.access_token));
        // A refused refresh leaves no usable token in this process.
        *slot = Slot::read(fresh);
        header
    }

    /// The server answered 403 `insufficient_scope`: remember the scopes so
    /// the next `mermaid mcp login` asks for them as well (step-up is a
    /// union with what was granted before).
    pub(crate) async fn note_insufficient_scope(&self, scope: &str) {
        let mut slot = self.tokens.lock().await;
        let Some(mut tokens) = self.read_store() else {
            return;
        };
        let mut scopes: Vec<String> = tokens
            .scope
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_string)
            .collect();
        for s in scope.split_whitespace() {
            if !scopes.iter().any(|x| x == s) {
                scopes.push(s.to_string());
            }
        }
        tokens.scope = Some(scopes.join(" "));
        if let Err(e) = save(self.store.as_ref(), &self.server, &tokens) {
            tracing::warn!(
                "MCP OAuth: could not store the scopes '{}' asked for: {e}",
                self.server
            );
        }
        *slot = Slot::read(Some(tokens));
    }

    /// The error a request returns when the server refuses our token.
    pub(crate) fn sign_in_required(&self, more_scope: Option<String>) -> anyhow::Error {
        AuthRequired {
            server: self.server.clone(),
            more_scope,
        }
        .into()
    }

    /// Refresh `tokens`; the new record is stored and returned. A failure is
    /// logged and is `None` (the request then reports sign-in required).
    async fn refresh(&self, tokens: &StoredTokens) -> Option<StoredTokens> {
        let refresh_token = tokens.refresh_token.as_deref()?;
        let secret = tokens.client_secret.clone().or_else(|| {
            let var = self.client_secret_env.as_deref()?;
            std::env::var(var).ok().filter(|v| !v.is_empty())
        });
        let client = Client {
            id: tokens.client_id.clone(),
            auth: if secret.is_some() {
                tokens.client_auth
            } else {
                ClientAuth::None
            },
            secret,
        };
        let response = match flow::refresh(
            &self.http,
            &tokens.token_endpoint,
            &client,
            refresh_token,
            &tokens.resource,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    "MCP OAuth: token refresh for '{}' failed: {e:#}",
                    self.server
                );
                return None;
            },
        };
        let fresh = StoredTokens {
            access_token: response.access_token,
            // Servers that rotate send a new one; others keep the old.
            refresh_token: response
                .refresh_token
                .or_else(|| tokens.refresh_token.clone()),
            expires_at: response.expires_in.map(|s| now_secs() + s),
            scope: response.scope.or_else(|| tokens.scope.clone()),
            ..tokens.clone()
        };
        if let Err(e) = save(self.store.as_ref(), &self.server, &fresh) {
            tracing::warn!(
                "MCP OAuth: could not store refreshed tokens for '{}': {e}",
                self.server
            );
        }
        Some(fresh)
    }
}

fn header_for(access_token: &str) -> Option<HeaderValue> {
    let mut v = HeaderValue::from_str(&format!("Bearer {access_token}")).ok()?;
    v.set_sensitive(true);
    Some(v)
}

/// The `insufficient_scope` scopes of a 403, if that is what it is.
pub(crate) fn insufficient_scope(headers: &HeaderMap) -> Option<String> {
    let c = challenge_of(headers);
    (c.error.as_deref() == Some("insufficient_scope")).then(|| c.scope.unwrap_or_default())
}

#[cfg(test)]
pub(crate) use self::store::test_support::MemStore;

#[cfg(test)]
mod tests;

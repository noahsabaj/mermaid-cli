//! The OAuth 2.1 authorization-code flow with PKCE for a remote MCP server:
//! client registration, the authorization URL, the loopback callback, and
//! the token endpoint (code exchange and refresh).

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use super::discovery::{AuthServerMetadata, require_secure};
use super::store::ClientAuth;
use mermaid_domain::McpOAuthConfig;

/// Mermaid's OAuth Client ID Metadata Document. The URL is the client id;
/// the document is `packaging/pages/oauth/client-metadata.json`, served by
/// the Pages workflow. Authorization servers that support CIMD fetch it, so
/// no registration step is needed.
pub(crate) const CLIENT_METADATA_URL: &str =
    "https://noahsabaj.github.io/mermaid-cli/oauth/client-metadata.json";
pub(crate) const CLIENT_NAME: &str = "Mermaid";
const CLIENT_URI: &str = "https://github.com/noahsabaj/mermaid-cli";

/// Path of the loopback redirect URI, `http://127.0.0.1:<port>/callback`.
const CALLBACK_PATH: &str = "/callback";
/// Budget for one token or registration request.
const TOKEN_TIMEOUT_SECS: u64 = 30;
/// Budget for reading one request on the callback listener.
const CALLBACK_READ_TIMEOUT_SECS: u64 = 10;
/// Cap on a callback request's head.
const CALLBACK_MAX_BYTES: usize = 16 * 1024;

fn random_b64(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf)
        .map_err(|e| anyhow!("operating-system randomness is unavailable: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

/// A PKCE verifier and its S256 challenge (RFC 7636).
pub(crate) struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Result<Self> {
        // 32 random bytes = 43 base64url characters, the RFC 7636 minimum.
        let verifier = random_b64(32)?;
        Ok(Self {
            challenge: s256(&verifier),
            verifier,
        })
    }
}

pub(crate) fn s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A fresh `state` value for CSRF protection.
pub(crate) fn new_state() -> Result<String> {
    random_b64(32)
}

/// The OAuth client Mermaid presents to one authorization server.
#[derive(Clone)]
pub(crate) struct Client {
    pub id: String,
    pub secret: Option<String>,
    pub auth: ClientAuth,
}

/// How the client was obtained, for the user-facing summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Registration {
    PreRegistered,
    MetadataDocument,
    Dynamic,
}

/// Pick the client in the spec's priority order: a pre-registered client
/// from config, then Mermaid's Client ID Metadata Document when the server
/// supports it, then Dynamic Client Registration (deprecated, kept for
/// servers without CIMD).
///
/// # Errors
///
/// An unset `client_secret_env` variable, a failed registration, and an
/// authorization server that offers no way to get a client.
pub(crate) async fn choose_client(
    http: &reqwest::Client,
    metadata: &AuthServerMetadata,
    config: Option<&McpOAuthConfig>,
    redirect_uri: &str,
    server: &str,
) -> Result<(Client, Registration)> {
    if let Some(id) = config.and_then(|c| c.client_id.clone()) {
        let secret = pre_registered_secret(config)?;
        let auth = if secret.is_some() {
            secret_auth_method(metadata)
        } else {
            ClientAuth::None
        };
        return Ok((Client { id, secret, auth }, Registration::PreRegistered));
    }
    if metadata.client_id_metadata_document_supported {
        let client = Client {
            id: CLIENT_METADATA_URL.to_string(),
            secret: None,
            auth: ClientAuth::None,
        };
        return Ok((client, Registration::MetadataDocument));
    }
    if let Some(endpoint) = &metadata.registration_endpoint {
        let client = register(http, endpoint, redirect_uri).await?;
        return Ok((client, Registration::Dynamic));
    }
    bail!(
        "the authorization server {} does not let Mermaid register itself. Register an \
         OAuth app there with redirect URI http://127.0.0.1:<port>{CALLBACK_PATH}, then set \
         client_id (and callback_port, client_secret_env if needed) under \
         [mcp_servers.{server}.oauth] in config.toml",
        metadata.issuer
    )
}

/// The secret of a pre-registered client, read from its env var now.
pub(crate) fn pre_registered_secret(config: Option<&McpOAuthConfig>) -> Result<Option<String>> {
    let Some(var) = config.and_then(|c| c.client_secret_env.as_deref()) else {
        return Ok(None);
    };
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Ok(Some(v)),
        _ => bail!("client_secret_env names ${var}, which is not set"),
    }
}

/// `client_secret_basic` unless the server lists only `client_secret_post`
/// (RFC 8414: an absent list means `client_secret_basic`).
fn secret_auth_method(metadata: &AuthServerMetadata) -> ClientAuth {
    match &metadata.token_endpoint_auth_methods_supported {
        Some(methods)
            if methods.iter().any(|m| m == "client_secret_post")
                && !methods.iter().any(|m| m == "client_secret_basic") =>
        {
            ClientAuth::ClientSecretPost
        },
        _ => ClientAuth::ClientSecretBasic,
    }
}

#[derive(Deserialize)]
struct RegistrationResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
}

/// RFC 7591 Dynamic Client Registration of a native public client.
async fn register(http: &reqwest::Client, endpoint: &str, redirect_uri: &str) -> Result<Client> {
    let url = require_secure(endpoint, "registration endpoint")?;
    let body = serde_json::json!({
        "client_name": CLIENT_NAME,
        "client_uri": CLIENT_URI,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        // A CLI with a loopback redirect is a native app; OIDC servers
        // default to "web" and then refuse the loopback redirect URI.
        "application_type": "native",
    });
    let response = http
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .timeout(Duration::from_secs(TOKEN_TIMEOUT_SECS))
        .send()
        .await
        .context("client registration request failed")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "the authorization server refused to register Mermaid (HTTP {status}): {}",
            oauth_error_text(&text)
        );
    }
    let reg: RegistrationResponse =
        serde_json::from_str(&text).context("client registration response is not valid")?;
    let auth = match reg.token_endpoint_auth_method.as_deref() {
        Some("client_secret_post") => ClientAuth::ClientSecretPost,
        Some("client_secret_basic") => ClientAuth::ClientSecretBasic,
        Some(_) | None if reg.client_secret.is_some() => ClientAuth::ClientSecretBasic,
        _ => ClientAuth::None,
    };
    Ok(Client {
        id: reg.client_id,
        secret: reg.client_secret,
        auth,
    })
}

/// The scope to request: config `scopes` replace everything the server
/// suggests; otherwise the challenge's scope, else the resource metadata's
/// `scopes_supported`. Either way, scopes requested at an earlier sign-in or
/// named by a later `insufficient_scope` challenge are kept (step-up is a
/// union), and `offline_access` is added when the authorization server
/// lists it, so OpenID Connect servers issue a refresh token.
pub(crate) fn select_scope(
    config: Option<&McpOAuthConfig>,
    challenge_scope: Option<&str>,
    resource_scopes: Option<&[String]>,
    previous: Option<&str>,
    auth_server_scopes: Option<&[String]>,
) -> Option<String> {
    let mut scopes: Vec<String> = Vec::new();
    let mut add = |s: &str| {
        for part in s.split_whitespace() {
            if !scopes.iter().any(|x| x == part) {
                scopes.push(part.to_string());
            }
        }
    };
    match config
        .map(|c| c.scopes.as_slice())
        .filter(|s| !s.is_empty())
    {
        Some(configured) => configured.iter().for_each(|s| add(s)),
        None => {
            if let Some(s) = challenge_scope {
                add(s);
            } else if let Some(list) = resource_scopes {
                list.iter().for_each(|s| add(s));
            }
        },
    }
    if let Some(prev) = previous {
        add(prev);
    }
    if auth_server_scopes.is_some_and(|l| l.iter().any(|s| s == "offline_access")) {
        add("offline_access");
    }
    (!scopes.is_empty()).then(|| scopes.join(" "))
}

/// Everything that goes into the authorization request.
pub(crate) struct AuthorizationRequest<'a> {
    pub endpoint: &'a str,
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub code_challenge: &'a str,
    pub state: &'a str,
    pub resource: &'a str,
    pub scope: Option<&'a str>,
}

/// The URL the user's browser opens.
///
/// # Errors
///
/// An authorization endpoint that does not parse or is not https.
pub(crate) fn authorization_url(req: &AuthorizationRequest<'_>) -> Result<Url> {
    let mut url = require_secure(req.endpoint, "authorization endpoint")?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", req.client_id)
            .append_pair("redirect_uri", req.redirect_uri)
            .append_pair("code_challenge", req.code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", req.state)
            .append_pair("resource", req.resource);
        if let Some(scope) = req.scope {
            q.append_pair("scope", scope);
        }
    }
    Ok(url)
}

/// The loopback listener the authorization server redirects to.
pub(crate) struct CallbackServer {
    listener: TcpListener,
    pub redirect_uri: String,
}

/// Bind `127.0.0.1:<port>` (any free port when `port` is `None`).
///
/// # Errors
///
/// The port is in use or cannot be bound.
pub(crate) async fn bind_callback(port: Option<u16>) -> Result<CallbackServer> {
    let listener = TcpListener::bind(("127.0.0.1", port.unwrap_or(0)))
        .await
        .with_context(|| match port {
            Some(p) => format!("cannot listen on 127.0.0.1:{p} for the sign-in callback"),
            None => "cannot listen on 127.0.0.1 for the sign-in callback".to_string(),
        })?;
    let port = listener.local_addr()?.port();
    Ok(CallbackServer {
        listener,
        redirect_uri: format!("http://127.0.0.1:{port}{CALLBACK_PATH}"),
    })
}

/// Query parameters of a redirect target (`/callback?code=...`) or a full
/// pasted redirect URL.
fn callback_params(target: &str) -> Option<HashMap<String, String>> {
    let url = if target.starts_with('/') {
        Url::parse(&format!("http://127.0.0.1{target}")).ok()?
    } else {
        Url::parse(target.trim()).ok()?
    };
    if url.path() != CALLBACK_PATH {
        return None;
    }
    Some(url.query_pairs().into_owned().collect())
}

const PAGE_DONE: &str = "<!doctype html><title>Mermaid</title><p>Sign-in is complete. \
                         You can close this tab and go back to the terminal.</p>";
const PAGE_FAILED: &str = "<!doctype html><title>Mermaid</title><p>Sign-in did not \
                           complete. Look at the terminal for the reason.</p>";

async fn respond(sock: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = sock.write_all(reply.as_bytes()).await;
    let _ = sock.shutdown().await;
}

/// Read the request line of one callback connection: the target of a
/// `GET`, or `None` for anything else.
async fn read_target(sock: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let read = async {
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < CALLBACK_MAX_BYTES {
            match sock.read(&mut tmp).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(CALLBACK_READ_TIMEOUT_SECS), read)
        .await
        .ok()?;
    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.lines().next()?.split_whitespace();
    (parts.next()? == "GET").then(|| parts.next().map(str::to_string))?
}

/// Wait for the authorization response: a browser request to the callback,
/// or a redirect URL the user pasted (for a browser on another machine).
/// A response whose `state` does not match is discarded, and waiting goes
/// on — a stray or forged request must not end a real sign-in.
///
/// # Errors
///
/// Nothing arrived within `timeout`.
pub(crate) async fn wait_for_callback(
    server: CallbackServer,
    expected_state: &str,
    mut pasted: Option<mpsc::UnboundedReceiver<String>>,
    timeout: Duration,
) -> Result<HashMap<String, String>> {
    let wait = async {
        loop {
            let pasted_line = async {
                match pasted.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                accepted = server.listener.accept() => {
                    let Ok((mut sock, _)) = accepted else { continue };
                    let Some(target) = read_target(&mut sock).await else {
                        respond(&mut sock, "400 Bad Request", PAGE_FAILED).await;
                        continue;
                    };
                    let Some(params) = callback_params(&target) else {
                        respond(&mut sock, "404 Not Found", "").await;
                        continue;
                    };
                    if params.get("state").map(String::as_str) != Some(expected_state) {
                        respond(&mut sock, "400 Bad Request", PAGE_FAILED).await;
                        continue;
                    }
                    let page = if params.contains_key("code") { PAGE_DONE } else { PAGE_FAILED };
                    respond(&mut sock, "200 OK", page).await;
                    return params;
                },
                line = pasted_line => {
                    let Some(line) = line else {
                        pasted = None;
                        continue;
                    };
                    match callback_params(&line) {
                        Some(params)
                            if params.get("state").map(String::as_str) == Some(expected_state) =>
                        {
                            return params;
                        },
                        _ if line.trim().is_empty() => {},
                        _ => eprintln!(
                            "That is not the redirect URL of this sign-in. Paste the whole \
                             address from the browser (it starts with http://127.0.0.1)."
                        ),
                    }
                },
            }
        }
    };
    tokio::time::timeout(timeout, wait)
        .await
        .map_err(|_| anyhow!("sign-in timed out after {}s", timeout.as_secs()))
}

/// Check an authorization response and return its code. The `iss` check
/// (RFC 9207 section 2.4, required by the MCP spec) comes before anything
/// else in the response is used — on a mismatch even its `error` text is
/// not shown, since it may come from another server.
///
/// # Errors
///
/// A wrong or (when the server promises one) missing `iss`, an `error`
/// response, and a response without a code.
pub(crate) fn check_authorization_response(
    params: &HashMap<String, String>,
    issuer: &str,
    iss_supported: bool,
) -> Result<String> {
    match params.get("iss") {
        Some(iss) if iss != issuer => {
            bail!("the sign-in response came from issuer '{iss}', not '{issuer}'; it was discarded")
        },
        None if iss_supported => bail!(
            "the sign-in response has no issuer, but {issuer} says it always sends one; \
             it was discarded"
        ),
        _ => {},
    }
    if let Some(error) = params.get("error") {
        let detail = params
            .get("error_description")
            .map(|d| format!(": {d}"))
            .unwrap_or_default();
        bail!("the authorization server refused the sign-in ({error}{detail})");
    }
    params
        .get("code")
        .filter(|c| !c.is_empty())
        .cloned()
        .ok_or_else(|| anyhow!("the sign-in response has no authorization code"))
}

/// A token endpoint response.
pub(crate) struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
    pub scope: Option<String>,
}

/// A token endpoint `error` response (RFC 6749 section 5.2).
#[derive(Debug)]
pub(crate) struct TokenError {
    pub error: String,
    pub description: Option<String>,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the token request was refused ({}", self.error)?;
        if let Some(d) = &self.description {
            write!(f, ": {d}")?;
        }
        write!(f, ")")
    }
}

impl std::error::Error for TokenError {}

/// Exchange an authorization code for tokens.
///
/// # Errors
///
/// The request failing, an `error` response, or a malformed token response.
pub(crate) async fn exchange_code(
    http: &reqwest::Client,
    token_endpoint: &str,
    client: &Client,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    resource: &str,
) -> Result<TokenResponse> {
    token_request(
        http,
        token_endpoint,
        client,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
            ("resource", resource),
        ],
    )
    .await
}

/// Use a refresh token for a new access token.
///
/// # Errors
///
/// As [`exchange_code`]; an `invalid_grant` is a [`TokenError`] in the
/// chain, meaning the user has to sign in again.
pub(crate) async fn refresh(
    http: &reqwest::Client,
    token_endpoint: &str,
    client: &Client,
    refresh_token: &str,
    resource: &str,
) -> Result<TokenResponse> {
    token_request(
        http,
        token_endpoint,
        client,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("resource", resource),
        ],
    )
    .await
}

#[derive(Deserialize)]
struct RawTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<serde_json::Value>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

fn form_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

async fn token_request(
    http: &reqwest::Client,
    token_endpoint: &str,
    client: &Client,
    fields: &[(&str, &str)],
) -> Result<TokenResponse> {
    let url = require_secure(token_endpoint, "token endpoint")?;
    let (basic, body_auth): (Option<String>, Vec<(&str, &str)>) =
        match (client.auth, client.secret.as_deref()) {
            // RFC 6749 section 2.3.1: each part form-encoded, then Basic.
            (ClientAuth::ClientSecretBasic, Some(secret)) => (
                Some(format!(
                    "Basic {}",
                    STANDARD.encode(format!(
                        "{}:{}",
                        form_encode(&client.id),
                        form_encode(secret)
                    ))
                )),
                Vec::new(),
            ),
            (ClientAuth::ClientSecretPost, Some(secret)) => (
                None,
                vec![("client_id", client.id.as_str()), ("client_secret", secret)],
            ),
            _ => (None, vec![("client_id", client.id.as_str())]),
        };
    // Built in a block: the serializer is not Send and must not live across
    // the await below.
    let body = {
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in fields.iter().chain(body_auth.iter()) {
            form.append_pair(k, v);
        }
        form.finish()
    };
    let mut request = http
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .timeout(Duration::from_secs(TOKEN_TIMEOUT_SECS));
    if let Some(basic) = basic {
        request = request.header(reqwest::header::AUTHORIZATION, basic);
    }
    let response = request
        .body(body)
        .send()
        .await
        .context("token request failed")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let raw: RawTokenResponse = match serde_json::from_str(&text) {
        Ok(raw) => raw,
        Err(_) if !status.is_success() => bail!(
            "the token request failed (HTTP {status}): {}",
            oauth_error_text(&text)
        ),
        Err(e) => bail!("the token response is not valid JSON: {e}"),
    };
    if let Some(error) = raw.error {
        return Err(TokenError {
            error,
            description: raw.error_description,
        }
        .into());
    }
    if !status.is_success() {
        bail!("the token request failed (HTTP {status})");
    }
    let access_token = raw
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("the token response has no access_token"))?;
    if let Some(kind) = &raw.token_type
        && !kind.eq_ignore_ascii_case("bearer")
    {
        bail!("the server issued a '{kind}' token; Mermaid supports only Bearer tokens");
    }
    // Some servers send expires_in as a string.
    let expires_in = match raw.expires_in {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    };
    Ok(TokenResponse {
        access_token,
        refresh_token: raw.refresh_token.filter(|t| !t.is_empty()),
        expires_in,
        scope: raw.scope,
    })
}

/// A short, redacted rendering of an OAuth error body for an error message.
fn oauth_error_text(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(e) = v.get("error").and_then(|e| e.as_str())
    {
        let d = v
            .get("error_description")
            .and_then(|d| d.as_str())
            .map(|d| format!(": {d}"))
            .unwrap_or_default();
        return format!("{e}{d}");
    }
    let redacted = mermaid_model::utils::redact_secrets(body);
    let end = redacted.floor_char_boundary(200.min(redacted.len()));
    redacted[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        // RFC 7636 appendix B.
        assert_eq!(
            s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let p = Pkce::new().unwrap();
        assert_eq!(p.verifier.len(), 43);
        assert_eq!(p.challenge, s256(&p.verifier));
    }

    #[test]
    fn authorization_url_carries_pkce_state_resource_and_scope() {
        let url = authorization_url(&AuthorizationRequest {
            endpoint: "https://auth.example.com/authorize?tenant=a",
            client_id: CLIENT_METADATA_URL,
            redirect_uri: "http://127.0.0.1:5555/callback",
            code_challenge: "ch",
            state: "st",
            resource: "https://mcp.example.com/mcp",
            scope: Some("files:read offline_access"),
        })
        .unwrap();
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(q["tenant"], "a", "existing query is kept");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], CLIENT_METADATA_URL);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], "ch");
        assert_eq!(q["state"], "st");
        assert_eq!(q["resource"], "https://mcp.example.com/mcp");
        assert_eq!(q["scope"], "files:read offline_access");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:5555/callback");
    }

    #[test]
    fn scope_selection_follows_spec_and_unions_step_up() {
        let prm = vec!["a".to_string(), "b".to_string()];
        let asm = vec!["offline_access".to_string()];
        // Challenge beats scopes_supported.
        assert_eq!(
            select_scope(None, Some("x"), Some(&prm), None, None).as_deref(),
            Some("x")
        );
        assert_eq!(
            select_scope(None, None, Some(&prm), None, None).as_deref(),
            Some("a b")
        );
        // Earlier scopes are kept; offline_access added when offered.
        assert_eq!(
            select_scope(None, Some("x"), None, Some("y x"), Some(&asm)).as_deref(),
            Some("x y offline_access")
        );
        assert_eq!(select_scope(None, None, None, None, None), None);
        let cfg = McpOAuthConfig {
            scopes: vec!["repo".to_string()],
            ..Default::default()
        };
        assert_eq!(
            select_scope(Some(&cfg), Some("x"), Some(&prm), None, None).as_deref(),
            Some("repo")
        );
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn iss_is_checked_before_code_or_error() {
        let issuer = "https://auth.example.com";
        let ok = params(&[("code", "c"), ("iss", issuer)]);
        assert_eq!(
            check_authorization_response(&ok, issuer, true).unwrap(),
            "c"
        );
        // Present iss is compared even when the server does not advertise it.
        let wrong = params(&[("code", "c"), ("iss", "https://evil.example")]);
        assert!(check_authorization_response(&wrong, issuer, false).is_err());
        // No normalization: a trailing slash is a different issuer.
        let slash = params(&[("code", "c"), ("iss", "https://auth.example.com/")]);
        assert!(check_authorization_response(&slash, issuer, false).is_err());
        // Advertised but absent: reject. Not advertised and absent: proceed.
        let bare = params(&[("code", "c")]);
        assert!(check_authorization_response(&bare, issuer, true).is_err());
        assert_eq!(
            check_authorization_response(&bare, issuer, false).unwrap(),
            "c"
        );
        // A mismatched error response is not shown.
        let forged = params(&[("error", "access_denied"), ("iss", "https://evil.example")]);
        let err = check_authorization_response(&forged, issuer, false).unwrap_err();
        assert!(!err.to_string().contains("access_denied"), "{err}");
        let denied = params(&[("error", "access_denied"), ("error_description", "no")]);
        let err = check_authorization_response(&denied, issuer, false).unwrap_err();
        assert!(err.to_string().contains("access_denied: no"), "{err}");
    }

    #[test]
    fn callback_params_accept_target_and_pasted_url() {
        let p = callback_params("/callback?code=a&state=s").unwrap();
        assert_eq!(p["code"], "a");
        let p = callback_params(" http://127.0.0.1:4000/callback?code=b&state=s\n").unwrap();
        assert_eq!(p["code"], "b");
        assert!(callback_params("/favicon.ico").is_none());
    }

    #[tokio::test]
    async fn callback_discards_wrong_state_then_takes_the_right_one() {
        let server = bind_callback(None).await.unwrap();
        let base = server.redirect_uri.clone();
        let http = reqwest::Client::new();
        let driver = tokio::spawn(async move {
            let r = http
                .get(format!("{base}?code=evil&state=bad"))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 400);
            let r = http
                .get(format!("{base}?code=good&state=s1"))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
        });
        let params = wait_for_callback(server, "s1", None, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(params["code"], "good");
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn pasted_redirect_url_completes_the_wait() {
        let server = bind_callback(None).await.unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send("not a url".to_string()).unwrap();
        tx.send(format!("{}?code=p&state=s2", server.redirect_uri))
            .unwrap();
        let params = wait_for_callback(server, "s2", Some(rx), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(params["code"], "p");
    }
}

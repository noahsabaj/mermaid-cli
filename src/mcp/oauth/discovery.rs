//! OAuth discovery for a remote MCP server (MCP authorization spec,
//! 2026-07-28): parse the `WWW-Authenticate` challenge, fetch the server's
//! Protected Resource Metadata (RFC 9728), then its authorization server's
//! metadata (RFC 8414, or OpenID Connect Discovery).

use anyhow::{Result, anyhow, bail};
use reqwest::Url;
use serde::Deserialize;

use super::super::transport_http::check_ip_literal;

/// Body cap for a metadata document; real ones are a few KB.
const MAX_METADATA_BYTES: usize = 256 * 1024;

/// The parameters of a `Bearer` challenge that discovery uses.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Challenge {
    pub resource_metadata: Option<String>,
    pub scope: Option<String>,
    pub error: Option<String>,
}

/// Parse the `Bearer` challenge out of one or more `WWW-Authenticate` header
/// values (RFC 9110 section 11.6.1). Parameters of other schemes are
/// ignored; the first value of a repeated parameter wins.
pub(crate) fn parse_challenge<'a>(values: impl IntoIterator<Item = &'a str>) -> Challenge {
    let mut out = Challenge::default();
    for value in values {
        let mut scheme_is_bearer = false;
        let mut rest = value.trim_start();
        while !rest.is_empty() {
            rest = rest.trim_start_matches(|c: char| c == ',' || c.is_whitespace());
            let end = rest
                .find(|c: char| c == '=' || c == ',' || c.is_whitespace())
                .unwrap_or(rest.len());
            let token = &rest[..end];
            rest = &rest[end..];
            if token.is_empty() {
                // A stray '=' (token68 padding): skip it.
                rest = rest.get(1..).unwrap_or("");
                continue;
            }
            let after_ws = rest.trim_start();
            if let Some(after_eq) = after_ws.strip_prefix('=')
                && !after_eq.starts_with('=')
            {
                let (param, remaining) = take_param_value(after_eq.trim_start());
                rest = remaining;
                if scheme_is_bearer {
                    let slot = match token.to_ascii_lowercase().as_str() {
                        "resource_metadata" => &mut out.resource_metadata,
                        "scope" => &mut out.scope,
                        "error" => &mut out.error,
                        _ => continue,
                    };
                    if slot.is_none() {
                        *slot = Some(param);
                    }
                }
            } else {
                // A bare token starts a new challenge (its scheme name).
                scheme_is_bearer = token.eq_ignore_ascii_case("bearer");
            }
        }
    }
    out
}

/// Read one auth-param value: a quoted-string (with `\` escapes) or a token.
fn take_param_value(s: &str) -> (String, &str) {
    if let Some(quoted) = s.strip_prefix('"') {
        let mut value = String::new();
        let mut chars = quoted.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    if let Some((_, escaped)) = chars.next() {
                        value.push(escaped);
                    }
                },
                '"' => return (value, &quoted[i + 1..]),
                _ => value.push(c),
            }
        }
        (value, "")
    } else {
        let end = s
            .find(|c: char| c == ',' || c.is_whitespace())
            .unwrap_or(s.len());
        (s[..end].to_string(), &s[end..])
    }
}

/// RFC 9728 Protected Resource Metadata, the fields Mermaid reads.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ResourceMetadata {
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub authorization_servers: Vec<String>,
    #[serde(default)]
    pub scopes_supported: Option<Vec<String>>,
}

/// RFC 8414 / OpenID Connect provider metadata, the fields Mermaid reads.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AuthServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub code_challenge_methods_supported: Option<Vec<String>>,
    #[serde(default)]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(default)]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(default)]
    pub client_id_metadata_document_supported: bool,
    #[serde(default)]
    pub authorization_response_iss_parameter_supported: bool,
}

/// An OAuth endpoint must be `https`, or `http` to a loopback host (local
/// test servers). Spec: all authorization server endpoints MUST be HTTPS.
pub(crate) fn require_secure(url: &str, what: &str) -> Result<Url> {
    let parsed = Url::parse(url).map_err(|e| anyhow!("invalid {what} URL '{url}': {e}"))?;
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http"
            if matches!(
                mermaid_model::utils::classify_host(parsed.host_str().unwrap_or("")),
                mermaid_model::utils::HostClass::Loopback
            ) =>
        {
            Ok(parsed)
        },
        _ => bail!("{what} URL '{url}' must use https"),
    }
}

/// The well-known Protected Resource Metadata URLs for `server`, in the
/// order the spec says to try them: path-specific first, then the root.
pub(crate) fn resource_metadata_candidates(server: &Url) -> Vec<Url> {
    let mut out = Vec::new();
    let path = server.path();
    let mut base = server.clone();
    base.set_query(None);
    base.set_fragment(None);
    if path != "/" && !path.is_empty() {
        let mut u = base.clone();
        u.set_path(&format!("/.well-known/oauth-protected-resource{path}"));
        out.push(u);
    }
    let mut root = base;
    root.set_path("/.well-known/oauth-protected-resource");
    out.push(root);
    out
}

/// The metadata URLs for issuer `issuer`, in the spec's priority order.
pub(crate) fn auth_server_metadata_candidates(issuer: &Url) -> Vec<Url> {
    let path = issuer.path().trim_end_matches('/');
    let at = |p: String| {
        let mut u = issuer.clone();
        u.set_query(None);
        u.set_fragment(None);
        u.set_path(&p);
        u
    };
    if path.is_empty() {
        vec![
            at("/.well-known/oauth-authorization-server".to_string()),
            at("/.well-known/openid-configuration".to_string()),
        ]
    } else {
        vec![
            at(format!("/.well-known/oauth-authorization-server{path}")),
            at(format!("/.well-known/openid-configuration{path}")),
            at(format!("{path}/.well-known/openid-configuration")),
        ]
    }
}

/// Lowercase scheme and host, default port dropped, no query, fragment or
/// trailing slash: the form two resource identifiers are compared in.
fn comparable(url: &Url) -> (String, String, Option<u16>, String) {
    (
        url.scheme().to_ascii_lowercase(),
        url.host_str().unwrap_or("").to_ascii_lowercase(),
        url.port_or_known_default(),
        url.path().trim_end_matches('/').to_string(),
    )
}

/// The RFC 8707 `resource` value to request tokens for: the metadata's
/// `resource` when it names this server (equal, or a parent path on the same
/// origin), else the server URL without a trailing slash. A `resource` that
/// names some other origin is refused (RFC 9728 section 3.3): it would let
/// one server collect tokens meant for another.
pub(crate) fn resource_indicator(server: &Url, metadata: &ResourceMetadata) -> Result<String> {
    let Some(declared) = &metadata.resource else {
        let mut canonical = server.clone();
        canonical.set_query(None);
        canonical.set_fragment(None);
        let text = canonical.to_string();
        return Ok(text
            .strip_suffix('/')
            .filter(|_| canonical.path() == "/")
            .map(str::to_string)
            .unwrap_or(text));
    };
    let parsed =
        Url::parse(declared).map_err(|e| anyhow!("server metadata has invalid resource: {e}"))?;
    let (ds, dh, dp, dpath) = comparable(&parsed);
    let (ss, sh, sp, spath) = comparable(server);
    let same_origin = ds == ss && dh == sh && dp == sp;
    let covers = dpath.is_empty() || spath == dpath || spath.starts_with(&format!("{dpath}/"));
    if same_origin && covers {
        Ok(declared.clone())
    } else {
        bail!(
            "the server's metadata names resource '{declared}', which is not {server}; \
             refusing to sign in"
        )
    }
}

/// GET a JSON metadata document. `Ok(None)` for a 404-class miss (try the
/// next candidate); other failures are errors.
async fn get_json<T: for<'de> Deserialize<'de>>(
    http: &reqwest::Client,
    url: &Url,
    allow_private: bool,
) -> Result<Option<T>> {
    check_ip_literal(url, allow_private)?;
    let response = http
        .get(url.clone())
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| anyhow!("fetch {url}: {e}"))?;
    let status = response.status();
    if status.is_client_error() || status.is_redirection() {
        return Ok(None);
    }
    if !status.is_success() {
        bail!("fetch {url}: HTTP {status}");
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| anyhow!("read {url}: {e}"))?;
    if bytes.len() > MAX_METADATA_BYTES {
        bail!("metadata at {url} is too large");
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| anyhow!("metadata at {url} is not valid: {e}"))
}

/// Fetch the server's Protected Resource Metadata: from the challenge's
/// `resource_metadata` URL when there is one, else from the well-known URLs.
///
/// # Errors
///
/// No document found, an invalid one, or one with no authorization server.
pub(crate) async fn fetch_resource_metadata(
    http: &reqwest::Client,
    server: &Url,
    challenge: &Challenge,
    allow_private: bool,
) -> Result<ResourceMetadata> {
    let candidates = match &challenge.resource_metadata {
        Some(url) => vec![require_secure(url, "resource metadata")?],
        None => resource_metadata_candidates(server),
    };
    for url in &candidates {
        if let Some(metadata) = get_json::<ResourceMetadata>(http, url, allow_private).await? {
            if metadata.authorization_servers.is_empty() {
                bail!("the server's metadata at {url} names no authorization server");
            }
            return Ok(metadata);
        }
    }
    bail!(
        "{server} does not publish OAuth metadata (tried {}); it may not support sign-in",
        candidates
            .iter()
            .map(Url::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Fetch and validate the metadata of authorization server `issuer`.
///
/// # Errors
///
/// No document found; an `issuer` in it that is not identical to `issuer`
/// (a document that claims another issuer MUST NOT be used); endpoints that
/// are not https; and no PKCE `S256` support, which the spec requires the
/// client to refuse.
pub(crate) async fn fetch_auth_server_metadata(
    http: &reqwest::Client,
    issuer: &str,
    allow_private: bool,
) -> Result<AuthServerMetadata> {
    let issuer_url = require_secure(issuer, "authorization server")?;
    for url in auth_server_metadata_candidates(&issuer_url) {
        let Some(metadata) = get_json::<AuthServerMetadata>(http, &url, allow_private).await?
        else {
            continue;
        };
        validate_auth_server_metadata(&metadata, issuer)?;
        return Ok(metadata);
    }
    bail!("authorization server {issuer} publishes no metadata")
}

/// The checks [`fetch_auth_server_metadata`] applies to a fetched document.
pub(crate) fn validate_auth_server_metadata(
    metadata: &AuthServerMetadata,
    issuer: &str,
) -> Result<()> {
    // Simple string comparison: no case folding or slash trimming.
    if metadata.issuer != issuer {
        bail!(
            "authorization server metadata names issuer '{}', not '{issuer}'; refusing it",
            metadata.issuer
        );
    }
    require_secure(&metadata.authorization_endpoint, "authorization endpoint")?;
    require_secure(&metadata.token_endpoint, "token endpoint")?;
    if let Some(registration) = &metadata.registration_endpoint {
        require_secure(registration, "registration endpoint")?;
    }
    let pkce = metadata
        .code_challenge_methods_supported
        .as_deref()
        .unwrap_or_default();
    if !pkce.iter().any(|m| m == "S256") {
        bail!(
            "authorization server {issuer} does not support PKCE (S256); \
             Mermaid will not sign in without it"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_parses_quoted_params_across_schemes() {
        let c = parse_challenge([
            r#"Basic realm="x", Bearer resource_metadata="https://m.example/.well-known/oauth-protected-resource", scope="files:read files:write""#,
        ]);
        assert_eq!(
            c.resource_metadata.as_deref(),
            Some("https://m.example/.well-known/oauth-protected-resource")
        );
        assert_eq!(c.scope.as_deref(), Some("files:read files:write"));
        assert_eq!(c.error, None);
    }

    #[test]
    fn challenge_ignores_other_schemes_and_reads_tokens() {
        let c = parse_challenge([
            r#"Basic scope="nope""#,
            r#"bearer error=insufficient_scope, scope="a \"b\"""#,
        ]);
        assert_eq!(c.error.as_deref(), Some("insufficient_scope"));
        assert_eq!(c.scope.as_deref(), Some(r#"a "b""#));
        assert_eq!(c.resource_metadata, None);
        // token68 after a scheme does not confuse the parser.
        let c = parse_challenge(["Negotiate abc==, Bearer scope=x"]);
        assert_eq!(c.scope.as_deref(), Some("x"));
    }

    #[test]
    fn well_known_resource_urls_try_path_then_root() {
        let server = Url::parse("https://example.com/public/mcp?x=1").unwrap();
        let urls: Vec<String> = resource_metadata_candidates(&server)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            urls,
            [
                "https://example.com/.well-known/oauth-protected-resource/public/mcp",
                "https://example.com/.well-known/oauth-protected-resource",
            ]
        );
        let root = Url::parse("https://example.com").unwrap();
        assert_eq!(resource_metadata_candidates(&root).len(), 1);
    }

    #[test]
    fn auth_server_urls_follow_spec_order() {
        let with_path = Url::parse("https://auth.example.com/tenant1").unwrap();
        let urls: Vec<String> = auth_server_metadata_candidates(&with_path)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            urls,
            [
                "https://auth.example.com/.well-known/oauth-authorization-server/tenant1",
                "https://auth.example.com/.well-known/openid-configuration/tenant1",
                "https://auth.example.com/tenant1/.well-known/openid-configuration",
            ]
        );
        let bare = Url::parse("https://auth.example.com").unwrap();
        let urls: Vec<String> = auth_server_metadata_candidates(&bare)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            urls,
            [
                "https://auth.example.com/.well-known/oauth-authorization-server",
                "https://auth.example.com/.well-known/openid-configuration",
            ]
        );
    }

    fn prm(resource: Option<&str>) -> ResourceMetadata {
        ResourceMetadata {
            resource: resource.map(str::to_string),
            authorization_servers: vec!["https://auth.example.com".to_string()],
            scopes_supported: None,
        }
    }

    #[test]
    fn resource_indicator_accepts_self_and_parent_refuses_other_origin() {
        let server = Url::parse("https://MCP.example.com/mcp/").unwrap();
        assert_eq!(
            resource_indicator(&server, &prm(Some("https://mcp.example.com/mcp"))).unwrap(),
            "https://mcp.example.com/mcp"
        );
        assert_eq!(
            resource_indicator(&server, &prm(Some("https://mcp.example.com"))).unwrap(),
            "https://mcp.example.com"
        );
        assert!(resource_indicator(&server, &prm(Some("https://evil.example/mcp"))).is_err());
        assert!(resource_indicator(&server, &prm(Some("https://mcp.example.com/other"))).is_err());
        // No declared resource: the server URL itself.
        assert_eq!(
            resource_indicator(&server, &prm(None)).unwrap(),
            "https://mcp.example.com/mcp/"
        );
        let root = Url::parse("https://mcp.example.com/").unwrap();
        assert_eq!(
            resource_indicator(&root, &prm(None)).unwrap(),
            "https://mcp.example.com"
        );
    }

    fn asm(issuer: &str, pkce: Option<&[&str]>) -> AuthServerMetadata {
        AuthServerMetadata {
            issuer: issuer.to_string(),
            authorization_endpoint: "https://auth.example.com/authorize".to_string(),
            token_endpoint: "https://auth.example.com/token".to_string(),
            registration_endpoint: None,
            code_challenge_methods_supported: pkce
                .map(|m| m.iter().map(|s| (*s).to_string()).collect()),
            token_endpoint_auth_methods_supported: None,
            scopes_supported: None,
            client_id_metadata_document_supported: false,
            authorization_response_iss_parameter_supported: false,
        }
    }

    #[test]
    fn metadata_needs_identical_issuer_and_s256() {
        let ok = asm("https://auth.example.com", Some(&["S256"]));
        assert!(validate_auth_server_metadata(&ok, "https://auth.example.com").is_ok());
        // A different issuer, even by a trailing slash, is refused.
        assert!(validate_auth_server_metadata(&ok, "https://auth.example.com/").is_err());
        let no_pkce = asm("https://auth.example.com", None);
        assert!(validate_auth_server_metadata(&no_pkce, "https://auth.example.com").is_err());
        let plain_only = asm("https://auth.example.com", Some(&["plain"]));
        assert!(validate_auth_server_metadata(&plain_only, "https://auth.example.com").is_err());
    }

    #[test]
    fn endpoints_must_be_https_or_loopback() {
        assert!(require_secure("https://a.example/x", "t").is_ok());
        assert!(require_secure("http://127.0.0.1:9/x", "t").is_ok());
        assert!(require_secure("http://a.example/x", "t").is_err());
        assert!(require_secure("ftp://a.example/x", "t").is_err());
    }
}

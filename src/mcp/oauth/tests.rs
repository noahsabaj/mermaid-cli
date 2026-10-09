//! End-to-end OAuth tests against a loopback fake: one HTTP server playing
//! the MCP server, its resource metadata, the authorization server, and its
//! registration and token endpoints. The "browser" follows the
//! authorization URL straight to Mermaid's callback.

use super::*;
use crate::mcp::client::McpClient;
use crate::mcp::transport_http::HttpTransport;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Debug)]
struct Req {
    method: String,
    target: String,
    /// Lowercased request head.
    head: String,
    body: String,
}

impl Req {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .into_owned()
            .collect()
    }
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

fn json(status: u16, body: serde_json::Value) -> Resp {
    Resp {
        status,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: body.to_string(),
    }
}

type Handler = Arc<dyn Fn(&Req) -> Resp + Send + Sync>;

struct Fake {
    base: String,
    requests: Arc<Mutex<Vec<Req>>>,
}

impl Fake {
    fn requests(&self) -> Vec<Req> {
        self.requests.lock().unwrap().clone()
    }

    fn to(&self, path: &str) -> Vec<Req> {
        self.requests()
            .into_iter()
            .filter(|r| r.path() == path)
            .collect()
    }

    fn config(&self) -> McpServerConfig {
        McpServerConfig {
            url: Some(format!("{}/mcp", self.base)),
            ..Default::default()
        }
    }
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<Req> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let pos = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
    let len = head
        .to_ascii_lowercase()
        .lines()
        .find_map(|l| {
            l.strip_prefix("content-length:")
                .map(|v| v.trim().to_string())
        })
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < pos + 4 + len {
        match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    let mut first = head.lines().next()?.split_whitespace();
    Some(Req {
        method: first.next()?.to_string(),
        target: first.next()?.to_string(),
        head: head.to_ascii_lowercase(),
        body: String::from_utf8_lossy(&buf[pos + 4..]).to_string(),
    })
}

async fn serve(make: impl FnOnce(String) -> Handler) -> Fake {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let handler = make(base.clone());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let handler = Arc::clone(&handler);
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                let Some(req) = read_request(&mut sock).await else {
                    return;
                };
                let resp = handler(&req);
                recorded.lock().unwrap().push(req);
                let mut out = format!("HTTP/1.1 {} X\r\n", resp.status);
                for (k, v) in &resp.headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str(&format!(
                    "Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp.body.len(),
                    resp.body
                ));
                let _ = sock.write_all(out.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    Fake { base, requests }
}

/// What the fake authorization server does.
#[derive(Clone, Default)]
struct AsOptions {
    cimd: bool,
    registration: bool,
    /// Access tokens the token endpoint hands out, in order.
    tokens: Vec<&'static str>,
}

/// The fake MCP endpoint: 401 with a challenge unless the request carries a
/// token in `valid`; then `initialize` and `tools/list` answers.
fn mcp_endpoint(req: &Req, base: &str, valid: &Mutex<Vec<String>>) -> Resp {
    let ok = valid.lock().unwrap().iter().any(|t| {
        req.head
            .contains(&format!("authorization: bearer {}", t.to_ascii_lowercase()))
    });
    if !ok {
        return Resp {
            status: 401,
            headers: vec![(
                "WWW-Authenticate".into(),
                format!(
                    r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/mcp", scope="files:read""#
                ),
            )],
            body: String::new(),
        };
    }
    if req.method == "DELETE" {
        return json(200, serde_json::json!({}));
    }
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    match v.get("id") {
        Some(id) => {
            let result = match v["method"].as_str() {
                Some("initialize") => serde_json::json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "serverInfo": {"name": "fake", "version": "1"},
                }),
                _ => {
                    serde_json::json!({"tools": [{"name": "t1", "inputSchema": {"type": "object"}}]})
                },
            };
            json(
                200,
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
            )
        },
        None => Resp {
            status: 202,
            headers: vec![],
            body: String::new(),
        },
    }
}

/// The MCP endpoint accepts only `Bearer <token>` for a token in `valid`.
fn oauth_server(opts: AsOptions, valid: Arc<Mutex<Vec<String>>>) -> impl FnOnce(String) -> Handler {
    move |base: String| {
        let issued = Arc::new(Mutex::new(0usize));
        Arc::new(move |req: &Req| -> Resp {
            match (req.method.as_str(), req.path()) {
                ("POST", "/mcp") | ("DELETE", "/mcp") => mcp_endpoint(req, &base, &valid),
                ("GET", "/.well-known/oauth-protected-resource/mcp") => json(
                    200,
                    serde_json::json!({
                        "resource": format!("{base}/mcp"),
                        "authorization_servers": [format!("{base}/as")],
                        "scopes_supported": ["files:read", "files:write"],
                    }),
                ),
                ("GET", "/.well-known/oauth-authorization-server/as") => {
                    let mut m = serde_json::json!({
                        "issuer": format!("{base}/as"),
                        "authorization_endpoint": format!("{base}/as/authorize"),
                        "token_endpoint": format!("{base}/as/token"),
                        "code_challenge_methods_supported": ["S256"],
                        "scopes_supported": ["files:read", "offline_access"],
                        "client_id_metadata_document_supported": opts.cimd,
                        "authorization_response_iss_parameter_supported": true,
                    });
                    if opts.registration {
                        m["registration_endpoint"] = format!("{base}/as/register").into();
                    }
                    json(200, m)
                },
                ("POST", "/as/register") => json(
                    201,
                    serde_json::json!({"client_id": "dyn-client", "token_endpoint_auth_method": "none"}),
                ),
                ("POST", "/as/token") => {
                    let form = req.form();
                    let mut n = issued.lock().unwrap();
                    if form.get("grant_type").map(String::as_str) == Some("refresh_token")
                        && form.get("refresh_token").map(String::as_str) != Some("rt-1")
                    {
                        return json(400, serde_json::json!({"error": "invalid_grant"}));
                    }
                    let Some(token) = opts.tokens.get(*n) else {
                        return json(400, serde_json::json!({"error": "invalid_grant"}));
                    };
                    *n += 1;
                    valid.lock().unwrap().push((*token).to_string());
                    json(
                        200,
                        serde_json::json!({
                            "access_token": token,
                            "token_type": "Bearer",
                            "expires_in": 3600,
                            "refresh_token": "rt-1",
                        }),
                    )
                },
                _ => json(404, serde_json::json!({})),
            }
        })
    }
}

/// A browser that approves at once: it redirects to the callback with a
/// code, the state, and `iss`.
fn approving_browser(issuer: String, seen: Arc<Mutex<Option<Url>>>) -> Browser {
    Box::new(move |url: &Url| {
        *seen.lock().unwrap() = Some(url.clone());
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        let mut target = Url::parse(&q["redirect_uri"]).unwrap();
        target
            .query_pairs_mut()
            .append_pair("code", "code-1")
            .append_pair("state", &q["state"])
            .append_pair("iss", &issuer);
        tokio::spawn(async move {
            let _ = reqwest::get(target).await;
        });
    })
}

#[tokio::test]
async fn login_with_metadata_document_then_transport_uses_the_token() {
    let valid = Arc::new(Mutex::new(Vec::new()));
    let fake = serve(oauth_server(
        AsOptions {
            cimd: true,
            registration: true,
            tokens: vec!["at-1"],
        },
        Arc::clone(&valid),
    ))
    .await;
    let store = MemStore::default();
    let seen = Arc::new(Mutex::new(None));
    let issuer = format!("{}/as", fake.base);
    let how = login_with(
        "fx",
        &fake.config(),
        &store,
        approving_browser(issuer.clone(), Arc::clone(&seen)),
        None,
    )
    .await
    .expect("login");
    assert!(how.contains("metadata document"), "{how}");
    // CIMD wins over DCR: no registration request.
    assert!(fake.to("/as/register").is_empty());

    let auth_url = seen.lock().unwrap().clone().expect("browser opened");
    let q: HashMap<String, String> = auth_url.query_pairs().into_owned().collect();
    assert_eq!(q["client_id"], flow::CLIENT_METADATA_URL);
    assert_eq!(q["resource"], format!("{}/mcp", fake.base));
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["scope"], "files:read offline_access");

    let token_req = fake.to("/as/token").remove(0).form();
    assert_eq!(token_req["grant_type"], "authorization_code");
    assert_eq!(token_req["code"], "code-1");
    assert_eq!(token_req["client_id"], flow::CLIENT_METADATA_URL);
    assert_eq!(token_req["resource"], format!("{}/mcp", fake.base));
    assert_eq!(flow::s256(&token_req["code_verifier"]), q["code_challenge"]);

    let stored = store::load(&store, "fx").expect("stored");
    assert_eq!(stored.access_token, "at-1");
    assert_eq!(stored.issuer, issuer);

    // The transport now sends the token and the server lets it in.
    let transport =
        HttpTransport::with_store("fx", &fake.config(), Some(Arc::new(clone_store(&store))))
            .unwrap();
    let mut client = McpClient::new(transport.into());
    client.initialize().await.expect("initialize with token");
    assert_eq!(client.list_tools().await.expect("tools").len(), 1);
}

fn clone_store(store: &MemStore) -> MemStore {
    MemStore {
        entries: Mutex::new(store.entries.lock().unwrap().clone()),
    }
}

#[tokio::test]
async fn login_falls_back_to_dynamic_registration_as_native_app() {
    let valid = Arc::new(Mutex::new(Vec::new()));
    let fake = serve(oauth_server(
        AsOptions {
            registration: true,
            tokens: vec!["at-1"],
            ..Default::default()
        },
        valid,
    ))
    .await;
    let store = MemStore::default();
    let how = login_with(
        "fx",
        &fake.config(),
        &store,
        approving_browser(format!("{}/as", fake.base), Arc::new(Mutex::new(None))),
        None,
    )
    .await
    .expect("login");
    assert!(how.contains("dynamically"), "{how}");
    let reg: serde_json::Value =
        serde_json::from_str(&fake.to("/as/register").remove(0).body).unwrap();
    assert_eq!(reg["application_type"], "native");
    assert_eq!(reg["token_endpoint_auth_method"], "none");
    assert!(
        reg["redirect_uris"][0]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:")
    );
    assert_eq!(
        fake.to("/as/token").remove(0).form()["client_id"],
        "dyn-client"
    );
    assert_eq!(store::load(&store, "fx").unwrap().client_id, "dyn-client");
}

#[tokio::test]
async fn wrong_iss_stops_before_the_code_is_redeemed() {
    let fake = serve(oauth_server(
        AsOptions {
            cimd: true,
            tokens: vec!["at-1"],
            ..Default::default()
        },
        Arc::new(Mutex::new(Vec::new())),
    ))
    .await;
    let store = MemStore::default();
    let err = login_with(
        "fx",
        &fake.config(),
        &store,
        approving_browser(
            "https://evil.example".to_string(),
            Arc::new(Mutex::new(None)),
        ),
        None,
    )
    .await
    .expect_err("mix-up must be refused");
    assert!(err.to_string().contains("issuer"), "{err}");
    assert!(fake.to("/as/token").is_empty(), "code never sent");
    assert!(store::load(&store, "fx").is_none());
}

#[tokio::test]
async fn no_client_option_names_the_config_to_set() {
    let fake = serve(oauth_server(
        AsOptions::default(),
        Arc::new(Mutex::new(Vec::new())),
    ))
    .await;
    let err = login_with(
        "fx",
        &fake.config(),
        &MemStore::default(),
        Box::new(|_: &Url| {}),
        None,
    )
    .await
    .expect_err("no way to get a client");
    assert!(err.to_string().contains("[mcp_servers.fx.oauth]"), "{err}");
}

fn stored_for(fake: &Fake, access: &str, expires_at: Option<u64>) -> StoredTokens {
    StoredTokens {
        server_url: fake.config().url.unwrap(),
        resource: format!("{}/mcp", fake.base),
        issuer: format!("{}/as", fake.base),
        token_endpoint: format!("{}/as/token", fake.base),
        client_id: "dyn-client".to_string(),
        client_secret: None,
        client_auth: ClientAuth::None,
        access_token: access.to_string(),
        refresh_token: Some("rt-1".to_string()),
        expires_at,
        scope: None,
    }
}

#[tokio::test]
async fn rejected_token_is_refreshed_once_and_the_request_retried() {
    let valid = Arc::new(Mutex::new(Vec::new()));
    let fake = serve(oauth_server(
        AsOptions {
            tokens: vec!["at-2"],
            ..Default::default()
        },
        Arc::clone(&valid),
    ))
    .await;
    let store = Arc::new(MemStore::default());
    // "at-old" is not valid at the server: it will answer 401.
    save(store.as_ref(), "fx", &stored_for(&fake, "at-old", None)).unwrap();
    let transport = HttpTransport::with_store("fx", &fake.config(), Some(store.clone())).unwrap();
    transport
        .send_request("tools/list", serde_json::json!({}))
        .await
        .expect("retried with the refreshed token");
    let refresh = fake.to("/as/token").remove(0).form();
    assert_eq!(refresh["grant_type"], "refresh_token");
    assert_eq!(refresh["resource"], format!("{}/mcp", fake.base));
    assert_eq!(
        store::load(store.as_ref(), "fx").unwrap().access_token,
        "at-2"
    );
}

#[tokio::test]
async fn expiring_token_is_refreshed_before_the_request() {
    let valid = Arc::new(Mutex::new(Vec::new()));
    let fake = serve(oauth_server(
        AsOptions {
            tokens: vec!["at-2"],
            ..Default::default()
        },
        Arc::clone(&valid),
    ))
    .await;
    let store = Arc::new(MemStore::default());
    save(
        store.as_ref(),
        "fx",
        &stored_for(&fake, "at-old", Some(now_secs())),
    )
    .unwrap();
    let transport = HttpTransport::with_store("fx", &fake.config(), Some(store.clone())).unwrap();
    transport
        .send_request("tools/list", serde_json::json!({}))
        .await
        .expect("fresh token");
    let mcp = fake.to("/mcp");
    assert_eq!(mcp.len(), 1, "no 401 round-trip: refreshed first");
    assert!(mcp[0].head.contains("authorization: bearer at-2"));
}

#[tokio::test]
async fn no_sign_in_reports_the_login_command() {
    let fake = serve(oauth_server(
        AsOptions::default(),
        Arc::new(Mutex::new(Vec::new())),
    ))
    .await;
    let transport = HttpTransport::with_store(
        "linear",
        &fake.config(),
        Some(Arc::new(MemStore::default())),
    )
    .unwrap();
    let err = transport
        .send_request("tools/list", serde_json::json!({}))
        .await
        .expect_err("401");
    assert!(is_auth_required(&err), "{err:#}");
    assert!(
        err.to_string().contains("mermaid mcp login linear"),
        "{err}"
    );
}

#[tokio::test]
async fn refused_refresh_reports_the_login_command() {
    let fake = serve(oauth_server(
        AsOptions::default(),
        Arc::new(Mutex::new(Vec::new())),
    ))
    .await;
    let store = Arc::new(MemStore::default());
    let mut tokens = stored_for(&fake, "at-old", None);
    tokens.refresh_token = Some("revoked".to_string());
    save(store.as_ref(), "fx", &tokens).unwrap();
    let transport = HttpTransport::with_store("fx", &fake.config(), Some(store)).unwrap();
    let err = transport
        .send_request("tools/list", serde_json::json!({}))
        .await
        .expect_err("401 after a refused refresh");
    assert!(is_auth_required(&err), "{err:#}");
}

#[tokio::test]
async fn tokens_for_another_url_are_not_sent() {
    let fake = serve(oauth_server(
        AsOptions::default(),
        Arc::new(Mutex::new(vec!["at-1".to_string()])),
    ))
    .await;
    let store = Arc::new(MemStore::default());
    let mut tokens = stored_for(&fake, "at-1", None);
    tokens.server_url = "https://elsewhere.example/mcp".to_string();
    save(store.as_ref(), "fx", &tokens).unwrap();
    let transport = HttpTransport::with_store("fx", &fake.config(), Some(store)).unwrap();
    let err = transport
        .send_request("tools/list", serde_json::json!({}))
        .await
        .expect_err("not signed in for this url");
    assert!(is_auth_required(&err), "{err:#}");
    assert!(!fake.to("/mcp")[0].head.contains("authorization"));
}

#[tokio::test]
async fn own_authorization_header_turns_oauth_off() {
    let fake = serve(oauth_server(
        AsOptions::default(),
        Arc::new(Mutex::new(Vec::new())),
    ))
    .await;
    let mut config = fake.config();
    config
        .headers
        .insert("Authorization".to_string(), "Bearer mine".to_string());
    let transport =
        HttpTransport::with_store("fx", &config, Some(Arc::new(MemStore::default()))).unwrap();
    let err = transport
        .send_request("tools/list", serde_json::json!({}))
        .await
        .expect_err("401");
    assert!(!is_auth_required(&err), "{err:#}");
    assert!(err.to_string().contains("401"), "{err}");
}

#[test]
fn insufficient_scope_is_read_from_a_403_challenge() {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::WWW_AUTHENTICATE,
        HeaderValue::from_static(r#"Bearer error="insufficient_scope", scope="files:write""#),
    );
    assert_eq!(insufficient_scope(&headers).as_deref(), Some("files:write"));
    assert_eq!(insufficient_scope(&HeaderMap::new()), None);
}

#[test]
fn client_metadata_document_matches_the_code() {
    let doc: serde_json::Value = serde_json::from_str(include_str!(
        "../../../packaging/pages/oauth/client-metadata.json"
    ))
    .unwrap();
    // The spec: client_id must equal the document's URL exactly.
    assert_eq!(doc["client_id"], flow::CLIENT_METADATA_URL);
    assert_eq!(doc["client_name"], flow::CLIENT_NAME);
    assert_eq!(doc["token_endpoint_auth_method"], "none");
    let redirects: Vec<&str> = doc["redirect_uris"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        redirects.contains(&"http://127.0.0.1/callback"),
        "{redirects:?}"
    );
}

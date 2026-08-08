//! Built-in MCP server registry and resolution chain.
//!
//! Resolution order:
//! A) Built-in registry — instant, offline, covers popular servers
//! B) Convention-based — try common npm package naming patterns
//! C) npm registry search — network lookup for unknown servers
//!
//! Entries may target either npm (`command: "npx"`, runs via
//! `npx -y <package>`) or PyPI (`command: "uvx"`, runs via
//! `uvx <package>`). Verify each entry against the appropriate
//! registry before releases:
//!
//! ```text
//! npm view <pkg> version deprecated
//! pip index versions <pkg>
//! ```
//!
//! These entries are load-bearing for the `mermaid add <name>` UX —
//! a stale or 404 package here produces a confusing first-run error.

use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use super::client::McpClient;
use super::transport::StdioTransport;
use mermaid_model::utils::{is_affirmative, should_refuse_noninteractive};

/// A resolved MCP server ready for configuration
pub struct ResolvedServer {
    /// Launcher command: "npx" (npm packages) or "uvx" (Python packages).
    pub command: String,
    pub package: String,
    pub env_vars: Vec<(String, String)>, // (name, description)
    pub extra_args: Vec<String>,
}

/// Built-in registry entry
struct RegistryEntry {
    name: &'static str,
    /// Launcher command: "npx" (npm) or "uvx" (PyPI).
    command: &'static str,
    package: &'static str,
    description: &'static str,
    env_vars: &'static [(&'static str, &'static str)],
    extra_args: &'static [&'static str],
}

/// The built-in registry of popular MCP servers.
///
/// Last npm/PyPI verification: 2026-04-16. TODO: re-verify quarterly —
/// stale/deprecated packages produce confusing `mermaid add` errors.
const REGISTRY: &[RegistryEntry] = &[
    RegistryEntry {
        name: "context7",
        command: "npx",
        package: "@upstash/context7-mcp",
        description: "Up-to-date library documentation and code examples",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "filesystem",
        command: "npx",
        package: "@modelcontextprotocol/server-filesystem",
        description: "Secure file operations with configurable access",
        env_vars: &[],
        extra_args: &["."],
    },
    RegistryEntry {
        name: "memory",
        command: "npx",
        package: "@modelcontextprotocol/server-memory",
        description: "Persistent memory via knowledge graph",
        env_vars: &[],
        extra_args: &[],
    },
    // Python-based MCP reference servers — published to PyPI, launched via uvx.
    RegistryEntry {
        name: "fetch",
        command: "uvx",
        package: "mcp-server-fetch",
        description: "Web content fetching and conversion (PyPI, uvx)",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "git",
        command: "uvx",
        package: "mcp-server-git",
        description: "Git repository tools — log, diff, status, blame (PyPI, uvx)",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "time",
        command: "uvx",
        package: "mcp-server-time",
        description: "Time and timezone conversion (PyPI, uvx)",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "playwright",
        command: "npx",
        package: "@playwright/mcp",
        description: "Browser automation and testing",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "notion",
        command: "npx",
        package: "@notionhq/notion-mcp-server",
        description: "Notion workspace — pages, databases, tasks",
        env_vars: &[("NOTION_API_KEY", "Notion API integration token")],
        extra_args: &[],
    },
    RegistryEntry {
        name: "slack",
        // @modelcontextprotocol/server-slack was deprecated 2026-02.
        // Maintenance handed off to Zencoder per upstream README.
        command: "npx",
        package: "@zencoderai/slack-mcp-server",
        description: "Slack messaging and channel management (maintained by Zencoder; handoff from deprecated @modelcontextprotocol/server-slack)",
        env_vars: &[
            ("SLACK_BOT_TOKEN", "Slack bot token (xoxb-...)"),
            ("SLACK_TEAM_ID", "Slack workspace/team ID"),
        ],
        extra_args: &[],
    },
    RegistryEntry {
        name: "postgres",
        // @modelcontextprotocol/server-postgres was archived 2026-02
        // with no official successor. crystaldba/postgres-mcp is the
        // most-cited community replacement (PyPI, uvx). Env var renamed
        // `DATABASE_URL` → `DATABASE_URI` per crystaldba convention.
        command: "uvx",
        package: "postgres-mcp",
        description: "PostgreSQL queries (community, crystaldba): RW access, EXPLAIN, index tuning, health checks",
        env_vars: &[(
            "DATABASE_URI",
            "PostgreSQL connection string (e.g., postgresql://user:pass@localhost:5432/db)",
        )],
        extra_args: &[],
    },
    RegistryEntry {
        name: "sequential-thinking",
        command: "npx",
        package: "@modelcontextprotocol/server-sequential-thinking",
        description: "Dynamic problem-solving through thought sequences",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "brave-search",
        // @modelcontextprotocol/server-brave-search was deprecated 2026-02.
        // Brave now publishes the official server themselves. Requires
        // `--transport stdio` flag (package supports multiple transports;
        // we always want stdio for our launcher pattern).
        command: "npx",
        package: "@brave/brave-search-mcp-server",
        description: "Brave Search (official, brave-maintained): web, local, image, video, news, AI summary",
        env_vars: &[(
            "BRAVE_API_KEY",
            "Brave Search API key (https://brave.com/search/api/)",
        )],
        extra_args: &["--transport", "stdio"],
    },
    RegistryEntry {
        name: "everything",
        command: "npx",
        package: "@modelcontextprotocol/server-everything",
        description: "Reference/test server with all MCP features",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "supabase",
        command: "npx",
        package: "@supabase/mcp-server-supabase",
        description: "Supabase — database, auth, edge functions",
        env_vars: &[
            ("SUPABASE_URL", "Supabase project URL"),
            ("SUPABASE_SERVICE_ROLE_KEY", "Supabase service role key"),
        ],
        extra_args: &[],
    },
    RegistryEntry {
        name: "perplexity",
        command: "npx",
        package: "perplexity-mcp",
        description: "Perplexity AI search API",
        env_vars: &[("PERPLEXITY_API_KEY", "Perplexity API key")],
        extra_args: &[],
    },
    RegistryEntry {
        name: "docker",
        command: "npx",
        package: "mcp-server-docker",
        description: "Docker container management (community)",
        env_vars: &[],
        extra_args: &[],
    },
    // Note: the official GitHub MCP server is distributed as a Go binary
    // (github.com/github/github-mcp-server), not an npm or PyPI package.
    // The previous @modelcontextprotocol/server-github npm package is
    // deprecated. Users who want GitHub MCP should install the Go binary
    // manually and add a custom entry to config.toml.
];

/// Step A: Look up in the built-in registry
fn lookup(name: &str) -> Option<&'static RegistryEntry> {
    REGISTRY.iter().find(|e| e.name == name)
}

/// Reject a package name that npx/uvx would parse as an option rather than a
/// package: an empty name, or one beginning with `-` (F78). The launcher places
/// the package immediately after `npx -y` / `uvx` with no `--` end-of-options
/// guard, so a leading-dash name (e.g. `--registry=http://evil`) would be
/// swallowed as a flag. Real npm/PyPI names are `@scope/...` or start with an
/// alphanumeric; a dash-leading name only arises from the untrusted
/// search/convention resolution path.
fn validate_package_name(package: &str) -> Result<()> {
    if package.is_empty() {
        bail!("refusing to launch an MCP server with an empty package name");
    }
    if package.starts_with('-') {
        bail!(
            "refusing to launch MCP package '{package}': a name beginning with '-' would be \
             parsed as a command-line flag by the launcher, not a package name"
        );
    }
    Ok(())
}

/// Build the arg vector for a launcher command + package.
///
/// - `npx` uses `["-y", <package>, ...extra_args]` (auto-installs).
/// - `uvx` uses `[<package>, ...extra_args]` (no `-y` flag).
///
/// Validates the package name first (F78): a dash-leading or empty name is
/// refused so it can't be smuggled into the launcher argv as a flag.
fn build_launch_args(command: &str, package: &str, extra_args: &[String]) -> Result<Vec<String>> {
    validate_package_name(package)?;
    let mut args = match command {
        "npx" => vec!["-y".to_string(), package.to_string()],
        _ => vec![package.to_string()], // uvx and any other launcher
    };
    args.extend_from_slice(extra_args);
    Ok(args)
}

/// Versions to pin curated registry packages to, so `mermaid add <name>` installs
/// an audited release instead of always-`latest` (#F51). **Empty by default** —
/// pinning a curated package is a maintainer decision made with verified versions
/// at the quarterly registry re-verification (see the module header); until a
/// version is listed here a curated entry tracks `latest`, gated by the
/// consent prompt. (Users can already pin their own config-defined servers by
/// writing `package@version` directly — `build_launch_args` passes it verbatim.)
const PINNED_VERSIONS: &[(&str, &str)] = &[];

/// Append the configured `@version` to a curated package name, if one is pinned.
fn pin_curated_package(package: &str) -> String {
    apply_pin(package, PINNED_VERSIONS)
}

fn apply_pin(package: &str, pins: &[(&str, &str)]) -> String {
    match pins.iter().find(|(p, _)| *p == package) {
        Some((_, version)) => format!("{package}@{version}"),
        None => package.to_string(),
    }
}

/// Validate an MCP server by spawning it, initializing, and listing tools.
/// Returns tool names on success. Kills the process after validation.
pub async fn validate_server(
    command: &str,
    package: &str,
    extra_args: &[String],
    env: &HashMap<String, String>,
) -> Result<Vec<String>> {
    let args = build_launch_args(command, package, extra_args)?;
    validate_argv(command, &args, env).await
}

/// Spawn `command argv`, MCP-initialize, list tools, then shut the child down on
/// every path. Shared by registry validation and raw `--command` registration
/// (which passes its verbatim argv, bypassing the package-launcher conventions).
pub async fn validate_argv(
    command: &str,
    argv: &[String],
    env: &HashMap<String, String>,
) -> Result<Vec<String>> {
    let transport = tokio::time::timeout(
        Duration::from_secs(60),
        StdioTransport::spawn(command, argv, env),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "Server startup timed out (60s). Is {} installed?",
            match command {
                "npx" => "Node.js/npx",
                "uvx" => "uv/uvx",
                other => other,
            }
        )
    })?
    .map_err(|e| anyhow!("Failed to spawn server: {}", e))?;

    let mut client = McpClient::new(transport.into());

    let result = tokio::time::timeout(Duration::from_secs(60), async {
        client.initialize().await?;
        let tools = client.list_tools().await?;
        let tool_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
        Ok::<Vec<String>, anyhow::Error>(tool_names)
    })
    .await;

    // Gracefully shut the validation child down on EVERY path — success, an
    // initialize/list_tools error, or the timeout — instead of only on success.
    // The old `?` short-circuit skipped this and fell back to `kill_on_drop`,
    // bypassing the close-stdin -> SIGTERM -> SIGKILL escalation (#139).
    client.shutdown().await;

    match result {
        Ok(inner) => inner,
        Err(_) => Err(anyhow!("Server initialization timed out (60s)")),
    }
}

/// Validate a Streamable HTTP MCP server config: connect, initialize, list
/// tools, then end the session on every path. Sibling of [`validate_argv`]
/// for url-shaped configs.
pub async fn validate_http(config: &crate::domain::McpServerConfig) -> Result<Vec<String>> {
    let transport = super::transport_http::HttpTransport::new(config)?;
    let mut client = McpClient::new(transport.into());

    let result = tokio::time::timeout(Duration::from_secs(60), async {
        client.initialize().await?;
        let tools = client.list_tools().await?;
        let tool_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
        Ok::<Vec<String>, anyhow::Error>(tool_names)
    })
    .await;

    // End the session on EVERY path — success, error, or timeout (#139).
    client.shutdown().await;

    match result {
        Ok(inner) => inner,
        Err(_) => Err(anyhow!("Server initialization timed out (60s)")),
    }
}

/// The npm package-name conventions tried for an unknown server name. Pure (no
/// I/O) so the set and ordering are unit-tested.
fn convention_patterns(name: &str) -> Vec<String> {
    vec![
        format!("@{}/mcp-server", name),
        format!("{}-mcp-server", name),
        format!("@modelcontextprotocol/server-{}", name),
        format!("{}-mcp", name),
    ]
}

/// Build the npm packument URL for `package`, percent-encoding the whole name as
/// ONE path segment (F77). Pushing the name through the `url` crate encodes every
/// character that isn't valid in a path segment — the scope `/` (→ `%2F`), plus
/// `?`, `#`, `%`, space, … — so a name carrying those reaches the registry as the
/// correct path instead of being truncated into a query/fragment or split into
/// extra segments. The old single-char `replace('/', "%2F")` left every other
/// special char unescaped, probing the wrong URL (false negative/positive).
fn npm_packument_url(package: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse("https://registry.npmjs.org/")
        .map_err(|e| anyhow!("failed to build npm packument base URL: {}", e))?;
    url.path_segments_mut()
        // Infallible for an https base, but `path_segments_mut` is fallible for
        // cannot-be-a-base URLs, so handle it rather than unwrap.
        .map_err(|_| anyhow!("npm registry base URL cannot be a base"))?
        // The base path is exactly "/" (one empty segment); drop it so the result
        // is `/<name>`, not `//<name>`.
        .pop_if_empty()
        .push(package);
    Ok(url)
}

/// Check whether an npm package *exists* via a registry metadata lookup —
/// WITHOUT executing it. This is the #10 fix: the old code probed convention
/// names by running `npx -y <guess>`, so a typosquatted guess executed before
/// any confirmation. A metadata GET never runs the package.
async fn npm_package_exists(client: &reqwest::Client, package: &str) -> Result<bool> {
    let url = npm_packument_url(package)?;
    let response = client
        .get(url)
        // Abbreviated packument — we only inspect the status code.
        .header("Accept", "application/vnd.npm.install-v1+json")
        .send()
        .await
        .map_err(|e| anyhow!("npm registry lookup failed (network unavailable?): {}", e))?;
    match response.status() {
        reqwest::StatusCode::OK => Ok(true),
        reqwest::StatusCode::NOT_FOUND => Ok(false),
        other => Err(anyhow!(
            "npm registry returned HTTP {} for '{}'",
            other,
            package
        )),
    }
}

/// Step B: probe convention-based npm names for *existence* (a metadata
/// lookup), NOT by spawning them (the #10 RCE). Returns the first that exists.
async fn try_conventions(client: &reqwest::Client, name: &str) -> Option<String> {
    for pattern in convention_patterns(name) {
        println!("  Checking npm for {}...", pattern);
        if npm_package_exists(client, &pattern).await.unwrap_or(false) {
            return Some(pattern);
        }
    }
    None
}

/// Confirm (default NO) before fetching+running a package that is NOT in the
/// trusted built-in registry — the #10 gate. `assume_yes` (`--yes`) is an
/// explicit opt-in for scripted use; without it a non-interactive session
/// refuses rather than silently running untrusted code.
fn confirm_untrusted_package(package: &str, command: &str, assume_yes: bool) -> Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    if should_refuse_noninteractive(io::stdin().is_terminal(), assume_yes) {
        bail!(
            "Refusing to fetch and run untrusted package '{package}' via `{command} -y {package}` \
             non-interactively — it is not in Mermaid's trusted registry, and a typosquatted \
             package could run arbitrary code. Re-run in an interactive terminal to confirm, or \
             pass --yes to allow it."
        );
    }
    print!(
        "About to fetch and run UNTRUSTED package '{package}' via `{command} -y {package}`.\n\
         It is not in Mermaid's trusted registry; a typosquatted package could run arbitrary \
         code. Continue? [y/N]: "
    );
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(is_affirmative(input.trim()))
}

/// Confirm before fetching+running a *curated* registry package (#F51). Even a
/// trusted entry pulls the LATEST release from npm/PyPI on every launch and runs
/// it with the user's privileges, so a compromised upstream release would
/// execute without notice — "in the registry" is not "pinned to audited code".
/// Surface the exact command and require consent; `assume_yes` (`--yes`) bypasses
/// for scripted use, and a non-interactive session without it fails closed.
/// (Version pinning would remove the always-latest risk outright, but the
/// built-in entries carry no pinned versions to launch.)
fn confirm_registry_launch(package: &str, command: &str, assume_yes: bool) -> Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    if should_refuse_noninteractive(io::stdin().is_terminal(), assume_yes) {
        bail!(
            "Refusing to fetch and run '{package}' via `{command} {package}` non-interactively: it \
             installs and executes the latest release from a third-party registry with your \
             privileges. Re-run in an interactive terminal to confirm, or pass --yes to allow it."
        );
    }
    print!(
        "About to fetch and run the latest '{package}' via `{command} {package}`.\n\
         This installs and executes third-party code with your privileges (a compromised release \
         would run too). Continue? [y/N]: "
    );
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(is_affirmative(input.trim()))
}

/// Step C: Search npm registry for MCP server packages.
///
/// Query-param encoding is handled by `reqwest::Url::parse_with_params`,
/// which delegates to the `url` crate's RFC-3986 form-urlencoded
/// serializer. No hand-rolled escaping — special characters in `name`
/// (%, &, =, UTF-8, …) are all handled correctly.
async fn search_npm(client: &reqwest::Client, name: &str) -> Result<Option<(String, String)>> {
    let query = format!("{} mcp server", name);

    let url = reqwest::Url::parse_with_params(
        "https://registry.npmjs.org/-/v1/search",
        &[("text", query.as_str()), ("size", "5")],
    )
    .map_err(|e| anyhow!("Failed to build npm search URL: {}", e))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| anyhow!("npm registry search failed (network unavailable?): {}", e))?;

    if !response.status().is_success() {
        return Err(anyhow!("npm registry returned HTTP {}", response.status()));
    }

    let body: serde_json::Value = response.json().await?;
    let objects = body
        .get("objects")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    for obj in &objects {
        let pkg_name = obj
            .pointer("/package/name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let description = obj
            .pointer("/package/description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let keywords = obj
            .pointer("/package/keywords")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();

        // Check if this looks like an MCP server
        let combined = format!("{} {} {}", pkg_name, description, keywords).to_lowercase();
        if combined.contains("mcp") {
            return Ok(Some((pkg_name.to_string(), description.to_string())));
        }
    }

    Ok(None)
}

/// Resolve an MCP server name to a ready-to-configure server.
/// Tries: A (built-in registry, trusted) → B (npm convention names) →
/// C (npm search). Any non-registry result must be confirmed before it is
/// returned, because configuring it leads to executing it via `npx -y` (#10).
/// `assume_yes` (from `--yes`) is an explicit opt-in for non-interactive use.
pub async fn resolve(name: &str, assume_yes: bool) -> Result<ResolvedServer> {
    // Step A: Built-in registry — curated, but still fetches+runs the latest
    // third-party release, so require informed consent (bypassable with --yes)
    // rather than executing it silently (#F51).
    if let Some(entry) = lookup(name) {
        println!("Found: {} ({})", entry.package, entry.description);
        if !confirm_registry_launch(entry.package, entry.command, assume_yes)? {
            bail!("Cancelled: did not confirm running '{}'.", entry.package);
        }
        return Ok(ResolvedServer {
            command: entry.command.to_string(),
            // Pin to an audited version when one is configured (#F51); else latest.
            package: pin_curated_package(entry.package),
            env_vars: entry
                .env_vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            extra_args: entry.extra_args.iter().map(|s| s.to_string()).collect(),
        });
    }

    println!("Not in built-in registry, trying conventions...");

    // One HTTP client shared by the existence check (B) and the search (C).
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    // Step B: convention names — existence check only (no spawn), then confirm.
    if let Some(package) = try_conventions(&client, name).await {
        // Refuse a dash-leading/empty name before it reaches the launcher (F78).
        validate_package_name(&package)?;
        println!("Found: {}", package);
        if !confirm_untrusted_package(&package, "npx", assume_yes)? {
            bail!(
                "Cancelled: did not confirm running untrusted package '{}'.",
                package
            );
        }
        return Ok(ResolvedServer {
            command: "npx".to_string(),
            package,
            env_vars: Vec::new(),
            extra_args: Vec::new(),
        });
    }

    println!("Searching npm registry...");

    // Step C: npm search — HTTP only (safe). Confirm before returning; the
    // package is validated (executed) exactly once afterwards, by `add_server`.
    match search_npm(&client, name).await {
        Ok(Some((package, description))) => {
            // Refuse a dash-leading/empty name before it reaches the launcher (F78).
            validate_package_name(&package)?;
            println!("Found: {} — {}", package, description);
            if !confirm_untrusted_package(&package, "npx", assume_yes)? {
                bail!(
                    "Cancelled: did not confirm running untrusted package '{}'.",
                    package
                );
            }
            Ok(ResolvedServer {
                command: "npx".to_string(),
                package,
                env_vars: Vec::new(),
                extra_args: Vec::new(),
            })
        },
        Ok(None) => Err(anyhow!(
            "Could not find MCP server '{}'\n\n\
            You can add it manually in ~/.config/mermaid/config.toml:\n\
            [mcp_servers.{}]\n\
            command = \"npx\"\n\
            args = [\"-y\", \"PACKAGE_NAME\"]",
            name,
            name
        )),
        Err(e) => Err(anyhow!(
            "Convention-based lookup failed, and npm search also failed: {}\n\n\
            You can add it manually in ~/.config/mermaid/config.toml:\n\
            [mcp_servers.{}]\n\
            command = \"npx\"\n\
            args = [\"-y\", \"PACKAGE_NAME\"]",
            e,
            name
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every registry entry must use a supported launcher and have a
    /// non-empty package. Guards against typo-level regressions when
    /// adding / updating entries.
    #[test]
    fn apply_pin_appends_version_when_pinned() {
        // #F51 mechanism: a pinned curated package gets `@version`; an unpinned
        // one is unchanged. (The shipped PINNED_VERSIONS is empty by design.)
        let pins = &[("@upstash/context7-mcp", "1.2.3")][..];
        assert_eq!(
            apply_pin("@upstash/context7-mcp", pins),
            "@upstash/context7-mcp@1.2.3"
        );
        assert_eq!(apply_pin("mcp-server-git", pins), "mcp-server-git");
        // Shipped default pins nothing.
        assert!(PINNED_VERSIONS.is_empty());
    }

    #[test]
    fn registry_entries_are_well_formed() {
        assert!(!REGISTRY.is_empty(), "registry must not be empty");
        for entry in REGISTRY {
            assert!(
                matches!(entry.command, "npx" | "uvx"),
                "entry {:?} has unsupported launcher {:?}",
                entry.name,
                entry.command
            );
            assert!(
                !entry.package.is_empty(),
                "entry {:?} has empty package",
                entry.name
            );
            assert!(
                !entry.name.is_empty(),
                "registry entry has empty name (package: {:?})",
                entry.package
            );
        }
    }

    /// Regression guard: no deprecated modelcontextprotocol npm packages
    /// should remain in the registry. @modelcontextprotocol/server-slack,
    /// -postgres, -brave-search were all deprecated upstream in 2026-02.
    #[test]
    fn registry_does_not_reference_deprecated_modelcontextprotocol_packages() {
        let deprecated = [
            "@modelcontextprotocol/server-slack",
            "@modelcontextprotocol/server-postgres",
            "@modelcontextprotocol/server-brave-search",
            "@modelcontextprotocol/server-github",
        ];
        for entry in REGISTRY {
            for pkg in &deprecated {
                assert_ne!(
                    entry.package, *pkg,
                    "registry entry {:?} still references deprecated package {}",
                    entry.name, pkg
                );
            }
        }
    }

    #[test]
    fn lookup_resolves_replacement_packages() {
        // Sanity: the three replacement entries land where expected.
        assert_eq!(
            lookup("slack").unwrap().package,
            "@zencoderai/slack-mcp-server"
        );
        assert_eq!(lookup("postgres").unwrap().package, "postgres-mcp");
        assert_eq!(lookup("postgres").unwrap().command, "uvx");
        assert_eq!(
            lookup("brave-search").unwrap().package,
            "@brave/brave-search-mcp-server"
        );
        assert_eq!(
            lookup("brave-search").unwrap().extra_args,
            &["--transport", "stdio"]
        );
    }

    #[test]
    fn convention_patterns_are_exact_and_ordered() {
        assert_eq!(
            convention_patterns("foo"),
            vec![
                "@foo/mcp-server".to_string(),
                "foo-mcp-server".to_string(),
                "@modelcontextprotocol/server-foo".to_string(),
                "foo-mcp".to_string(),
            ]
        );
    }

    #[test]
    fn assume_yes_confirms_without_io() {
        // --yes short-circuits before any TTY check / stdin read, so it never
        // blocks. (The assume_yes=false path is NOT exercised here: under a TTY
        // it would block on stdin.read_line.)
        assert!(confirm_untrusted_package("evil-pkg", "npx", true).unwrap());
    }

    #[tokio::test]
    async fn resolve_registry_name_needs_no_network() {
        // A trusted built-in entry resolves via lookup() alone — no HTTP. With
        // `--yes` the curated-launch consent gate is bypassed, so this exercises
        // the no-network path without prompting (#F51).
        let resolved = resolve("context7", true).await.expect("registry resolve");
        assert_eq!(resolved.command, "npx");
        assert_eq!(resolved.package, "@upstash/context7-mcp");
    }

    #[tokio::test]
    async fn resolve_registry_name_fails_closed_without_consent() {
        // #F51: a curated entry now requires consent before its latest release is
        // fetched and run. Non-interactively (test stdin is not a TTY) and without
        // `--yes`, resolution must refuse rather than silently execute it.
        assert!(
            resolve("context7", false).await.is_err(),
            "curated launch must fail closed without consent",
        );
    }

    // F77: the existence-probe URL must percent-encode the WHOLE package name as
    // one path segment — not just the scope slash.
    #[test]
    fn npm_packument_url_encodes_whole_name_as_one_segment() {
        // Scoped name: slash → %2F, single path segment, `@` left literal.
        assert_eq!(
            npm_packument_url("@scope/name").unwrap().as_str(),
            "https://registry.npmjs.org/@scope%2Fname"
        );
        // Plain name unchanged.
        assert_eq!(
            npm_packument_url("mcp-server-fetch").unwrap().as_str(),
            "https://registry.npmjs.org/mcp-server-fetch"
        );
        // `?` `#` `%` must be percent-encoded INTO the path — not begin a query or
        // fragment, and not pass through raw (the old replace('/', "%2F") bug).
        let u = npm_packument_url("weird?#%name").unwrap();
        assert!(
            u.query().is_none(),
            "'?' must be encoded, not begin a query: {u}"
        );
        assert!(
            u.fragment().is_none(),
            "'#' must be encoded, not begin a fragment: {u}"
        );
        let s = u.as_str();
        assert!(s.contains("%3F"), "'?' should be percent-encoded: {s}");
        assert!(s.contains("%23"), "'#' should be percent-encoded: {s}");
        assert!(s.contains("%25"), "'%' should be percent-encoded: {s}");
    }

    // F78: a package name that npx/uvx would parse as a flag must be refused.
    #[test]
    fn validate_package_name_rejects_dash_and_empty() {
        assert!(validate_package_name("").is_err());
        assert!(validate_package_name("-rf").is_err());
        assert!(validate_package_name("--registry=http://evil").is_err());
        // Real package names pass.
        assert!(validate_package_name("@scope/pkg").is_ok());
        assert!(validate_package_name("mcp-server-fetch").is_ok());
        assert!(validate_package_name("postgres-mcp").is_ok());
    }

    #[test]
    fn build_launch_args_rejects_leading_dash_and_builds_argv() {
        // Dash-leading / empty packages are refused before launch (F78).
        assert!(build_launch_args("npx", "-evil", &[]).is_err());
        assert!(build_launch_args("uvx", "", &[]).is_err());
        // Valid packages build the expected argv (package never first; npx keeps -y).
        let npx = build_launch_args("npx", "@scope/pkg", &["x".to_string()]).unwrap();
        assert_eq!(
            npx,
            vec!["-y".to_string(), "@scope/pkg".to_string(), "x".to_string()]
        );
        let uvx = build_launch_args("uvx", "mcp-server-git", &[]).unwrap();
        assert_eq!(uvx, vec!["mcp-server-git".to_string()]);
    }
}

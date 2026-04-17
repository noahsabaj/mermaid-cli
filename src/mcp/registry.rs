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

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::time::Duration;

use super::client::McpClient;
use super::transport::StdioTransport;

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

/// Build the arg vector for a launcher command + package.
///
/// - `npx` uses `["-y", <package>, ...extra_args]` (auto-installs).
/// - `uvx` uses `[<package>, ...extra_args]` (no `-y` flag).
fn build_launch_args(command: &str, package: &str, extra_args: &[String]) -> Vec<String> {
    let mut args = match command {
        "npx" => vec!["-y".to_string(), package.to_string()],
        _ => vec![package.to_string()], // uvx and any other launcher
    };
    args.extend_from_slice(extra_args);
    args
}

/// Validate an MCP server by spawning it, initializing, and listing tools.
/// Returns tool names on success. Kills the process after validation.
pub async fn validate_server(
    command: &str,
    package: &str,
    extra_args: &[String],
    env: &HashMap<String, String>,
) -> Result<Vec<String>> {
    let args = build_launch_args(command, package, extra_args);

    let transport = tokio::time::timeout(
        Duration::from_secs(60),
        StdioTransport::spawn(command, &args, env),
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

    let mut client = McpClient::new(transport);

    tokio::time::timeout(Duration::from_secs(60), async {
        client.initialize().await?;
        let tools = client.list_tools().await?;
        let tool_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
        client.shutdown().await;
        Ok::<Vec<String>, anyhow::Error>(tool_names)
    })
    .await
    .map_err(|_| anyhow!("Server initialization timed out (60s)"))?
}

/// Step B: Try convention-based package name patterns (npm only).
async fn try_conventions(name: &str) -> Option<String> {
    let patterns = [
        format!("@{}/mcp-server", name),
        format!("{}-mcp-server", name),
        format!("@modelcontextprotocol/server-{}", name),
        format!("{}-mcp", name),
    ];

    for pattern in &patterns {
        println!("  Trying {}...", pattern);
        let empty_env = HashMap::new();
        if validate_server("npx", pattern, &[], &empty_env)
            .await
            .is_ok()
        {
            return Some(pattern.clone());
        }
    }

    None
}

/// Step C: Search npm registry for MCP server packages.
///
/// Query-param encoding is handled by `reqwest::Url::parse_with_params`,
/// which delegates to the `url` crate's RFC-3986 form-urlencoded
/// serializer. No hand-rolled escaping — special characters in `name`
/// (%, &, =, UTF-8, …) are all handled correctly.
async fn search_npm(name: &str) -> Result<Option<(String, String)>> {
    let query = format!("{} mcp server", name);

    let url = reqwest::Url::parse_with_params(
        "https://registry.npmjs.org/-/v1/search",
        &[("text", query.as_str()), ("size", "5")],
    )
    .map_err(|e| anyhow!("Failed to build npm search URL: {}", e))?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

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

/// Resolve an MCP server name to a validated, ready-to-configure server.
/// Tries: A (built-in) → B (convention) → C (npm search)
pub async fn resolve(name: &str) -> Result<ResolvedServer> {
    // Step A: Built-in registry
    if let Some(entry) = lookup(name) {
        println!("Found: {} ({})", entry.package, entry.description);
        return Ok(ResolvedServer {
            command: entry.command.to_string(),
            package: entry.package.to_string(),
            env_vars: entry
                .env_vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            extra_args: entry.extra_args.iter().map(|s| s.to_string()).collect(),
        });
    }

    println!("Not in built-in registry, trying conventions...");

    // Step B: Convention-based (npm only)
    if let Some(package) = try_conventions(name).await {
        println!("Found: {}", package);
        return Ok(ResolvedServer {
            command: "npx".to_string(),
            package,
            env_vars: Vec::new(),
            extra_args: Vec::new(),
        });
    }

    println!("Searching npm registry...");

    // Step C: npm search
    match search_npm(name).await {
        Ok(Some((package, description))) => {
            println!("Found: {} — {}", package, description);

            // Validate the npm result actually works
            let empty_env = HashMap::new();
            validate_server("npx", &package, &[], &empty_env)
                .await
                .map_err(|e| {
                    anyhow!(
                        "Found npm package '{}' but it failed validation: {}",
                        package,
                        e
                    )
                })?;

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
}

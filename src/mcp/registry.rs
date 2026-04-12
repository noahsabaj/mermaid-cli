//! Built-in MCP server registry and resolution chain.
//!
//! Resolution order:
//! A) Built-in registry — instant, offline, covers popular servers
//! B) Convention-based — try common npm package naming patterns
//! C) npm registry search — network lookup for unknown servers

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::time::Duration;

use super::client::McpClient;
use super::transport::StdioTransport;

/// A resolved MCP server ready for configuration
pub struct ResolvedServer {
    pub package: String,
    pub env_vars: Vec<(String, String)>, // (name, description)
    pub extra_args: Vec<String>,
}

/// Built-in registry entry
struct RegistryEntry {
    name: &'static str,
    package: &'static str,
    description: &'static str,
    env_vars: &'static [(&'static str, &'static str)],
    extra_args: &'static [&'static str],
}

/// The built-in registry of popular MCP servers
const REGISTRY: &[RegistryEntry] = &[
    RegistryEntry {
        name: "context7",
        package: "@upstash/context7-mcp",
        description: "Up-to-date library documentation and code examples",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "github",
        package: "@github/github-mcp-server",
        description: "GitHub API — repos, issues, PRs, code search",
        env_vars: &[("GITHUB_PERSONAL_ACCESS_TOKEN", "GitHub personal access token (https://github.com/settings/tokens)")],
        extra_args: &[],
    },
    RegistryEntry {
        name: "filesystem",
        package: "@modelcontextprotocol/server-filesystem",
        description: "Secure file operations with configurable access",
        env_vars: &[],
        extra_args: &["."],
    },
    RegistryEntry {
        name: "memory",
        package: "@modelcontextprotocol/server-memory",
        description: "Persistent memory via knowledge graph",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "fetch",
        package: "@modelcontextprotocol/server-fetch",
        description: "Web content fetching and conversion",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "git",
        package: "@modelcontextprotocol/server-git",
        description: "Git repository tools — log, diff, status, blame",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "playwright",
        package: "@playwright/mcp",
        description: "Browser automation and testing",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "notion",
        package: "@notionhq/notion-mcp-server",
        description: "Notion workspace — pages, databases, tasks",
        env_vars: &[("NOTION_API_KEY", "Notion API integration token")],
        extra_args: &[],
    },
    RegistryEntry {
        name: "slack",
        package: "@anthropic/mcp-server-slack",
        description: "Slack messaging and channel management",
        env_vars: &[
            ("SLACK_BOT_TOKEN", "Slack bot token (xoxb-...)"),
            ("SLACK_TEAM_ID", "Slack workspace/team ID"),
        ],
        extra_args: &[],
    },
    RegistryEntry {
        name: "postgres",
        package: "@modelcontextprotocol/server-postgres",
        description: "PostgreSQL database queries",
        env_vars: &[("DATABASE_URL", "PostgreSQL connection string (e.g., postgresql://user:pass@localhost/db)")],
        extra_args: &[],
    },
    RegistryEntry {
        name: "sequential-thinking",
        package: "@modelcontextprotocol/server-sequential-thinking",
        description: "Dynamic problem-solving through thought sequences",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "brave-search",
        package: "@anthropic/mcp-server-brave-search",
        description: "Brave Search API — web, images, news",
        env_vars: &[("BRAVE_API_KEY", "Brave Search API key (https://brave.com/search/api/)")],
        extra_args: &[],
    },
    RegistryEntry {
        name: "time",
        package: "@modelcontextprotocol/server-time",
        description: "Time and timezone conversion",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "everything",
        package: "@modelcontextprotocol/server-everything",
        description: "Reference/test server with all MCP features",
        env_vars: &[],
        extra_args: &[],
    },
    RegistryEntry {
        name: "supabase",
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
        package: "perplexity-mcp",
        description: "Perplexity AI search API",
        env_vars: &[("PERPLEXITY_API_KEY", "Perplexity API key")],
        extra_args: &[],
    },
    RegistryEntry {
        name: "docker",
        package: "@modelcontextprotocol/server-docker",
        description: "Docker container management",
        env_vars: &[],
        extra_args: &[],
    },
];

/// Step A: Look up in the built-in registry
fn lookup(name: &str) -> Option<&'static RegistryEntry> {
    REGISTRY.iter().find(|e| e.name == name)
}

/// Validate an MCP server by spawning it, initializing, and listing tools.
/// Returns tool names on success. Kills the process after validation.
pub async fn validate_server(
    package: &str,
    extra_args: &[String],
    env: &HashMap<String, String>,
) -> Result<Vec<String>> {
    let mut args = vec!["-y".to_string(), package.to_string()];
    args.extend_from_slice(extra_args);

    let transport = tokio::time::timeout(
        Duration::from_secs(60),
        StdioTransport::spawn("npx", &args, env),
    )
    .await
    .map_err(|_| anyhow!("Server startup timed out (60s). Is Node.js/npx installed?"))?
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

/// Step B: Try convention-based package name patterns
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
        if validate_server(pattern, &[], &empty_env).await.is_ok() {
            return Some(pattern.clone());
        }
    }

    None
}

/// Step C: Search npm registry for MCP server packages
async fn search_npm(name: &str) -> Result<Option<(String, String)>> {
    let query = format!("{} mcp server", name);
    let url = format!(
        "https://registry.npmjs.org/-/v1/search?text={}&size=5",
        urlencoded(&query)
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let response = client.get(&url).send().await.map_err(|e| {
        anyhow!("npm registry search failed (network unavailable?): {}", e)
    })?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "npm registry returned HTTP {}",
            response.status()
        ));
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

/// Simple URL encoding for query parameters
fn urlencoded(s: &str) -> String {
    s.replace(' ', "+")
        .replace('@', "%40")
        .replace('/', "%2F")
}

/// Resolve an MCP server name to a validated, ready-to-configure server.
/// Tries: A (built-in) → B (convention) → C (npm search)
pub async fn resolve(name: &str) -> Result<ResolvedServer> {
    // Step A: Built-in registry
    if let Some(entry) = lookup(name) {
        println!("Found: {} ({})", entry.package, entry.description);
        return Ok(ResolvedServer {
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

    // Step B: Convention-based
    if let Some(package) = try_conventions(name).await {
        println!("Found: {}", package);
        return Ok(ResolvedServer {
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
            validate_server(&package, &[], &empty_env).await.map_err(|e| {
                anyhow!(
                    "Found npm package '{}' but it failed validation: {}",
                    package,
                    e
                )
            })?;

            Ok(ResolvedServer {
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

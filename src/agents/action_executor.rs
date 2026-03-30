use futures::future::join_all;
use std::time::Instant;

use super::executor;
use super::filesystem;
use super::types::{ActionResult, AgentAction};
use super::web_search::WebSearchClient;
use crate::ollama::get_cloud_api_key;

/// Get a human-readable description of an action (for UI display)
pub fn describe_action(action: &AgentAction) -> String {
    match action {
        AgentAction::ReadFile { paths } => {
            if paths.len() == 1 {
                format!("Read file: {}", paths[0])
            } else {
                format!("Read {} files", paths.len())
            }
        },
        AgentAction::WriteFile { path, content } => {
            format!("Write file: {} ({} bytes)", path, content.len())
        },
        AgentAction::EditFile { path, .. } => format!("Edit file: {}", path),
        AgentAction::DeleteFile { path } => {
            format!("Delete file: {}", path)
        },
        AgentAction::CreateDirectory { path } => {
            format!("Create directory: {}", path)
        },
        AgentAction::ExecuteCommand {
            command,
            working_dir,
            ..
        } => {
            if let Some(dir) = working_dir {
                format!("Execute command in {}: {}", dir, command)
            } else {
                format!("Execute command: {}", command)
            }
        },
        AgentAction::WebSearch { queries } => {
            if queries.len() == 1 {
                format!("Web search: '{}' ({} results)", queries[0].0, queries[0].1)
            } else {
                format!("Web search with {} queries", queries.len())
            }
        },
        AgentAction::WebFetch { url } => format!("Web fetch: {}", url),
        AgentAction::SpawnAgent { description, .. } => {
            format!("Spawn agent: {}", description)
        },
        AgentAction::ParseError { message } => format!("Parse error: {}", message),
    }
}

/// Execute an agent action
///
/// Returns ActionResult directly - Success or Error variant.
/// Errors are captured in ActionResult::Error, not propagated via Result.
pub async fn execute_action(action: &AgentAction) -> ActionResult {
    match action {
        AgentAction::ReadFile { paths } => execute_read_files(paths).await,
        AgentAction::WriteFile { path, content } => match filesystem::write_file(path, content) {
            Ok(_) => ActionResult::Success {
                output: format!("File written: {}", path),
            },
            Err(e) => ActionResult::Error {
                error: e.to_string(),
            },
        },
        AgentAction::EditFile {
            path,
            old_string,
            new_string,
        } => match filesystem::edit_file(path, old_string, new_string) {
            Ok(diff) => ActionResult::Success { output: diff },
            Err(e) => ActionResult::Error {
                error: e.to_string(),
            },
        },
        AgentAction::DeleteFile { path } => match filesystem::delete_file(path) {
            Ok(_) => ActionResult::Success {
                output: format!("File deleted: {}", path),
            },
            Err(e) => ActionResult::Error {
                error: e.to_string(),
            },
        },
        AgentAction::CreateDirectory { path } => match filesystem::create_directory(path) {
            Ok(_) => ActionResult::Success {
                output: format!("Directory created: {}", path),
            },
            Err(e) => ActionResult::Error {
                error: e.to_string(),
            },
        },
        AgentAction::ExecuteCommand {
            command,
            working_dir,
            timeout,
        } => executor::execute_command(command, working_dir.as_deref(), *timeout).await,
        AgentAction::WebSearch { queries } => execute_web_searches(queries).await,
        AgentAction::WebFetch { url } => execute_web_fetch(url).await,
        AgentAction::SpawnAgent { .. } => ActionResult::Error {
            error: "SpawnAgent must be handled at the agent loop level, not execute_action"
                .to_string(),
        },
        AgentAction::ParseError { message } => ActionResult::Error {
            error: message.clone(),
        },
    }
}

/// Execute file read(s) - parallelizes if multiple paths
async fn execute_read_files(paths: &[String]) -> ActionResult {
    if paths.is_empty() {
        return ActionResult::Error {
            error: "No paths provided for read operation".to_string(),
        };
    }

    // Single file: simple synchronous read
    if paths.len() == 1 {
        return match filesystem::read_file(&paths[0]) {
            Ok(content) => ActionResult::Success { output: content },
            Err(e) => ActionResult::Error {
                error: e.to_string(),
            },
        };
    }

    // Multiple files: parallel execution
    let start = Instant::now();
    let mut results = Vec::new();
    let mut failed_items = Vec::new();

    let futures: Vec<_> = paths
        .iter()
        .map(|path| filesystem::read_file_async(path.clone()))
        .collect();

    let read_results = join_all(futures).await;

    for (result, path) in read_results.into_iter().zip(paths.iter()) {
        match result {
            Ok(content) => results.push((path.clone(), content)),
            Err(_) => failed_items.push(path.clone()),
        }
    }

    // Retry failed files sequentially
    let mut retry_successful = Vec::new();
    for (i, path) in failed_items.iter().enumerate() {
        if let Ok(content) = filesystem::read_file_async(path.clone()).await {
            results.push((path.clone(), content));
            retry_successful.push(i);
        }
    }

    for i in retry_successful.into_iter().rev() {
        failed_items.remove(i);
    }

    let duration = start.elapsed().as_secs_f64();

    if results.is_empty() {
        return ActionResult::Error {
            error: format!(
                "Failed to read all {} files: {}",
                paths.len(),
                failed_items.join(", ")
            ),
        };
    }

    let mut output = format!("Successfully read {} file(s):\n\n", results.len());
    for (path, content) in results {
        output.push_str(&format!("=== {} ===\n{}\n\n", path, content));
    }

    if !failed_items.is_empty() {
        output.push_str(&format!(
            "Failed to read {} file(s): {}\n",
            failed_items.len(),
            failed_items.join(", ")
        ));
    }

    output.push_str(&format!("(Completed in {:.1}s)", duration));
    ActionResult::Success { output }
}

/// Resolve the Ollama Cloud API key, returning an error ActionResult if not configured
fn resolve_api_key() -> Result<String, ActionResult> {
    get_cloud_api_key().ok_or_else(|| ActionResult::Error {
        error: "Web search unavailable: Ollama Cloud API key not configured\n\n\
            Web search requires an Ollama Cloud API key. To set one up:\n\
            1. Run :cloud-setup in Mermaid\n\
            2. Or set the environment variable: export OLLAMA_API_KEY=your_key\n\
            3. Or add to ~/.config/mermaid/config.toml:\n\
               [ollama]\n\
               cloud_api_key = \"your_key\"\n\n\
            Get a free API key at: https://ollama.com/cloud"
            .to_string(),
    })
}

/// Execute web search(es) - parallelizes if multiple queries
async fn execute_web_searches(queries: &[(String, usize)]) -> ActionResult {
    if queries.is_empty() {
        return ActionResult::Error {
            error: "No queries provided for web search".to_string(),
        };
    }

    let api_key = match resolve_api_key() {
        Ok(key) => key,
        Err(err) => return err,
    };

    // Single query: simple execution with detailed error handling
    if queries.len() == 1 {
        let (query, result_count) = &queries[0];
        let client = WebSearchClient::new(api_key);

        return match client.search_query(query, *result_count).await {
            Ok(results) => {
                let formatted = client.format_results(&results);
                ActionResult::Success { output: formatted }
            },
            Err(e) => {
                let error_str = e.to_string();
                let error_msg = if error_str.contains("Result count") {
                    "Invalid search parameters: result count must be between 1 and 10".to_string()
                } else {
                    format!(
                        "Web search error: {}\n\n\
                        This may be a temporary issue. Try again in a moment.",
                        error_str
                    )
                };
                ActionResult::Error { error: error_msg }
            },
        };
    }

    // Multiple queries: parallel execution (shared client for connection pooling)
    let start = Instant::now();
    let mut results = Vec::new();
    let mut failed_items = Vec::new();
    let shared_client = WebSearchClient::new(api_key.clone());

    let futures: Vec<_> = queries
        .iter()
        .map(|(query, count)| {
            let client = shared_client.clone();
            let query_clone = query.clone();
            let count_clone = *count;
            async move {
                (
                    client.search_query(&query_clone, count_clone).await,
                    query_clone,
                )
            }
        })
        .collect();

    let search_results = join_all(futures).await;

    for (search_result, query) in search_results {
        match search_result {
            Ok(search_results) => {
                let formatted = shared_client.format_results(&search_results);
                results.push((query, formatted));
            },
            Err(_) => failed_items.push(query),
        }
    }

    let duration = start.elapsed().as_secs_f64();

    if results.is_empty() {
        return ActionResult::Error {
            error: format!(
                "Failed to complete all {} searches: {}",
                queries.len(),
                failed_items.join(", ")
            ),
        };
    }

    let mut output = format!("Completed {} search(es):\n\n", results.len());
    for (query, formatted_results) in results {
        output.push_str(&format!(
            "=== Search: {} ===\n{}\n\n",
            query, formatted_results
        ));
    }

    if !failed_items.is_empty() {
        output.push_str(&format!(
            "Failed to complete {} search(es): {}\n",
            failed_items.len(),
            failed_items.join(", ")
        ));
    }

    output.push_str(&format!("(Completed in {:.1}s)", duration));
    ActionResult::Success { output }
}

/// Execute a web fetch - fetch a URL's content as markdown
async fn execute_web_fetch(url: &str) -> ActionResult {
    let api_key = match resolve_api_key() {
        Ok(key) => key,
        Err(err) => return err,
    };

    let client = WebSearchClient::new(api_key);
    match client.fetch_url(url).await {
        Ok(result) => {
            let content = crate::utils::truncate_content(
                &result.content,
                crate::constants::WEB_CONTENT_MAX_CHARS,
            );
            let output = format!(
                "Title: {}\nURL: {}\nContent:\n{}",
                result.title, url, content
            );
            ActionResult::Success { output }
        },
        Err(e) => ActionResult::Error {
            error: format!("Failed to fetch {}: {}", url, e),
        },
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_read_file_action() {
        let action = AgentAction::ReadFile {
            paths: vec!["Cargo.toml".to_string()],
        };
        let result = execute_action(&action).await;
        match result {
            ActionResult::Success { output } => {
                assert!(output.contains("[package]") || !output.is_empty());
            },
            ActionResult::Error { .. } => panic!("Should not error on valid file"),
        }
    }

    #[tokio::test]
    async fn test_execute_read_file_not_found() {
        let action = AgentAction::ReadFile {
            paths: vec!["nonexistent_file_xyz.txt".to_string()],
        };
        let result = execute_action(&action).await;
        match result {
            ActionResult::Error { .. } => {}, // Expected
            _ => panic!("Should return error for missing file"),
        }
    }

    #[tokio::test]
    async fn test_execute_write_file_action() {
        let action = AgentAction::WriteFile {
            path: "target/test_output.txt".to_string(),
            content: "test content".to_string(),
        };
        let result = execute_action(&action).await;
        match result {
            ActionResult::Success { output } => {
                assert!(output.contains("File written"));
            },
            ActionResult::Error { error } => {
                assert!(!error.is_empty());
            },
        }
    }

    #[tokio::test]
    async fn test_execute_create_directory_action() {
        let action = AgentAction::CreateDirectory {
            path: "target/test_mermaid_dir".to_string(),
        };
        let result = execute_action(&action).await;
        match result {
            ActionResult::Success { output } => {
                assert!(output.contains("Directory created"));
            },
            ActionResult::Error { error } => {
                assert!(!error.is_empty());
            },
        }
    }


    #[tokio::test]
    async fn test_execute_command_safe_action() {
        let action = AgentAction::ExecuteCommand {
            command: "echo test".to_string(),
            working_dir: None,
            timeout: None,
        };
        let result = execute_action(&action).await;
        assert!(matches!(result, ActionResult::Success { .. }));
    }

    #[tokio::test]
    async fn test_execute_command_with_working_dir() {
        let action = AgentAction::ExecuteCommand {
            command: "pwd".to_string(),
            working_dir: Some("/tmp".to_string()),
            timeout: None,
        };
        let result = execute_action(&action).await;
        assert!(matches!(result, ActionResult::Success { .. }));
    }
}

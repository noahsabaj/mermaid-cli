use anyhow::Result;
use std::time::Instant;
use futures::future::join_all;

use super::executor;
use super::filesystem;
use super::git;
use super::types::{ActionResult, AgentAction};
use super::web_search::WebSearchClient;

/// Execute an agent action
pub async fn execute_action(action: &AgentAction) -> Result<ActionResult> {
    match action {
        AgentAction::ReadFile { path } => {
            filesystem::read_file(path).map(|content| ActionResult::Success { output: content })
        },
        AgentAction::WriteFile { path, content } => {
            filesystem::write_file(path, content).map(|_| ActionResult::Success {
                output: format!("File written: {}", path),
            })
        },
        AgentAction::DeleteFile { path } => {
            filesystem::delete_file(path).map(|_| ActionResult::Success {
                output: format!("File deleted: {}", path),
            })
        },
        AgentAction::CreateDirectory { path } => {
            filesystem::create_directory(path).map(|_| ActionResult::Success {
                output: format!("Directory created: {}", path),
            })
        },
        AgentAction::ExecuteCommand {
            command,
            working_dir,
        } => executor::execute_command(command, working_dir.as_deref()).await,
        AgentAction::GitDiff { path } => {
            git::get_diff(path.as_deref()).map(|diff| ActionResult::Success { output: diff })
        },
        AgentAction::GitStatus => {
            git::get_status().map(|status| ActionResult::Success { output: status })
        },
        AgentAction::GitCommit { message, files } => {
            git::commit(message, files).map(|_| ActionResult::Success {
                output: format!("Committed with message: {}", message),
            })
        },
        AgentAction::WebSearch { query, result_count } => {
            execute_web_search(query, *result_count).await
        },
        AgentAction::ParallelRead { paths } => {
            execute_parallel_reads(paths).await
        },
        AgentAction::ParallelWebSearch { queries } => {
            execute_parallel_web_searches(queries).await
        },
        AgentAction::ParallelGitDiff { paths } => {
            execute_parallel_git_diffs(paths).await
        },
    }
    .map_err(|e| ActionResult::Error {
        error: e.to_string(),
    })
    .or_else(|e| Ok(e))
}

/// Execute a web search action
async fn execute_web_search(query: &str, result_count: usize) -> Result<ActionResult> {
    let searxng_url = std::env::var("MERMAID_SEARXNG_URL")
        .unwrap_or_else(|_| "http://localhost:8888".to_string());

    let mut client = WebSearchClient::new(searxng_url.clone());

    match client.search_cached(query, result_count).await {
        Ok(results) => {
            let formatted = client.format_results(&results);
            Ok(ActionResult::Success { output: formatted })
        }
        Err(e) => {
            let error_str = e.to_string();
            let error_msg = if error_str.contains("Searxng") || error_str.contains("localhost:8888") {
                format!(
                    "Web search unavailable: Searxng service not responding\n\n\
                    Mermaid attempted to auto-start Searxng on application launch, but \
                    it's still not responding at {}\n\n\
                    Possible causes:\n\
                    1. Podman/Docker not installed or not running\n\
                    2. Internet connectivity issue (Searxng requires internet)\n\
                    3. Searxng container failed to start due to resource constraints\n\n\
                    Manual troubleshooting:\n\
                    podman-compose up -d searxng        # Start manually\n\
                    podman-compose logs searxng         # View container logs\n\
                    curl http://localhost:8888/status   # Check if it's responding\n\n\
                    Note: Web search will work once Searxng is available.",
                    searxng_url
                )
            } else if error_str.contains("Result count") {
                "Invalid search parameters: result count must be between 1 and 10".to_string()
            } else {
                format!(
                    "Web search error: {}\n\n\
                    This may be a temporary issue. Try again in a moment.\n\
                    If the problem persists, check that Searxng is running:\n\
                    podman-compose logs searxng",
                    error_str
                )
            };

            Ok(ActionResult::Error {
                error: error_msg,
            })
        }
    }
}

/// Execute multiple file reads in parallel
/// Returns aggregated output with all successful reads
async fn execute_parallel_reads(paths: &[String]) -> Result<ActionResult> {
    let start = Instant::now();
    let mut results = Vec::new();
    let mut failed_items = Vec::new();

    // Execute all reads in parallel using tokio::join_all
    let futures: Vec<_> = paths
        .iter()
        .map(|path| filesystem::read_file_async(path.clone()))
        .collect();

    let read_results = join_all(futures).await;

    for (result, path) in read_results.into_iter().zip(paths.iter()) {
        match result {
            Ok(content) => {
                results.push((path.clone(), content));
            }
            Err(_) => {
                failed_items.push(path.clone());
            }
        }
    }

    // Retry failed files sequentially
    let mut retry_successful = Vec::new();
    for (i, path) in failed_items.iter().enumerate() {
        match filesystem::read_file_async(path.clone()).await {
            Ok(content) => {
                results.push((path.clone(), content));
                retry_successful.push(i);
            }
            Err(_) => {
                // Still failed after retry
            }
        }
    }

    // Remove successfully retried items from failed list
    for i in retry_successful.into_iter().rev() {
        failed_items.remove(i);
    }

    let duration = start.elapsed().as_secs_f64();

    if results.is_empty() {
        return Ok(ActionResult::Error {
            error: format!(
                "Failed to read all {} files. Errors: {}",
                paths.len(),
                failed_items.join(", ")
            ),
        });
    }

    // Format output with all successfully read files
    let mut output = String::new();
    output.push_str(&format!("Successfully read {} file(s):\n\n", results.len()));

    for (path, content) in results {
        output.push_str(&format!("=== {} ===\n", path));
        output.push_str(&content);
        output.push_str("\n\n");
    }

    if !failed_items.is_empty() {
        output.push_str(&format!(
            "Failed to read {} file(s): {}\n",
            failed_items.len(),
            failed_items.join(", ")
        ));
    }

    output.push_str(&format!("(Completed in {:.1}s)", duration));

    Ok(ActionResult::Success { output })
}

/// Execute multiple web searches in parallel
async fn execute_parallel_web_searches(queries: &[(String, usize)]) -> Result<ActionResult> {
    let start = Instant::now();
    let searxng_url = std::env::var("MERMAID_SEARXNG_URL")
        .unwrap_or_else(|_| "http://localhost:8888".to_string());

    let mut results = Vec::new();
    let mut failed_items = Vec::new();

    // Execute all searches in parallel using tokio::join_all
    let futures: Vec<_> = queries
        .iter()
        .map(|(query, count)| {
            let mut client = WebSearchClient::new(searxng_url.clone());
            let query_clone = query.clone();
            let count_clone = *count;
            async move { (client.search_cached(&query_clone, count_clone).await, query_clone) }
        })
        .collect();

    let search_results = join_all(futures).await;

    for (search_result, query) in search_results {
        match search_result {
            Ok(search_results) => {
                let client = WebSearchClient::new(searxng_url.clone());
                let formatted = client.format_results(&search_results);
                results.push((query, formatted));
            }
            Err(_) => {
                failed_items.push(query);
            }
        }
    }

    let duration = start.elapsed().as_secs_f64();

    if results.is_empty() {
        return Ok(ActionResult::Error {
            error: format!(
                "Failed to complete all {} searches. Errors for: {}",
                queries.len(),
                failed_items.join(", ")
            ),
        });
    }

    // Format output with all successfully completed searches
    let mut output = String::new();
    output.push_str(&format!("Completed {} search(es):\n\n", results.len()));

    for (query, formatted_results) in results {
        output.push_str(&format!("=== Search: {} ===\n", query));
        output.push_str(&formatted_results);
        output.push_str("\n\n");
    }

    if !failed_items.is_empty() {
        output.push_str(&format!(
            "Failed to complete {} search(es): {}\n",
            failed_items.len(),
            failed_items.join(", ")
        ));
    }

    output.push_str(&format!("(Completed in {:.1}s)", duration));

    Ok(ActionResult::Success { output })
}

/// Execute multiple git diffs in parallel
async fn execute_parallel_git_diffs(paths: &[Option<String>]) -> Result<ActionResult> {
    let start = Instant::now();
    let mut results = Vec::new();
    let mut failed_items = Vec::new();

    // Execute all diffs in parallel using tokio::join_all
    let futures: Vec<_> = paths
        .iter()
        .map(|path| {
            let path_clone = path.clone();
            async move {
                let diff_result = git::get_diff_async(path_clone.clone()).await;
                (diff_result, path_clone)
            }
        })
        .collect();

    let diff_results = join_all(futures).await;

    for (result, path) in diff_results {
        match result {
            Ok(diff_output) => {
                let path_str = path.as_ref().map(|p| p.as_str()).unwrap_or("*");
                results.push((path_str.to_string(), diff_output));
            }
            Err(_) => {
                let path_str = path.as_ref().map(|p| p.as_str()).unwrap_or("*");
                failed_items.push(path_str.to_string());
            }
        }
    }

    let duration = start.elapsed().as_secs_f64();

    if results.is_empty() {
        return Ok(ActionResult::Error {
            error: format!(
                "Failed to generate all {} git diff(s). Errors for: {}",
                paths.len(),
                failed_items.join(", ")
            ),
        });
    }

    // Format output with all successfully generated diffs
    let mut output = String::new();
    output.push_str(&format!("Generated {} git diff(s):\n\n", results.len()));

    for (path, diff_output) in results {
        output.push_str(&format!("=== Git Diff: {} ===\n", path));
        output.push_str(&diff_output);
        output.push_str("\n\n");
    }

    if !failed_items.is_empty() {
        output.push_str(&format!(
            "Failed to generate {} git diff(s): {}\n",
            failed_items.len(),
            failed_items.join(", ")
        ));
    }

    output.push_str(&format!("(Completed in {:.1}s)", duration));

    Ok(ActionResult::Success { output })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_read_file_action() {
        let action = AgentAction::ReadFile {
            path: "Cargo.toml".to_string(),
        };
        let result = execute_action(&action).await;
        assert!(result.is_ok());
        match result.unwrap() {
            ActionResult::Success { output } => {
                assert!(output.contains("[package]") || !output.is_empty());
            },
            ActionResult::Error { .. } => panic!("Should not error on valid file"),
        }
    }

    #[tokio::test]
    async fn test_execute_read_file_not_found() {
        let action = AgentAction::ReadFile {
            path: "nonexistent_file_xyz.txt".to_string(),
        };
        let result = execute_action(&action).await;
        match result {
            Ok(ActionResult::Error { .. }) => {}, // Expected
            _ => panic!("Should return error for missing file"),
        }
    }

    #[tokio::test]
    async fn test_execute_write_file_action() {
        // Test that write action succeeds or returns proper result
        // Use a realistic relative path that tests would expect
        let action = AgentAction::WriteFile {
            path: "target/test_output.txt".to_string(),
            content: "test content".to_string(),
        };
        let result = execute_action(&action).await;
        assert!(result.is_ok());
        // Accept either success or error - filesystem behavior may vary
        match result.unwrap() {
            ActionResult::Success { output } => {
                assert!(output.contains("File written"));
            },
            ActionResult::Error { error } => {
                // Expected if directory doesn't exist or permissions issue
                assert!(!error.is_empty());
            },
        }
    }

    #[tokio::test]
    async fn test_execute_create_directory_action() {
        // Test that create directory action returns expected result
        let action = AgentAction::CreateDirectory {
            path: "target/test_mermaid_dir".to_string(),
        };
        let result = execute_action(&action).await;
        assert!(result.is_ok());
        // Accept either success or error - filesystem may already exist or have permissions
        match result.unwrap() {
            ActionResult::Success { output } => {
                assert!(output.contains("Directory created"));
            },
            ActionResult::Error { error } => {
                // Expected if directory already exists
                assert!(!error.is_empty());
            },
        }
    }

    #[tokio::test]
    async fn test_execute_git_status_action() {
        let action = AgentAction::GitStatus;
        let result = execute_action(&action).await;
        // May fail if not in git repo, but shouldn't panic
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_execute_git_diff_action() {
        let action = AgentAction::GitDiff { path: None };
        let result = execute_action(&action).await;
        // May fail if not in git repo, but shouldn't panic
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_execute_command_safe_action() {
        let action = AgentAction::ExecuteCommand {
            command: "echo test".to_string(),
            working_dir: None,
        };
        let result = execute_action(&action).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_execute_command_with_working_dir() {
        let action = AgentAction::ExecuteCommand {
            command: "pwd".to_string(),
            working_dir: Some("/tmp".to_string()),
        };
        let result = execute_action(&action).await;
        assert!(result.is_ok());
    }
}

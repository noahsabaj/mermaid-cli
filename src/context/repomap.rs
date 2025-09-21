use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use super::ranker::{RankerConfig, RepoRanker};
use super::repo_graph::RepoGraph;
use super::tree_parser::{Symbol, SymbolReference, TreeParser};

/// Main repository map builder
pub struct RepoMap {
    parser: TreeParser,
    graph: RepoGraph,
    ranker: RepoRanker,
    cache: Arc<Mutex<RepoMapCache>>,
}

/// Cache for parsed files and generated maps
#[derive(Default)]
struct RepoMapCache {
    parsed_files: HashMap<PathBuf, Vec<Symbol>>,
    file_references: HashMap<PathBuf, Vec<SymbolReference>>,
    last_map: Option<String>,
    last_token_budget: usize,
}

impl RepoMap {
    /// Create a new repository map builder
    pub fn new(config: Option<RankerConfig>) -> Result<Self> {
        let config = config.unwrap_or_default();
        let parser = TreeParser::new()?;
        let graph = RepoGraph::new();
        let ranker = RepoRanker::new(config)?;
        let cache = Arc::new(Mutex::new(RepoMapCache::default()));

        Ok(Self {
            parser,
            graph,
            ranker,
            cache,
        })
    }

    /// Build repository map from a directory
    pub async fn build_from_directory(&mut self, root: &Path) -> Result<()> {
        let files = self.scan_directory(root)?;
        self.parse_files(&files).await?;
        self.build_graph().await?;
        Ok(())
    }

    /// Scan directory for source files
    fn scan_directory(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        let supported_extensions = self.parser.supported_extensions();

        // Use ignore crate to respect .gitignore
        let walker = ignore::WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .build();

        for entry in walker {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if supported_extensions.contains(ext) {
                        files.push(path.to_path_buf());
                    }
                }
            }
        }

        Ok(files)
    }

    /// Parse all files and extract symbols
    async fn parse_files(&mut self, files: &[PathBuf]) -> Result<()> {
        let cache_lock = self.cache.lock().await;

        // Filter out already cached files
        let files_to_parse: Vec<PathBuf> = files
            .iter()
            .filter(|f| !cache_lock.parsed_files.contains_key(*f))
            .cloned()
            .collect();

        drop(cache_lock); // Release lock before parallel processing

        // Parse files in parallel
        let parsed_results: Vec<(PathBuf, Option<Vec<Symbol>>, Option<Vec<SymbolReference>>)> =
            files_to_parse
                .par_iter()
                .filter_map(|file| {
                    // Read file content
                    match fs::read_to_string(file) {
                        Ok(content) => {
                            // Create a new parser for this thread
                            match TreeParser::new() {
                                Ok(mut parser) => {
                                    // Parse symbols
                                    let symbols = match parser.parse_file(file, &content) {
                                        Ok(syms) => Some(syms),
                                        Err(e) => {
                                            eprintln!("Failed to parse {}: {}", file.display(), e);
                                            None
                                        }
                                    };

                                    // Extract references
                                    let references = parser.find_references(file, &content).ok();

                                    Some((file.clone(), symbols, references))
                                }
                                Err(e) => {
                                    eprintln!("Failed to create parser for {}: {}", file.display(), e);
                                    None
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to read {}: {}", file.display(), e);
                            None
                        }
                    }
                })
                .collect();

        // Update cache with parsed results
        let mut cache = self.cache.lock().await;
        for (path, symbols, references) in parsed_results {
            if let Some(syms) = symbols {
                cache.parsed_files.insert(path.clone(), syms);
            }
            if let Some(refs) = references {
                cache.file_references.insert(path, refs);
            }
        }

        Ok(())
    }

    /// Build the dependency graph from parsed data
    async fn build_graph(&mut self) -> Result<()> {
        let cache = self.cache.lock().await;

        // Clear existing graph
        self.graph.clear();

        // Add all files and their symbols
        for (path, symbols) in &cache.parsed_files {
            self.graph.add_file(path.clone(), symbols.clone());
        }

        // Add all references
        let mut all_references = Vec::new();
        for refs in cache.file_references.values() {
            all_references.extend(refs.clone());
        }
        self.graph.add_references(all_references);

        Ok(())
    }

    /// Generate repository map with given context
    pub async fn generate_map(
        &mut self,
        chat_files: &[PathBuf],
        mentioned_files: &[PathBuf],
        token_budget: Option<usize>,
    ) -> Result<String> {
        let budget = token_budget.unwrap_or(1024);

        // Check cache
        {
            let cache = self.cache.lock().await;
            if let Some(ref last_map) = cache.last_map {
                if cache.last_token_budget == budget {
                    return Ok(last_map.clone());
                }
            }
        }

        // Run PageRank with personalization on the ranker's graph
        {
            // Transfer current graph data to ranker's graph
            let ranker_graph = self.ranker.graph_mut();
            *ranker_graph = std::mem::replace(&mut self.graph, RepoGraph::new());
        }

        self.ranker.rank_with_context(chat_files, mentioned_files)?;

        {
            // Get the graph back
            let ranker_graph = self.ranker.graph_mut();
            self.graph = std::mem::replace(ranker_graph, RepoGraph::new());
        }

        // Generate optimized map
        let map = self.ranker.generate_map(Some(budget))?;

        // Update cache
        {
            let mut cache = self.cache.lock().await;
            cache.last_map = Some(map.clone());
            cache.last_token_budget = budget;
        }

        Ok(map)
    }

    /// Update map when files change
    pub async fn update_files(&mut self, changed_files: &[PathBuf]) -> Result<()> {
        let mut cache = self.cache.lock().await;

        for file in changed_files {
            // Remove from cache to force reparse
            cache.parsed_files.remove(file);
            cache.file_references.remove(file);
        }

        // Clear last map cache
        cache.last_map = None;

        // Drop lock before rebuilding
        drop(cache);

        // Reparse changed files
        self.parse_files(changed_files).await?;

        // Rebuild graph
        self.build_graph().await?;

        Ok(())
    }

    /// Get statistics about the repository map
    pub fn get_stats(&self) -> RepoMapStats {
        let graph_stats = self.graph.stats();
        let ranking_stats = self.ranker.get_stats();

        RepoMapStats {
            total_files: graph_stats.total_files,
            total_symbols: graph_stats.total_symbols,
            unique_symbols: graph_stats.unique_symbols,
            selected_symbols: ranking_stats.selected_symbols,
            estimated_tokens: ranking_stats.estimated_tokens,
            token_budget: ranking_stats.max_token_budget,
        }
    }

    /// Get the most important files
    pub fn get_top_files(&self, limit: usize) -> Vec<PathBuf> {
        self.graph
            .get_ranked_files()
            .into_iter()
            .take(limit)
            .map(|(path, _score)| path.to_path_buf())
            .collect()
    }

    /// Clear all cached data
    pub async fn clear_cache(&mut self) {
        let mut cache = self.cache.lock().await;
        cache.parsed_files.clear();
        cache.file_references.clear();
        cache.last_map = None;
        self.graph.clear();
    }
}

/// Statistics about the repository map
#[derive(Debug)]
pub struct RepoMapStats {
    pub total_files: usize,
    pub total_symbols: usize,
    pub unique_symbols: usize,
    pub selected_symbols: usize,
    pub estimated_tokens: usize,
    pub token_budget: usize,
}

/// Quick function to generate a repository map
pub async fn generate_repo_map(
    root_path: &Path,
    chat_files: &[PathBuf],
    token_budget: usize,
) -> Result<String> {
    let config = RankerConfig {
        max_tokens: token_budget,
        ..Default::default()
    };

    let mut repomap = RepoMap::new(Some(config))?;
    repomap.build_from_directory(root_path).await?;
    repomap.generate_map(chat_files, &[], Some(token_budget)).await
}